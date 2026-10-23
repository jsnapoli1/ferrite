//! S/MIME (RFC 8551) on CMS (RFC 5652): reading certificates and keys (PEM, DER, PKCS#12),
//! signing (detached, `multipart/signed`), encrypting (`application/pkcs7-mime`
//! enveloped-data), verifying signatures with a certificate chain check, and decrypting.
//!
//! The CMS structures come from the RustCrypto `cms`/`x509-cert`/`der` crates and PKCS#12 from
//! `p12-keystore`; the cryptography is aws-lc (already Ferrite's TLS provider): RSA
//! PKCS#1 v1.5 and PSS signatures, ECDSA (P-256/P-384/P-521), RSA key transport (PKCS#1 v1.5
//! and OAEP) and AES-CBC. Triple-DES (RustCrypto `des`) is only used to read old mail.
//!
//! What is written: SHA-256 signatures (RSA PKCS#1 v1.5 or ECDSA) with signed attributes
//! (content type, signing time, message digest), the signer's certificate and chain included;
//! AES-256-CBC content encryption with RSA PKCS#1 v1.5 key transport (what every S/MIME client
//! reads). Received BER (indefinite lengths, chunked octet strings) is normalized first.
//!
//! Everything here is pure (no I/O), so it is tested against OpenSSL-made fixtures (see
//! `testdata/smime/generate.sh`); `mail::smime_store` keeps certificates and keys per user.

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{self as sig, KeyPair};
use base64::Engine;
use chrono::{DateTime, TimeZone, Utc};
use cms::cert::{CertificateChoices, IssuerAndSerialNumber};
use cms::content_info::{CmsVersion, ContentInfo};
use cms::enveloped_data::{EncryptedContentInfo, EnvelopedData, KeyTransRecipientInfo, RecipientIdentifier, RecipientInfo, RecipientInfos};
use cms::signed_data::{CertificateSet, EncapsulatedContentInfo, SignedData, SignerIdentifier, SignerInfo, SignerInfos};
use der::asn1::{ObjectIdentifier as Oid, OctetString, SetOfVec, UtcTime};
use der::{Any, Decode, Encode};
use serde::Serialize;
use x509_cert::attr::Attribute;
use x509_cert::spki::AlgorithmIdentifierOwned;
use zeroize::Zeroizing;

pub type R<T> = std::result::Result<T, String>;

/// Certificates kept in one chain at most.
const MAX_CHAIN: usize = 8;
/// Upper bound for CMS structures we parse (a message is capped well below).
const MAX_CMS: usize = 64 * 1024 * 1024;

pub mod oid {
    use der::asn1::ObjectIdentifier as Oid;
    pub const DATA: Oid = Oid::new_unwrap("1.2.840.113549.1.7.1");
    pub const SIGNED_DATA: Oid = Oid::new_unwrap("1.2.840.113549.1.7.2");
    pub const ENVELOPED_DATA: Oid = Oid::new_unwrap("1.2.840.113549.1.7.3");
    pub const AUTH_ENVELOPED_DATA: Oid = Oid::new_unwrap("1.2.840.113549.1.9.16.1.23");
    pub const CONTENT_TYPE: Oid = Oid::new_unwrap("1.2.840.113549.1.9.3");
    pub const MESSAGE_DIGEST: Oid = Oid::new_unwrap("1.2.840.113549.1.9.4");
    pub const SIGNING_TIME: Oid = Oid::new_unwrap("1.2.840.113549.1.9.5");
    pub const SMIME_CAPABILITIES: Oid = Oid::new_unwrap("1.2.840.113549.1.9.15");
    pub const RSA: Oid = Oid::new_unwrap("1.2.840.113549.1.1.1");
    pub const RSA_SHA1: Oid = Oid::new_unwrap("1.2.840.113549.1.1.5");
    pub const RSA_OAEP: Oid = Oid::new_unwrap("1.2.840.113549.1.1.7");
    pub const MGF1: Oid = Oid::new_unwrap("1.2.840.113549.1.1.8");
    pub const RSA_PSS: Oid = Oid::new_unwrap("1.2.840.113549.1.1.10");
    pub const RSA_SHA256: Oid = Oid::new_unwrap("1.2.840.113549.1.1.11");
    pub const RSA_SHA384: Oid = Oid::new_unwrap("1.2.840.113549.1.1.12");
    pub const RSA_SHA512: Oid = Oid::new_unwrap("1.2.840.113549.1.1.13");
    pub const EC_PUBLIC_KEY: Oid = Oid::new_unwrap("1.2.840.10045.2.1");
    pub const ECDSA_SHA256: Oid = Oid::new_unwrap("1.2.840.10045.4.3.2");
    pub const ECDSA_SHA384: Oid = Oid::new_unwrap("1.2.840.10045.4.3.3");
    pub const ECDSA_SHA512: Oid = Oid::new_unwrap("1.2.840.10045.4.3.4");
    pub const P256: Oid = Oid::new_unwrap("1.2.840.10045.3.1.7");
    pub const P384: Oid = Oid::new_unwrap("1.3.132.0.34");
    pub const P521: Oid = Oid::new_unwrap("1.3.132.0.35");
    pub const ED25519: Oid = Oid::new_unwrap("1.3.101.112");
    pub const SHA1: Oid = Oid::new_unwrap("1.3.14.3.2.26");
    pub const SHA256: Oid = Oid::new_unwrap("2.16.840.1.101.3.4.2.1");
    pub const SHA384: Oid = Oid::new_unwrap("2.16.840.1.101.3.4.2.2");
    pub const SHA512: Oid = Oid::new_unwrap("2.16.840.1.101.3.4.2.3");
    pub const AES128_CBC: Oid = Oid::new_unwrap("2.16.840.1.101.3.4.1.2");
    pub const AES192_CBC: Oid = Oid::new_unwrap("2.16.840.1.101.3.4.1.22");
    pub const AES256_CBC: Oid = Oid::new_unwrap("2.16.840.1.101.3.4.1.42");
    pub const DES_EDE3_CBC: Oid = Oid::new_unwrap("1.2.840.113549.3.7");
    pub const RC2_CBC: Oid = Oid::new_unwrap("1.2.840.113549.3.2");
}

// ---- BER -------------------------------------------------------------------------------------

struct Tlv {
    start: usize,
    /// The identifier octets.
    tag: Vec<u8>,
    constructed: bool,
    /// Content (definite) or None (indefinite: children up to end-of-contents).
    content: Option<std::ops::Range<usize>>,
    /// Where the element (including an indefinite element's end-of-contents) ends.
    end: usize,
}

fn read_header(b: &[u8], mut p: usize) -> R<(Vec<u8>, bool, Option<usize>, usize)> {
    let start = p;
    let first = *b.get(p).ok_or("truncated DER")?;
    p += 1;
    if first & 0x1f == 0x1f {
        loop {
            let c = *b.get(p).ok_or("truncated tag")?;
            p += 1;
            if p - start > 6 {
                return Err("tag too long".into());
            }
            if c & 0x80 == 0 {
                break;
            }
        }
    }
    let tag = b[start..p].to_vec();
    let constructed = first & 0x20 != 0;
    let l = *b.get(p).ok_or("truncated length")?;
    p += 1;
    let len = if l == 0x80 {
        if !constructed {
            return Err("indefinite length on a primitive".into());
        }
        None
    } else if l & 0x80 == 0 {
        Some(l as usize)
    } else {
        let n = (l & 0x7f) as usize;
        if n == 0 || n > 4 {
            return Err("unsupported length".into());
        }
        let mut v = 0usize;
        for _ in 0..n {
            v = (v << 8) | *b.get(p).ok_or("truncated length")? as usize;
            p += 1;
        }
        Some(v)
    };
    Ok((tag, constructed, len, p))
}

fn read_tlv(b: &[u8], p: usize, depth: usize) -> R<Tlv> {
    if depth > 64 {
        return Err("nesting too deep".into());
    }
    let (tag, constructed, len, body) = read_header(b, p)?;
    match len {
        Some(l) => {
            let end = body.checked_add(l).filter(|e| *e <= b.len()).ok_or("length past the end")?;
            Ok(Tlv {
                start: p,
                tag,
                constructed,
                content: Some(body..end),
                end,
            })
        }
        None => {
            let mut q = body;
            loop {
                if b.get(q..q + 2) == Some(&[0, 0]) {
                    return Ok(Tlv {
                        start: p,
                        tag,
                        constructed,
                        content: None,
                        end: q + 2,
                    });
                }
                if q >= b.len() {
                    return Err("missing end-of-contents".into());
                }
                q = read_tlv(b, q, depth + 1)?.end;
            }
        }
    }
}

fn push_len(out: &mut Vec<u8>, n: usize) {
    if n < 0x80 {
        out.push(n as u8);
    } else {
        let bytes = (n as u64).to_be_bytes();
        let skip = bytes.iter().take_while(|b| **b == 0).count();
        out.push(0x80 | (8 - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
}

fn children(b: &[u8], t: &Tlv, depth: usize) -> R<Vec<Tlv>> {
    let r = t.content.clone().ok_or("indefinite length after normalization")?;
    if !t.constructed {
        return Ok(vec![]);
    }
    let mut q = r.start;
    let mut out = Vec::new();
    while q < r.end {
        let c = read_tlv(b, q, depth + 1)?;
        q = c.end;
        out.push(c);
    }
    Ok(out)
}

/// Normalizes BER into DER where it matters for parsing: indefinite lengths become definite,
/// and constructed (chunked) OCTET STRINGs become one primitive OCTET STRING. Everything else
/// is copied as it is.
pub fn ber_to_der(b: &[u8]) -> R<Vec<u8>> {
    if b.len() > MAX_CMS {
        return Err("the S/MIME structure is too large".into());
    }
    let mut out = Vec::with_capacity(b.len());
    normalize_at(b, 0, &mut out, 0)?;
    Ok(out)
}

fn normalize_at(b: &[u8], p: usize, out: &mut Vec<u8>, depth: usize) -> R<usize> {
    if depth > 64 {
        return Err("nesting too deep".into());
    }
    let (tag, constructed, len, body) = read_header(b, p)?;
    if !constructed {
        let l = len.ok_or("indefinite primitive")?;
        let end = body.checked_add(l).filter(|e| *e <= b.len()).ok_or("length past the end")?;
        out.extend_from_slice(&tag);
        push_len(out, l);
        out.extend_from_slice(&b[body..end]);
        return Ok(end);
    }
    // Children, normalized, into a scratch buffer.
    let mut inner = Vec::new();
    let mut q = body;
    let end = match len {
        Some(l) => {
            let end = body.checked_add(l).filter(|e| *e <= b.len()).ok_or("length past the end")?;
            while q < end {
                q = normalize_at(b, q, &mut inner, depth + 1)?;
            }
            end
        }
        None => loop {
            if b.get(q..q + 2) == Some(&[0, 0]) {
                break q + 2;
            }
            if q >= b.len() {
                return Err("missing end-of-contents".into());
            }
            q = normalize_at(b, q, &mut inner, depth + 1)?;
        },
    };
    if tag == [0x24] {
        // Chunked OCTET STRING: concatenate the (normalized, primitive) chunks.
        let mut data = Vec::new();
        let mut r = 0;
        while r < inner.len() {
            let (t, _, l, s) = read_header(&inner, r)?;
            let l = l.ok_or("bad chunk")?;
            if t != [0x04] {
                return Err("unexpected element in a chunked OCTET STRING".into());
            }
            data.extend_from_slice(&inner[s..s + l]);
            r = s + l;
        }
        out.push(0x04);
        push_len(out, data.len());
        out.extend_from_slice(&data);
    } else {
        out.extend_from_slice(&tag);
        push_len(out, inner.len());
        out.extend_from_slice(&inner);
    }
    Ok(end)
}

/// In EnvelopedData, `encryptedContent [0] IMPLICIT OCTET STRING` may arrive chunked
/// (constructed `[0]` holding OCTET STRINGs); DER wants it primitive. This rewrites that one
/// field (only there: elsewhere the same bytes mean an EXPLICIT tag).
fn fix_encrypted_content(der: &[u8]) -> R<Vec<u8>> {
    let ci = read_tlv(der, 0, 0)?;
    let ci_children = children(der, &ci, 0)?;
    let [_oid, wrapper] = ci_children.as_slice() else { return Ok(der.to_vec()) };
    let ed = children(der, wrapper, 1)?;
    let Some(env) = ed.first() else { return Ok(der.to_vec()) };
    let env_children = children(der, env, 2)?;
    let Some(eci) = env_children.iter().find(|c| c.tag == [0x30] && c.content.as_ref().is_some_and(|r| der.get(r.start) == Some(&0x06))) else {
        return Ok(der.to_vec());
    };
    let eci_children = children(der, eci, 3)?;
    let Some(enc) = eci_children.get(2) else { return Ok(der.to_vec()) };
    if enc.tag != [0xa0] {
        return Ok(der.to_vec());
    }
    let mut data = Vec::new();
    for chunk in children(der, enc, 4)? {
        if chunk.tag != [0x04] {
            return Err("unexpected element in the encrypted content".into());
        }
        data.extend_from_slice(&der[chunk.content.clone().ok_or("bad chunk")?]);
    }
    let mut replacement = vec![0x80];
    push_len(&mut replacement, data.len());
    replacement.extend_from_slice(&data);
    // Rebuild the ancestors (ContentInfo, [0], EnvelopedData, EncryptedContentInfo) around
    // the replaced field, each with its new length.
    let rebuild = |t: &Tlv, before: &[u8], middle: &[u8], after: &[u8]| {
        let mut body = before.to_vec();
        body.extend_from_slice(middle);
        body.extend_from_slice(after);
        let mut v = t.tag.clone();
        push_len(&mut v, body.len());
        v.extend_from_slice(&body);
        v
    };
    let content_start = |t: &Tlv| t.content.as_ref().map(|r| r.start).unwrap_or(t.end);
    let new_eci = rebuild(eci, &der[content_start(eci)..enc.start], &replacement, &der[enc.end..eci.end]);
    let new_env = rebuild(env, &der[content_start(env)..eci.start], &new_eci, &der[eci.end..env.end]);
    let new_wrapper = rebuild(wrapper, &[], &new_env, &[]);
    Ok(rebuild(&ci, &der[content_start(&ci)..wrapper.start], &new_wrapper, &der[wrapper.end..ci.end]))
}

/// Parses a CMS ContentInfo from BER or DER.
pub fn parse_content_info(raw: &[u8]) -> R<ContentInfo> {
    let der = ber_to_der(raw)?;
    let der = fix_encrypted_content(&der).unwrap_or(der);
    ContentInfo::from_der(&der).map_err(|e| format!("not a CMS structure: {e}"))
}

// ---- PEM and certificates --------------------------------------------------------------------

/// `(label, DER)` of every PEM block in `text`.
pub fn pem_blocks(text: &str) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(b) = rest.find("-----BEGIN ") {
        let after = &rest[b + 11..];
        let Some(le) = after.find("-----") else { break };
        let label = after[..le].trim().to_string();
        let body_start = &after[le + 5..];
        let end_marker = format!("-----END {label}-----");
        let Some(e) = body_start.find(&end_marker) else { break };
        let body = &body_start[..e];
        // Headers such as "Proc-Type:" (legacy encrypted PEM) are kept out of the base64.
        let b64: String = body
            .lines()
            .filter(|l| !l.contains(':'))
            .flat_map(|l| l.chars())
            .filter(|c| !c.is_whitespace())
            .collect();
        if body.contains("Proc-Type:") {
            out.push((format!("{label} (legacy encrypted)"), Vec::new()));
        } else if let Ok(der) = base64::engine::general_purpose::STANDARD.decode(b64) {
            out.push((label, der));
        }
        rest = &body_start[e + end_marker.len()..];
    }
    out
}

/// Certificates in `data`: PEM blocks, else one DER certificate.
pub fn read_certificates(data: &[u8]) -> Vec<Vec<u8>> {
    if let Ok(text) = std::str::from_utf8(data) {
        let pem: Vec<Vec<u8>> = pem_blocks(text)
            .into_iter()
            .filter(|(l, _)| l == "CERTIFICATE" || l == "X509 CERTIFICATE" || l == "TRUSTED CERTIFICATE")
            .map(|(_, d)| d)
            .collect();
        if !pem.is_empty() {
            return pem;
        }
    }
    if x509_parser::parse_x509_certificate(data).is_ok() {
        vec![data.to_vec()]
    } else {
        vec![]
    }
}

/// What the UI and the checks need to know about a certificate.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CertInfo {
    pub subject: String,
    /// The subject's common name, when there is one.
    pub name: Option<String>,
    pub issuer: String,
    pub serial: String,
    pub not_before: DateTime<Utc>,
    pub not_after: DateTime<Utc>,
    /// SHA-256 of the DER, hex.
    pub fingerprint: String,
    /// rfc822Name subject alternative names and emailAddress attributes, lower case.
    pub emails: Vec<String>,
    pub is_ca: bool,
    /// rsa, ec or ed25519
    pub key_type: String,
    pub key_bits: Option<usize>,
    /// Usable for S/MIME: no extended key usage, or one that includes email protection.
    pub email_protection: bool,
    pub self_signed: bool,
}

fn ts(t: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(t, 0).single().unwrap_or_default()
}

pub fn cert_info(der: &[u8]) -> R<CertInfo> {
    let (_, c) = x509_parser::parse_x509_certificate(der).map_err(|e| format!("not a certificate: {e}"))?;
    let mut emails: Vec<String> = Vec::new();
    if let Ok(Some(san)) = c.subject_alternative_name() {
        for n in &san.value.general_names {
            if let x509_parser::extensions::GeneralName::RFC822Name(e) = n {
                emails.push(e.trim().to_lowercase());
            }
        }
    }
    for e in c.subject().iter_email() {
        if let Ok(s) = e.as_str() {
            emails.push(s.trim().to_lowercase());
        }
    }
    emails.retain(|e| e.contains('@'));
    emails.sort();
    emails.dedup();
    let name = c.subject().iter_common_name().next().and_then(|cn| cn.as_str().ok()).map(str::to_string);
    let spki = c.public_key();
    let key_oid = spki.algorithm.algorithm.to_id_string();
    let (key_type, key_bits) = match key_oid.as_str() {
        "1.2.840.113549.1.1.1" => (
            "rsa",
            match spki.parsed() {
                Ok(x509_parser::public_key::PublicKey::RSA(k)) => Some(k.key_size()),
                _ => None,
            },
        ),
        "1.2.840.10045.2.1" => ("ec", None),
        "1.3.101.112" => ("ed25519", Some(256)),
        _ => ("other", None),
    };
    let email_protection = match c.extended_key_usage() {
        Ok(Some(eku)) => eku.value.email_protection || eku.value.any,
        _ => true,
    };
    Ok(CertInfo {
        subject: c.subject().to_string(),
        name,
        issuer: c.issuer().to_string(),
        serial: hex::encode(c.raw_serial()),
        not_before: ts(c.validity().not_before.timestamp()),
        not_after: ts(c.validity().not_after.timestamp()),
        fingerprint: crate::util::sha256_hex(der),
        emails,
        is_ca: c.is_ca(),
        key_type: key_type.into(),
        key_bits,
        email_protection,
        self_signed: c.subject().as_raw() == c.issuer().as_raw(),
    })
}

// ---- private keys ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    Rsa,
    P256,
    P384,
    P521,
}

/// A private key as PKCS#8 DER (wiped from memory when dropped).
#[derive(Clone)]
pub struct PrivateKey {
    pub pkcs8: Zeroizing<Vec<u8>>,
    pub kind: KeyKind,
}

impl std::fmt::Debug for PrivateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrivateKey").field("kind", &self.kind).finish_non_exhaustive()
    }
}

fn ecdsa_signing(kind: KeyKind) -> Option<&'static sig::EcdsaSigningAlgorithm> {
    match kind {
        KeyKind::P256 => Some(&sig::ECDSA_P256_SHA256_ASN1_SIGNING),
        KeyKind::P384 => Some(&sig::ECDSA_P384_SHA384_ASN1_SIGNING),
        KeyKind::P521 => Some(&sig::ECDSA_P521_SHA512_ASN1_SIGNING),
        KeyKind::Rsa => None,
    }
}

impl PrivateKey {
    /// From PKCS#8 (`PRIVATE KEY`), PKCS#1 (`RSA PRIVATE KEY`) or SEC 1 (`EC PRIVATE KEY`) DER.
    pub fn from_der(der: &[u8]) -> R<PrivateKey> {
        if sig::RsaKeyPair::from_pkcs8(der).is_ok() {
            return Ok(PrivateKey {
                pkcs8: Zeroizing::new(der.to_vec()),
                kind: KeyKind::Rsa,
            });
        }
        if sig::RsaKeyPair::from_der(der).is_ok() {
            return Ok(PrivateKey {
                pkcs8: Zeroizing::new(rsa_pkcs1_to_pkcs8(der)),
                kind: KeyKind::Rsa,
            });
        }
        for kind in [KeyKind::P256, KeyKind::P384, KeyKind::P521] {
            let alg = ecdsa_signing(kind).expect("EC kinds have an algorithm");
            if let Ok(k) = sig::EcdsaKeyPair::from_private_key_der(alg, der) {
                let pkcs8 = k.to_pkcs8v1().map_err(|_| "the EC key could not be converted".to_string())?;
                return Ok(PrivateKey {
                    pkcs8: Zeroizing::new(pkcs8.as_ref().to_vec()),
                    kind,
                });
            }
        }
        Err("unsupported private key (use RSA or ECDSA P-256/P-384/P-521)".into())
    }

    /// The public key bytes as they appear in a certificate's subjectPublicKey.
    pub fn public_key(&self) -> R<Vec<u8>> {
        match self.kind {
            KeyKind::Rsa => Ok(sig::RsaKeyPair::from_pkcs8(&self.pkcs8)
                .map_err(|e| e.to_string())?
                .public_key()
                .as_ref()
                .to_vec()),
            k => Ok(sig::EcdsaKeyPair::from_pkcs8(ecdsa_signing(k).expect("EC kind"), &self.pkcs8)
                .map_err(|e| e.to_string())?
                .public_key()
                .as_ref()
                .to_vec()),
        }
    }
}

fn der_tlv(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![tag];
    push_len(&mut v, body.len());
    v.extend_from_slice(body);
    v
}

fn rsa_pkcs1_to_pkcs8(pkcs1: &[u8]) -> Vec<u8> {
    // PrivateKeyInfo ::= SEQUENCE { version 0, AlgorithmIdentifier { rsaEncryption, NULL }, OCTET STRING }
    let mut body = vec![0x02, 0x01, 0x00];
    body.extend_from_slice(&[0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00]);
    body.extend_from_slice(&der_tlv(0x04, pkcs1));
    der_tlv(0x30, &body)
}

/// A private key from PEM or DER, decrypting `ENCRYPTED PRIVATE KEY` (PKCS#8 PBES2) with
/// `password`.
pub fn read_private_key(data: &[u8], password: Option<&str>) -> R<PrivateKey> {
    if let Ok(text) = std::str::from_utf8(data) {
        let blocks = pem_blocks(text);
        if let Some((label, _)) = blocks.iter().find(|(l, _)| l.ends_with("(legacy encrypted)")) {
            return Err(format!(
                "this {} uses the legacy OpenSSL encryption; convert it with `openssl pkcs8 -topk8` or import a PKCS#12 file",
                label.trim_end_matches(" (legacy encrypted)")
            ));
        }
        if let Some((_, der)) = blocks.iter().find(|(l, _)| l == "ENCRYPTED PRIVATE KEY") {
            let password = password.filter(|p| !p.is_empty()).ok_or("the private key is encrypted: enter its password")?;
            let info = pkcs8::EncryptedPrivateKeyInfoRef::try_from(der.as_slice()).map_err(|e| format!("encrypted private key: {e}"))?;
            let plain = info.decrypt(password).map_err(|_| "wrong password for the private key".to_string())?;
            return PrivateKey::from_der(plain.as_bytes());
        }
        if let Some((_, der)) = blocks.iter().find(|(l, _)| l.ends_with("PRIVATE KEY")) {
            return PrivateKey::from_der(der);
        }
    }
    PrivateKey::from_der(data)
}

/// Whether `key` belongs to the certificate.
pub fn key_matches(key: &PrivateKey, cert_der: &[u8]) -> bool {
    let Ok((_, c)) = x509_parser::parse_x509_certificate(cert_der) else { return false };
    key.public_key().is_ok_and(|pk| pk.as_slice() == c.public_key().subject_public_key.data.as_ref())
}

/// A certificate with its private key and the intermediates to send along.
#[derive(Debug, Clone)]
pub struct Identity {
    pub cert: Vec<u8>,
    pub chain: Vec<Vec<u8>>,
    pub key: PrivateKey,
}

/// The identity in a PKCS#12 file: its key, the certificate for that key, and the other
/// certificates (as the chain).
pub fn read_pkcs12(data: &[u8], password: &str) -> R<Identity> {
    let archive = p12_keystore::Pkcs12Archive::from_pkcs12(data, password).map_err(|e| {
        let s = e.to_string();
        if s.to_lowercase().contains("mac") || s.to_lowercase().contains("password") || s.to_lowercase().contains("decrypt") {
            "wrong password, or not a PKCS#12 file".to_string()
        } else {
            format!("the PKCS#12 file could not be read: {s}")
        }
    })?;
    let certs: Vec<Vec<u8>> = archive.certs.iter().map(|c| c.cert.as_der().to_vec()).collect();
    for bag in &archive.keys {
        let key = PrivateKey::from_der(bag.key.as_der())?;
        if let Some(cert) = certs.iter().find(|c| key_matches(&key, c)) {
            let chain = certs.iter().filter(|c| *c != cert).cloned().take(MAX_CHAIN).collect();
            return Ok(Identity {
                cert: cert.clone(),
                chain,
                key,
            });
        }
    }
    if archive.keys.is_empty() {
        Err("the PKCS#12 file has no private key".into())
    } else {
        Err("the PKCS#12 file has no certificate for its private key".into())
    }
}

// ---- helpers ---------------------------------------------------------------------------------

fn alg(oid: Oid, params: Option<Any>) -> AlgorithmIdentifierOwned {
    AlgorithmIdentifierOwned { oid, parameters: params }
}

fn issuer_and_serial(cert_der: &[u8]) -> R<IssuerAndSerialNumber> {
    let c = x509_cert::Certificate::from_der(cert_der).map_err(|e| format!("certificate: {e}"))?;
    Ok(IssuerAndSerialNumber {
        issuer: c.tbs_certificate().issuer().clone(),
        serial_number: c.tbs_certificate().serial_number().clone(),
    })
}

fn digest(alg_oid: &Oid, data: &[u8]) -> R<Vec<u8>> {
    let a = if *alg_oid == oid::SHA256 {
        &aws_lc_rs::digest::SHA256
    } else if *alg_oid == oid::SHA384 {
        &aws_lc_rs::digest::SHA384
    } else if *alg_oid == oid::SHA512 {
        &aws_lc_rs::digest::SHA512
    } else if *alg_oid == oid::SHA1 {
        return Err("signed with SHA-1, which is no longer considered secure".into());
    } else {
        return Err(format!("unsupported digest algorithm {alg_oid}"));
    };
    Ok(aws_lc_rs::digest::digest(a, data).as_ref().to_vec())
}

fn attribute(oid: Oid, value: Any) -> R<Attribute> {
    let mut values = SetOfVec::new();
    values.insert(value).map_err(|e| e.to_string())?;
    Ok(Attribute { oid, values })
}

fn any_of<T: Encode>(v: &T) -> R<Any> {
    Any::from_der(&v.to_der().map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

/// RFC 3851/8551 `micalg` name of a digest.
pub fn micalg(alg_oid: &Oid) -> &'static str {
    if *alg_oid == oid::SHA384 {
        "sha-384"
    } else if *alg_oid == oid::SHA512 {
        "sha-512"
    } else {
        "sha-256"
    }
}

// ---- signing ---------------------------------------------------------------------------------

/// The digest used with a key: SHA-256, or the curve's own size for P-384 and P-521 (RFC 5753).
pub fn digest_for(kind: KeyKind) -> Oid {
    match kind {
        KeyKind::Rsa | KeyKind::P256 => oid::SHA256,
        KeyKind::P384 => oid::SHA384,
        KeyKind::P521 => oid::SHA512,
    }
}

/// Signs `content` (the canonical MIME entity) as detached SignedData (DER ContentInfo).
pub fn sign_detached(content: &[u8], id: &Identity, now: DateTime<Utc>) -> R<Vec<u8>> {
    let digest_oid = digest_for(id.key.kind);
    let md = digest(&digest_oid, content)?;
    let signing_time = x509_cert::time::Time::UtcTime(
        UtcTime::from_unix_duration(std::time::Duration::from_secs(now.timestamp().max(0) as u64)).map_err(|e| e.to_string())?,
    );
    // SMIMECapabilities: AES-256-CBC, AES-128-CBC (what we can read).
    let caps = der_tlv(0x30, &[der_tlv(0x30, &oid_tlv(&oid::AES256_CBC)), der_tlv(0x30, &oid_tlv(&oid::AES128_CBC))].concat());
    let mut signed_attrs = SetOfVec::new();
    for a in [
        attribute(oid::CONTENT_TYPE, any_of(&oid::DATA)?)?,
        attribute(oid::SIGNING_TIME, any_of(&signing_time)?)?,
        attribute(oid::MESSAGE_DIGEST, any_of(&OctetString::new(md).map_err(|e| e.to_string())?)?)?,
        attribute(oid::SMIME_CAPABILITIES, Any::from_der(&caps).map_err(|e| e.to_string())?)?,
    ] {
        signed_attrs.insert(a).map_err(|e| e.to_string())?;
    }
    let to_sign = signed_attrs.to_der().map_err(|e| e.to_string())?;
    let rng = SystemRandom::new();
    let (signature, sig_alg) = match id.key.kind {
        KeyKind::Rsa => {
            let kp = sig::RsaKeyPair::from_pkcs8(&id.key.pkcs8).map_err(|e| format!("private key: {e}"))?;
            let mut out = vec![0u8; kp.public_modulus_len()];
            kp.sign(&sig::RSA_PKCS1_SHA256, &rng, &to_sign, &mut out).map_err(|_| "signing failed".to_string())?;
            (out, alg(oid::RSA, Some(Any::null())))
        }
        kind => {
            let curve_alg = match kind {
                KeyKind::P256 => &sig::ECDSA_P256_SHA256_ASN1_SIGNING,
                KeyKind::P384 => &sig::ECDSA_P384_SHA384_ASN1_SIGNING,
                _ => &sig::ECDSA_P521_SHA512_ASN1_SIGNING,
            };
            let kp = sig::EcdsaKeyPair::from_pkcs8(curve_alg, &id.key.pkcs8).map_err(|e| format!("private key: {e}"))?;
            let s = kp.sign(&rng, &to_sign).map_err(|_| "signing failed".to_string())?;
            let sig_oid = match kind {
                KeyKind::P256 => oid::ECDSA_SHA256,
                KeyKind::P384 => oid::ECDSA_SHA384,
                _ => oid::ECDSA_SHA512,
            };
            (s.as_ref().to_vec(), alg(sig_oid, None))
        }
    };
    let mut certs = CertificateSet(SetOfVec::new());
    for der in std::iter::once(&id.cert).chain(id.chain.iter()) {
        let c = x509_cert::Certificate::from_der(der).map_err(|e| format!("certificate: {e}"))?;
        certs.0.insert(CertificateChoices::Certificate(c)).map_err(|e| e.to_string())?;
    }
    let mut digest_algorithms = SetOfVec::new();
    digest_algorithms.insert(alg(digest_oid, None)).map_err(|e| e.to_string())?;
    let mut infos = SetOfVec::new();
    infos
        .insert(SignerInfo {
            version: CmsVersion::V1,
            sid: SignerIdentifier::IssuerAndSerialNumber(issuer_and_serial(&id.cert)?),
            digest_alg: alg(digest_oid, None),
            signed_attrs: Some(signed_attrs),
            signature_algorithm: sig_alg,
            signature: OctetString::new(signature).map_err(|e| e.to_string())?,
            unsigned_attrs: None,
        })
        .map_err(|e| e.to_string())?;
    let sd = SignedData {
        version: CmsVersion::V1,
        digest_algorithms,
        encap_content_info: EncapsulatedContentInfo {
            econtent_type: oid::DATA,
            econtent: None,
        },
        certificates: Some(certs),
        crls: None,
        signer_infos: SignerInfos(infos),
    };
    let ci = ContentInfo {
        content_type: oid::SIGNED_DATA,
        content: any_of(&sd)?,
    };
    ci.to_der().map_err(|e| e.to_string())
}

fn oid_tlv(o: &Oid) -> Vec<u8> {
    o.to_der().expect("OIDs encode")
}

// ---- encryption ------------------------------------------------------------------------------

/// Encrypts `content` (a MIME entity) to every certificate in `recipients`: AES-256-CBC with
/// RSA PKCS#1 v1.5 key transport. Returns the DER ContentInfo.
pub fn encrypt(content: &[u8], recipients: &[Vec<u8>]) -> R<Vec<u8>> {
    use aws_lc_rs::cipher::{AES_256, PaddedBlockEncryptingKey, UnboundCipherKey};
    if recipients.is_empty() {
        return Err("no recipient certificates".into());
    }
    let mut cek = Zeroizing::new([0u8; 32]);
    aws_lc_rs::rand::fill(&mut cek[..]).map_err(|_| "no randomness".to_string())?;
    let key = PaddedBlockEncryptingKey::cbc_pkcs7(UnboundCipherKey::new(&AES_256, &cek[..]).map_err(|_| "cipher".to_string())?)
        .map_err(|_| "cipher".to_string())?;
    let mut data = content.to_vec();
    let ctx = key.encrypt(&mut data).map_err(|_| "encryption failed".to_string())?;
    let iv: Vec<u8> = match ctx {
        aws_lc_rs::cipher::DecryptionContext::Iv128(iv) => iv.as_ref().to_vec(),
        _ => return Err("unexpected cipher context".into()),
    };
    let mut infos = SetOfVec::new();
    for r in recipients {
        let (_, c) = x509_parser::parse_x509_certificate(r).map_err(|e| format!("recipient certificate: {e}"))?;
        if c.public_key().algorithm.algorithm.to_id_string() != "1.2.840.113549.1.1.1" {
            return Err(format!("{}: only RSA certificates can be encrypted to", c.subject()));
        }
        let spki = c.public_key().raw;
        let pk = aws_lc_rs::rsa::PublicEncryptingKey::from_der(spki).map_err(|e| format!("recipient key: {e}"))?;
        let enc = aws_lc_rs::rsa::Pkcs1PublicEncryptingKey::new(pk).map_err(|_| "recipient key".to_string())?;
        let mut out = vec![0u8; enc.ciphertext_size()];
        let ek = enc.encrypt(&cek[..], &mut out).map_err(|_| "key transport failed".to_string())?;
        infos
            .insert(RecipientInfo::Ktri(KeyTransRecipientInfo {
                version: CmsVersion::V0,
                rid: RecipientIdentifier::IssuerAndSerialNumber(issuer_and_serial(r)?),
                key_enc_alg: alg(oid::RSA, Some(Any::null())),
                enc_key: OctetString::new(ek.to_vec()).map_err(|e| e.to_string())?,
            }))
            .map_err(|e| e.to_string())?;
    }
    let ed = EnvelopedData {
        version: CmsVersion::V0,
        originator_info: None,
        recip_infos: RecipientInfos(infos),
        encrypted_content: EncryptedContentInfo {
            content_type: oid::DATA,
            content_enc_alg: alg(oid::AES256_CBC, Some(any_of(&OctetString::new(iv).map_err(|e| e.to_string())?)?)),
            encrypted_content: Some(OctetString::new(data).map_err(|e| e.to_string())?),
        },
        unprotected_attrs: None,
    };
    ContentInfo {
        content_type: oid::ENVELOPED_DATA,
        content: any_of(&ed)?,
    }
    .to_der()
    .map_err(|e| e.to_string())
}

/// The recipients of an encrypted structure, as (issuer DER, serial) or subject key id, for
/// matching against the user's certificates.
fn recipient_matches(rid: &RecipientIdentifier, cert_der: &[u8]) -> bool {
    match rid {
        RecipientIdentifier::IssuerAndSerialNumber(isn) => issuer_and_serial(cert_der).is_ok_and(|mine| mine == *isn),
        RecipientIdentifier::SubjectKeyIdentifier(ski) => x509_cert::Certificate::from_der(cert_der).is_ok_and(|c| {
            c.tbs_certificate()
                .get_extension::<x509_cert::ext::pkix::SubjectKeyIdentifier>()
                .ok()
                .flatten()
                .is_some_and(|(_, mine)| mine == *ski)
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecryptError {
    /// None of the recipients is one of the given certificates.
    NotForYou { recipients: usize },
    /// Ours, but it does not decrypt (corrupt, or the wrong key).
    Failed(String),
    Unsupported(String),
    Malformed(String),
}

impl std::fmt::Display for DecryptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecryptError::NotForYou { .. } => f.write_str("this message was not encrypted for any of your certificates"),
            DecryptError::Failed(m) => write!(f, "the message could not be decrypted: {m}"),
            DecryptError::Unsupported(m) => write!(f, "unsupported encryption: {m}"),
            DecryptError::Malformed(m) => write!(f, "the encrypted message is malformed: {m}"),
        }
    }
}

fn oaep_alg(params: Option<&Any>) -> std::result::Result<&'static aws_lc_rs::rsa::OaepAlgorithm, DecryptError> {
    use aws_lc_rs::rsa::{OAEP_SHA1_MGF1SHA1, OAEP_SHA256_MGF1SHA256, OAEP_SHA384_MGF1SHA384, OAEP_SHA512_MGF1SHA512};
    // RSAES-OAEP-params ::= SEQUENCE { hashFunc [0] DEFAULT sha1, maskGenFunc [1] DEFAULT mgf1SHA1, pSourceFunc [2] DEFAULT empty }
    let Some(p) = params.filter(|p| !p.is_null()) else { return Ok(&OAEP_SHA1_MGF1SHA1) };
    let der = p.to_der().map_err(|e| DecryptError::Malformed(e.to_string()))?;
    let top = read_tlv(&der, 0, 0).map_err(DecryptError::Malformed)?;
    let mut hash = oid::SHA1;
    let mut mgf_hash = oid::SHA1;
    for c in children(&der, &top, 0).map_err(DecryptError::Malformed)? {
        let inner = c.content.clone().ok_or_else(|| DecryptError::Malformed("OAEP parameters".into()))?;
        let alg_id = AlgorithmIdentifierOwned::from_der(&der[inner]).map_err(|e| DecryptError::Malformed(e.to_string()))?;
        match c.tag.as_slice() {
            [0xa0] => hash = alg_id.oid,
            [0xa1] => {
                if alg_id.oid != oid::MGF1 {
                    return Err(DecryptError::Unsupported("OAEP mask generation".into()));
                }
                let h = alg_id
                    .parameters
                    .as_ref()
                    .and_then(|a| a.decode_as::<AlgorithmIdentifierOwned>().ok())
                    .ok_or_else(|| DecryptError::Malformed("MGF1 parameters".into()))?;
                mgf_hash = h.oid;
            }
            [0xa2] => return Err(DecryptError::Unsupported("OAEP labels".into())),
            _ => {}
        }
    }
    if hash != mgf_hash {
        return Err(DecryptError::Unsupported("OAEP with different hash and MGF1 hash".into()));
    }
    Ok(if hash == oid::SHA1 {
        &OAEP_SHA1_MGF1SHA1
    } else if hash == oid::SHA256 {
        &OAEP_SHA256_MGF1SHA256
    } else if hash == oid::SHA384 {
        &OAEP_SHA384_MGF1SHA384
    } else if hash == oid::SHA512 {
        &OAEP_SHA512_MGF1SHA512
    } else {
        return Err(DecryptError::Unsupported(format!("OAEP hash {hash}")));
    })
}

/// Content-encryption key length of a supported cipher.
fn cek_len(o: &Oid) -> Option<usize> {
    if *o == oid::AES128_CBC {
        Some(16)
    } else if *o == oid::AES192_CBC {
        Some(24)
    } else if *o == oid::AES256_CBC {
        Some(32)
    } else if *o == oid::DES_EDE3_CBC {
        Some(24)
    } else {
        None
    }
}

/// Decrypts EnvelopedData with one of `ids` (certificate DER, private key). Returns the
/// content (a MIME entity).
pub fn decrypt(raw: &[u8], ids: &[(Vec<u8>, PrivateKey)]) -> std::result::Result<Vec<u8>, DecryptError> {
    let ci = parse_content_info(raw).map_err(DecryptError::Malformed)?;
    if ci.content_type == oid::AUTH_ENVELOPED_DATA {
        return Err(DecryptError::Unsupported("AES-GCM (authenticated enveloped data)".into()));
    }
    if ci.content_type != oid::ENVELOPED_DATA {
        return Err(DecryptError::Malformed("not enveloped data".into()));
    }
    let ed = ci.content.decode_as::<EnvelopedData>().map_err(|e| DecryptError::Malformed(e.to_string()))?;
    let eci = &ed.encrypted_content;
    let calg = &eci.content_enc_alg;
    let Some(klen) = cek_len(&calg.oid) else {
        return Err(DecryptError::Unsupported(if calg.oid == oid::RC2_CBC {
            "RC2 (too weak to be supported)".into()
        } else {
            format!("cipher {}", calg.oid)
        }));
    };
    let iv: OctetString = calg
        .parameters
        .as_ref()
        .and_then(|p| p.decode_as().ok())
        .ok_or_else(|| DecryptError::Malformed("missing IV".into()))?;
    let data = eci
        .encrypted_content
        .as_ref()
        .ok_or_else(|| DecryptError::Malformed("no encrypted content (detached content is not supported)".into()))?;
    let mut ours = 0usize;
    for ri in ed.recip_infos.0.iter() {
        let RecipientInfo::Ktri(k) = ri else { continue };
        for (cert, key) in ids {
            if key.kind != KeyKind::Rsa || !recipient_matches(&k.rid, cert) {
                continue;
            }
            ours += 1;
            let pk = aws_lc_rs::rsa::PrivateDecryptingKey::from_pkcs8(&key.pkcs8).map_err(|e| DecryptError::Failed(e.to_string()))?;
            let mut buf = Zeroizing::new(vec![0u8; pk.key_size_bytes()]);
            let unwrapped: Option<Vec<u8>> = if k.key_enc_alg.oid == oid::RSA {
                let d = aws_lc_rs::rsa::Pkcs1PrivateDecryptingKey::new(pk).map_err(|_| DecryptError::Failed("key".into()))?;
                d.decrypt(k.enc_key.as_bytes(), &mut buf).ok().map(|s| s.to_vec())
            } else if k.key_enc_alg.oid == oid::RSA_OAEP {
                let a = oaep_alg(k.key_enc_alg.parameters.as_ref())?;
                let d = aws_lc_rs::rsa::OaepPrivateDecryptingKey::new(pk).map_err(|_| DecryptError::Failed("key".into()))?;
                d.decrypt(a, k.enc_key.as_bytes(), &mut buf, None).ok().map(|s| s.to_vec())
            } else {
                return Err(DecryptError::Unsupported(format!("key transport {}", k.key_enc_alg.oid)));
            };
            // RFC 3218: a bad key transport must look like any other failure, so a random key
            // stands in and the content decryption fails the same way.
            let cek = Zeroizing::new(match unwrapped.filter(|c| c.len() == klen) {
                Some(c) => c,
                None => {
                    let mut r = vec![0u8; klen];
                    aws_lc_rs::rand::fill(&mut r).map_err(|_| DecryptError::Failed("randomness".into()))?;
                    r
                }
            });
            match decrypt_content(&calg.oid, &cek, iv.as_bytes(), data.as_bytes()) {
                Ok(plain) => return Ok(plain),
                Err(_) => continue,
            }
        }
    }
    if ours == 0 {
        Err(DecryptError::NotForYou {
            recipients: ed.recip_infos.0.len(),
        })
    } else {
        Err(DecryptError::Failed("the key or the content is corrupt".into()))
    }
}

fn decrypt_content(alg_oid: &Oid, key: &[u8], iv: &[u8], data: &[u8]) -> R<Vec<u8>> {
    use aws_lc_rs::cipher::{AES_128, AES_192, AES_256, DecryptionContext, PaddedBlockDecryptingKey, UnboundCipherKey};
    if *alg_oid == oid::DES_EDE3_CBC {
        use cbc::cipher::{BlockModeDecrypt, KeyIvInit, block_padding::Pkcs7};
        let d = cbc::Decryptor::<des::TdesEde3>::new_from_slices(key, iv).map_err(|_| "3DES key or IV".to_string())?;
        return d.decrypt_padded_vec::<Pkcs7>(data).map_err(|_| "padding".to_string());
    }
    let alg = if *alg_oid == oid::AES128_CBC {
        &AES_128
    } else if *alg_oid == oid::AES192_CBC {
        &AES_192
    } else {
        &AES_256
    };
    let iv: [u8; 16] = iv.try_into().map_err(|_| "IV length".to_string())?;
    let k = PaddedBlockDecryptingKey::cbc_pkcs7(UnboundCipherKey::new(alg, key).map_err(|_| "key".to_string())?).map_err(|_| "key".to_string())?;
    let mut buf = data.to_vec();
    let plain = k.decrypt(&mut buf, DecryptionContext::Iv128(iv.into())).map_err(|_| "padding".to_string())?;
    Ok(plain.to_vec())
}

/// Number of recipients and whether the structure is enveloped data at all.
pub fn is_enveloped(raw: &[u8]) -> bool {
    parse_content_info(raw).is_ok_and(|ci| ci.content_type == oid::ENVELOPED_DATA || ci.content_type == oid::AUTH_ENVELOPED_DATA)
}

// ---- verification ----------------------------------------------------------------------------

/// The result of checking a SignedData's signature (not yet its chain).
#[derive(Debug, Clone, Default)]
pub struct Verification {
    /// The signature over the content is correct.
    pub valid: bool,
    pub signer: Option<Vec<u8>>,
    /// Every certificate carried in the structure.
    pub certificates: Vec<Vec<u8>>,
    pub signing_time: Option<DateTime<Utc>>,
    pub digest: Option<Oid>,
    /// Why it is not valid.
    pub problem: Option<String>,
    /// The encapsulated content (opaque signatures).
    pub content: Option<Vec<u8>>,
}

fn verify_raw(cert_der: &[u8], sig_alg: &AlgorithmIdentifierOwned, digest_alg: &Oid, message: &[u8], signature: &[u8]) -> R<()> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert_der).map_err(|e| format!("the signer's certificate is malformed: {e}"))?;
    let spki = cert.public_key();
    let key = spki.subject_public_key.data.as_ref();
    let key_oid = spki.algorithm.algorithm.to_id_string();
    let s = &sig_alg.oid;
    let d = digest_alg;
    let algorithm: &'static dyn sig::VerificationAlgorithm = if *s == oid::RSA || *s == oid::RSA_SHA256 || *s == oid::RSA_SHA384 || *s == oid::RSA_SHA512 {
        if key_oid != "1.2.840.113549.1.1.1" {
            return Err("RSA signature with a non-RSA key".into());
        }
        let h = if *s == oid::RSA { *d } else if *s == oid::RSA_SHA256 { oid::SHA256 } else if *s == oid::RSA_SHA384 { oid::SHA384 } else { oid::SHA512 };
        if h == oid::SHA256 {
            &sig::RSA_PKCS1_2048_8192_SHA256
        } else if h == oid::SHA384 {
            &sig::RSA_PKCS1_2048_8192_SHA384
        } else if h == oid::SHA512 {
            &sig::RSA_PKCS1_2048_8192_SHA512
        } else {
            return Err("signed with SHA-1, which is no longer considered secure".into());
        }
    } else if *s == oid::RSA_SHA1 {
        return Err("signed with SHA-1, which is no longer considered secure".into());
    } else if *s == oid::RSA_PSS {
        let params = sig_alg.parameters.as_ref().ok_or("RSA-PSS without parameters")?;
        let pder = params.to_der().map_err(|e| e.to_string())?;
        let top = read_tlv(&pder, 0, 0)?;
        let mut h = oid::SHA1;
        for c in children(&pder, &top, 0)? {
            if c.tag == [0xa0] {
                let r = c.content.clone().ok_or("PSS parameters")?;
                h = AlgorithmIdentifierOwned::from_der(&pder[r]).map_err(|e| e.to_string())?.oid;
            }
        }
        if h == oid::SHA256 {
            &sig::RSA_PSS_2048_8192_SHA256
        } else if h == oid::SHA384 {
            &sig::RSA_PSS_2048_8192_SHA384
        } else if h == oid::SHA512 {
            &sig::RSA_PSS_2048_8192_SHA512
        } else {
            return Err("RSA-PSS with an unsupported hash".into());
        }
    } else if *s == oid::ECDSA_SHA256 || *s == oid::ECDSA_SHA384 || *s == oid::ECDSA_SHA512 || (*s == oid::EC_PUBLIC_KEY) {
        let curve = spki
            .algorithm
            .parameters
            .as_ref()
            .and_then(|p| p.as_oid().ok())
            .map(|o| o.to_id_string())
            .unwrap_or_default();
        let h = if *s == oid::ECDSA_SHA256 {
            oid::SHA256
        } else if *s == oid::ECDSA_SHA384 {
            oid::SHA384
        } else if *s == oid::ECDSA_SHA512 {
            oid::SHA512
        } else {
            *d
        };
        match (curve.as_str(), h) {
            ("1.2.840.10045.3.1.7", x) if x == oid::SHA256 => &sig::ECDSA_P256_SHA256_ASN1,
            ("1.2.840.10045.3.1.7", x) if x == oid::SHA384 => &sig::ECDSA_P256_SHA384_ASN1,
            ("1.2.840.10045.3.1.7", x) if x == oid::SHA512 => &sig::ECDSA_P256_SHA512_ASN1,
            ("1.3.132.0.34", x) if x == oid::SHA256 => &sig::ECDSA_P384_SHA256_ASN1,
            ("1.3.132.0.34", x) if x == oid::SHA384 => &sig::ECDSA_P384_SHA384_ASN1,
            ("1.3.132.0.34", x) if x == oid::SHA512 => &sig::ECDSA_P384_SHA512_ASN1,
            ("1.3.132.0.35", x) if x == oid::SHA256 => &sig::ECDSA_P521_SHA256_ASN1,
            ("1.3.132.0.35", x) if x == oid::SHA384 => &sig::ECDSA_P521_SHA384_ASN1,
            ("1.3.132.0.35", x) if x == oid::SHA512 => &sig::ECDSA_P521_SHA512_ASN1,
            _ => return Err("unsupported ECDSA curve or hash".into()),
        }
    } else if *s == oid::ED25519 {
        &sig::ED25519
    } else {
        return Err(format!("unsupported signature algorithm {s}"));
    };
    sig::UnparsedPublicKey::new(algorithm, key)
        .verify(message, signature)
        .map_err(|_| "the signature does not match the content".to_string())
}

fn signer_cert<'a>(sid: &SignerIdentifier, certs: &'a [Vec<u8>]) -> Option<&'a Vec<u8>> {
    certs.iter().find(|c| match sid {
        SignerIdentifier::IssuerAndSerialNumber(isn) => issuer_and_serial(c).is_ok_and(|m| m == *isn),
        SignerIdentifier::SubjectKeyIdentifier(ski) => x509_cert::Certificate::from_der(c).is_ok_and(|x| {
            x.tbs_certificate()
                .get_extension::<x509_cert::ext::pkix::SubjectKeyIdentifier>()
                .ok()
                .flatten()
                .is_some_and(|(_, m)| m == *ski)
        }),
    })
}

/// Verifies a SignedData (DER or BER ContentInfo). `detached` is the signed content of a
/// `multipart/signed` message (its first part, canonical CRLF); opaque signatures carry it.
/// `extra_certs` are certificates to look the signer up in when the message does not carry it.
pub fn verify(raw: &[u8], detached: Option<&[u8]>, extra_certs: &[Vec<u8>]) -> Verification {
    let mut v = Verification::default();
    let ci = match parse_content_info(raw) {
        Ok(ci) => ci,
        Err(e) => {
            v.problem = Some(e);
            return v;
        }
    };
    if ci.content_type != oid::SIGNED_DATA {
        v.problem = Some("not signed data".into());
        return v;
    }
    let sd: SignedData = match ci.content.decode_as() {
        Ok(sd) => sd,
        Err(e) => {
            v.problem = Some(format!("the signature is malformed: {e}"));
            return v;
        }
    };
    if let Some(certs) = &sd.certificates {
        for c in certs.0.iter() {
            if let CertificateChoices::Certificate(c) = c {
                if let Ok(d) = c.to_der() {
                    v.certificates.push(d);
                }
            }
        }
    }
    let encapsulated: Option<Vec<u8>> = sd
        .encap_content_info
        .econtent
        .as_ref()
        .and_then(|a| a.decode_as::<OctetString>().ok().map(|o| o.as_bytes().to_vec()));
    v.content = encapsulated.clone();
    let content: Vec<u8> = match (detached, &encapsulated) {
        (Some(d), _) => d.to_vec(),
        (None, Some(e)) => e.clone(),
        (None, None) => {
            v.problem = Some("the signed content is missing".into());
            return v;
        }
    };
    let Some(si) = sd.signer_infos.0.iter().next() else {
        v.problem = Some("there is no signer".into());
        return v;
    };
    let pool: Vec<Vec<u8>> = v.certificates.iter().chain(extra_certs.iter()).cloned().collect();
    let Some(cert) = signer_cert(&si.sid, &pool).cloned() else {
        v.problem = Some("the signer's certificate is not included".into());
        return v;
    };
    v.signer = Some(cert.clone());
    v.digest = Some(si.digest_alg.oid);
    let md = match digest(&si.digest_alg.oid, &content) {
        Ok(m) => m,
        Err(e) => {
            v.problem = Some(e);
            return v;
        }
    };
    let message: Vec<u8> = match &si.signed_attrs {
        Some(attrs) => {
            let mut ok_digest = false;
            let mut ok_type = false;
            for a in attrs.iter() {
                if a.oid == oid::MESSAGE_DIGEST {
                    ok_digest = a.values.len() == 1 && a.values.iter().next().and_then(|x| x.decode_as::<OctetString>().ok()).is_some_and(|o| o.as_bytes() == md.as_slice());
                } else if a.oid == oid::CONTENT_TYPE {
                    ok_type = a.values.iter().next().and_then(|x| x.decode_as::<Oid>().ok()) == Some(sd.encap_content_info.econtent_type);
                } else if a.oid == oid::SIGNING_TIME {
                    if let Some(t) = a.values.iter().next().and_then(|x| x.decode_as::<x509_cert::time::Time>().ok()) {
                        v.signing_time = Some(ts(t.to_unix_duration().as_secs() as i64));
                    }
                }
            }
            if !ok_digest {
                v.problem = Some("the content was changed after it was signed".into());
                return v;
            }
            if !ok_type {
                v.problem = Some("the signed content type does not match".into());
                return v;
            }
            match attrs.to_der() {
                Ok(d) => d,
                Err(e) => {
                    v.problem = Some(e.to_string());
                    return v;
                }
            }
        }
        None => content.clone(),
    };
    match verify_raw(&cert, &si.signature_algorithm, &si.digest_alg.oid, &message, si.signature.as_bytes()) {
        Ok(()) => v.valid = true,
        Err(e) => v.problem = Some(e),
    }
    v
}

/// The chain verdict for a signer certificate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChainCheck {
    /// trusted | untrusted | expired | not_yet_valid | invalid
    pub status: String,
    pub detail: Option<String>,
    /// Subjects from the signer up to the trust anchor.
    pub path: Vec<String>,
}

fn valid_at(c: &x509_parser::certificate::X509Certificate, at: i64) -> Option<&'static str> {
    if c.validity().not_after.timestamp() < at {
        Some("expired")
    } else if c.validity().not_before.timestamp() > at {
        Some("not_yet_valid")
    } else {
        None
    }
}

/// Builds and checks the chain from `leaf` to one of `anchors` through `pool`
/// (intermediates), at `now`: every signature, validity period and CA flag, plus the leaf's
/// email-protection usage.
pub fn check_chain(leaf: &[u8], pool: &[Vec<u8>], anchors: &[Vec<u8>], now: DateTime<Utc>) -> ChainCheck {
    let fail = |status: &str, detail: String, path: Vec<String>| ChainCheck {
        status: status.into(),
        detail: Some(detail),
        path,
    };
    let at = now.timestamp();
    let Ok((_, leaf_c)) = x509_parser::parse_x509_certificate(leaf) else {
        return fail("invalid", "the certificate is malformed".into(), vec![]);
    };
    let mut path = vec![leaf_c.subject().to_string()];
    if let Some(s) = valid_at(&leaf_c, at) {
        return fail(s, format!("the certificate is {}", if s == "expired" { "expired" } else { "not valid yet" }), path);
    }
    if let Ok(Some(eku)) = leaf_c.extended_key_usage() {
        if !(eku.value.email_protection || eku.value.any) {
            return fail("invalid", "the certificate is not meant for email".into(), path);
        }
    }
    let parsed_anchors: Vec<x509_parser::certificate::X509Certificate> = anchors.iter().filter_map(|a| x509_parser::parse_x509_certificate(a).ok().map(|(_, c)| c)).collect();
    let parsed_pool: Vec<x509_parser::certificate::X509Certificate> = pool.iter().filter_map(|a| x509_parser::parse_x509_certificate(a).ok().map(|(_, c)| c)).collect();
    // The leaf itself being an anchor (a certificate the user marked as trusted).
    if parsed_anchors.iter().any(|a| a.as_raw() == leaf_c.as_raw()) {
        return ChainCheck {
            status: "trusted".into(),
            detail: None,
            path,
        };
    }
    let mut current = leaf_c;
    for _ in 0..MAX_CHAIN {
        let issuer_name = current.issuer().as_raw();
        // An anchor that issued it?
        for a in &parsed_anchors {
            if a.subject().as_raw() == issuer_name && current.verify_signature(Some(a.public_key())).is_ok() {
                if let Some(s) = valid_at(a, at) {
                    return fail(s, format!("the issuer {} is {}", a.subject(), s.replace('_', " ")), path);
                }
                path.push(a.subject().to_string());
                return ChainCheck {
                    status: "trusted".into(),
                    detail: None,
                    path,
                };
            }
        }
        let next = parsed_pool
            .iter()
            .find(|p| p.subject().as_raw() == issuer_name && p.as_raw() != current.as_raw() && current.verify_signature(Some(p.public_key())).is_ok());
        let Some(next) = next else {
            let self_signed = current.subject().as_raw() == current.issuer().as_raw();
            return fail(
                "untrusted",
                if self_signed {
                    "the certificate is self-signed and not in your trusted certificates".into()
                } else {
                    format!("the issuer {} is not trusted", current.issuer())
                },
                path,
            );
        };
        if !next.is_ca() {
            return fail("invalid", format!("{} is not a certificate authority", next.subject()), path);
        }
        if let Some(s) = valid_at(next, at) {
            return fail(s, format!("the issuer {} is {}", next.subject(), s.replace('_', " ")), path);
        }
        path.push(next.subject().to_string());
        current = next.clone();
    }
    fail("untrusted", "the certificate chain is too long".into(), path)
}

// ---- MIME ------------------------------------------------------------------------------------

/// The header block and body of a message or entity: (header end, body start).
pub fn split_header(raw: &[u8]) -> (usize, usize) {
    let crlf = find(raw, b"\r\n\r\n").map(|p| (p + 2, p + 4));
    let lf = find(raw, b"\n\n").map(|p| (p + 1, p + 2));
    match (crlf, lf) {
        (Some(a), Some(b)) => {
            if a.0 <= b.0 {
                a
            } else {
                b
            }
        }
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => (raw.len(), raw.len()),
    }
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

/// Header fields (unfolded name, raw field including folding and line end).
pub fn header_fields(header: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    for line in header.split_inclusive(|b| *b == b'\n') {
        if line.trim_ascii().is_empty() {
            continue;
        }
        if (line[0] == b' ' || line[0] == b'\t') && !out.is_empty() {
            out.last_mut().expect("checked").1.extend_from_slice(line);
            continue;
        }
        let name = line.iter().position(|b| *b == b':').map(|p| String::from_utf8_lossy(&line[..p]).trim().to_ascii_lowercase()).unwrap_or_default();
        out.push((name, line.to_vec()));
    }
    out
}

fn field_value(raw_field: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw_field);
    let v = s.split_once(':').map(|x| x.1).unwrap_or("");
    v.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A Content-Type: (type/subtype lower case, parameters with lower-case names).
pub fn content_type(header: &[u8]) -> (String, Vec<(String, String)>) {
    let Some((_, f)) = header_fields(header).into_iter().find(|(n, _)| n == "content-type") else {
        return ("text/plain".into(), vec![]);
    };
    let v = field_value(&f);
    let mut parts = split_params(&v).into_iter();
    let ty = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
    let params = parts
        .filter_map(|p| {
            let (k, val) = p.split_once('=')?;
            let val = val.trim();
            let val = if val.starts_with('"') && val.ends_with('"') && val.len() >= 2 {
                val[1..val.len() - 1].replace("\\\"", "\"").replace("\\\\", "\\")
            } else {
                val.to_string()
            };
            Some((k.trim().to_ascii_lowercase(), val))
        })
        .collect();
    (ty, params)
}

fn split_params(v: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for c in v.chars() {
        if escaped {
            cur.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if quoted => {
                cur.push(c);
                escaped = true;
            }
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            ';' if !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

fn param<'a>(params: &'a [(String, String)], name: &str) -> Option<&'a str> {
    params.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// What a message (or entity) is, S/MIME-wise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// application/pkcs7-mime enveloped-data: the DER/BER structure.
    Enveloped(Vec<u8>),
    /// application/pkcs7-mime signed-data (content inside).
    SignedOpaque(Vec<u8>),
    /// multipart/signed: the signed first part (canonical CRLF) and the signature.
    SignedDetached { content: Vec<u8>, signature: Vec<u8> },
    Plain,
}

fn decode_body(header: &[u8], body: &[u8]) -> Vec<u8> {
    let cte = header_fields(header)
        .into_iter()
        .find(|(n, _)| n == "content-transfer-encoding")
        .map(|(_, f)| field_value(&f).to_ascii_lowercase())
        .unwrap_or_default();
    if cte == "base64" {
        let clean: Vec<u8> = body.iter().copied().filter(|b| !b.is_ascii_whitespace()).collect();
        base64::engine::general_purpose::STANDARD
            .decode(&clean)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(clean.strip_suffix(b"=").unwrap_or(&clean)))
            .unwrap_or_default()
    } else {
        body.to_vec()
    }
}

/// Canonical form for signatures: every line ends in CRLF.
pub fn canonical_crlf(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len() + b.len() / 40);
    let mut prev = 0u8;
    for &c in b {
        if c == b'\n' && prev != b'\r' {
            out.push(b'\r');
        }
        out.push(c);
        prev = c;
    }
    out
}

/// The parts of a multipart body: raw bytes of each part (headers and body), without the
/// CRLF that belongs to the following boundary.
pub fn multipart_parts(body: &[u8], boundary: &str) -> Vec<Vec<u8>> {
    let delim = format!("--{boundary}");
    let d = delim.as_bytes();
    // Positions of delimiter lines (at the start of the body or after a line break).
    let mut marks: Vec<(usize, usize)> = Vec::new(); // (line start, after the line)
    let mut i = 0;
    while let Some(p) = find(&body[i..], d).map(|p| p + i) {
        let at_line_start = p == 0 || body[p - 1] == b'\n';
        let line_end = body[p..].iter().position(|b| *b == b'\n').map(|e| p + e + 1).unwrap_or(body.len());
        if at_line_start {
            marks.push((p, line_end));
            if body[p + d.len()..].starts_with(b"--") {
                break;
            }
        }
        i = p + d.len();
    }
    let mut parts = Vec::new();
    for w in marks.windows(2) {
        let start = w[0].1;
        let mut end = w[1].0;
        // The line break before a delimiter belongs to the delimiter.
        if end > start && body[end - 1] == b'\n' {
            end -= 1;
            if end > start && body[end - 1] == b'\r' {
                end -= 1;
            }
        }
        parts.push(body[start..end.max(start)].to_vec());
    }
    parts
}

/// Classifies a message or entity.
pub fn classify(raw: &[u8]) -> Kind {
    let (he, bs) = split_header(raw);
    let header = &raw[..he];
    let body = &raw[bs.min(raw.len())..];
    let (ty, params) = content_type(header);
    match ty.as_str() {
        "application/pkcs7-mime" | "application/x-pkcs7-mime" => {
            let der = decode_body(header, body);
            let smime_type = param(&params, "smime-type").map(str::to_ascii_lowercase);
            match smime_type.as_deref() {
                Some("enveloped-data") | Some("authenveloped-data") => Kind::Enveloped(der),
                Some("signed-data") => Kind::SignedOpaque(der),
                _ => match parse_content_info(&der) {
                    Ok(ci) if ci.content_type == oid::SIGNED_DATA => Kind::SignedOpaque(der),
                    Ok(ci) if ci.content_type == oid::ENVELOPED_DATA || ci.content_type == oid::AUTH_ENVELOPED_DATA => Kind::Enveloped(der),
                    _ => Kind::Plain,
                },
            }
        }
        "multipart/signed" => {
            let protocol = param(&params, "protocol").unwrap_or("").to_ascii_lowercase();
            if protocol != "application/pkcs7-signature" && protocol != "application/x-pkcs7-signature" {
                return Kind::Plain;
            }
            let Some(boundary) = param(&params, "boundary") else { return Kind::Plain };
            let parts = multipart_parts(body, boundary);
            let [content, sig_part, ..] = parts.as_slice() else { return Kind::Plain };
            let (sh, sb) = split_header(sig_part);
            let signature = decode_body(&sig_part[..sh], &sig_part[sb.min(sig_part.len())..]);
            Kind::SignedDetached {
                content: canonical_crlf(content),
                signature,
            }
        }
        _ => Kind::Plain,
    }
}

const CONTENT_FIELDS: &[&str] = &["content-type", "content-transfer-encoding", "content-disposition", "content-id", "content-description", "mime-version"];

/// The MIME entity of a message: its Content-* fields and the body (canonical CRLF).
pub fn entity_of(raw: &[u8]) -> Vec<u8> {
    let (he, bs) = split_header(raw);
    let mut out = Vec::new();
    for (name, f) in header_fields(&raw[..he]) {
        if CONTENT_FIELDS.contains(&name.as_str()) && name != "mime-version" {
            out.extend_from_slice(&f);
        }
    }
    if out.is_empty() {
        out.extend_from_slice(b"Content-Type: text/plain\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(&raw[bs.min(raw.len())..]);
    canonical_crlf(&out)
}

/// The message with its content replaced by `entity`: the message's own header fields (From,
/// To, Subject, …) followed by the entity's.
pub fn with_entity(raw: &[u8], entity: &[u8]) -> Vec<u8> {
    let (he, _) = split_header(raw);
    let mut out = Vec::new();
    for (name, f) in header_fields(&raw[..he]) {
        if !CONTENT_FIELDS.contains(&name.as_str()) {
            out.extend_from_slice(&f);
        }
    }
    out.extend_from_slice(b"MIME-Version: 1.0\r\n");
    out.extend_from_slice(entity);
    canonical_crlf(&out)
}

fn b64_lines(data: &[u8]) -> Vec<u8> {
    let s = base64::engine::general_purpose::STANDARD.encode(data);
    let mut out = Vec::with_capacity(s.len() + s.len() / 38);
    for chunk in s.as_bytes().chunks(76) {
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\r\n");
    }
    out
}

fn boundary() -> String {
    let mut b = [0u8; 12];
    rand::fill(&mut b);
    format!("----=_smime_{}", hex::encode(b))
}

/// A `multipart/signed` entity around `entity` with the detached signature `sig` (`micalg`
/// from [`micalg`]).
pub fn signed_entity(entity: &[u8], sig: &[u8], micalg: &str) -> Vec<u8> {
    let b = boundary();
    let mut out = format!(
        "Content-Type: multipart/signed; protocol=\"application/pkcs7-signature\"; micalg={micalg}; boundary=\"{b}\"\r\n\r\nThis is a cryptographically signed message in MIME format.\r\n\r\n--{b}\r\n"
    )
    .into_bytes();
    out.extend_from_slice(entity);
    out.extend_from_slice(format!("\r\n--{b}\r\nContent-Type: application/pkcs7-signature; name=\"smime.p7s\"\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"smime.p7s\"\r\nContent-Description: S/MIME Cryptographic Signature\r\n\r\n").as_bytes());
    out.extend_from_slice(&b64_lines(sig));
    out.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());
    out
}

/// An `application/pkcs7-mime` enveloped-data entity.
pub fn enveloped_entity(der: &[u8]) -> Vec<u8> {
    let mut out = b"Content-Type: application/pkcs7-mime; smime-type=enveloped-data; name=\"smime.p7m\"\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"smime.p7m\"\r\nContent-Description: S/MIME Encrypted Message\r\n\r\n".to_vec();
    out.extend_from_slice(&b64_lines(der));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/mail/testdata/smime/");

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{DIR}{name}")).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    fn cert(name: &str) -> Vec<u8> {
        read_certificates(&fixture(name)).remove(0)
    }

    fn alice() -> (Vec<u8>, PrivateKey) {
        (cert("alice.pem"), read_private_key(&fixture("alice.key"), None).unwrap())
    }

    fn bob() -> Identity {
        Identity {
            cert: cert("bob.pem"),
            chain: vec![],
            key: read_private_key(&fixture("bob.key"), None).unwrap(),
        }
    }

    fn crlf(name: &str) -> Vec<u8> {
        canonical_crlf(&fixture(name))
    }

    #[test]
    fn certificates_and_keys() {
        let info = cert_info(&cert("alice.pem")).unwrap();
        assert_eq!(info.emails, vec!["alice@example.test"]);
        assert_eq!(info.name.as_deref(), Some("Alice"));
        assert!(info.issuer.contains("Ferrite Test CA") && !info.is_ca && info.email_protection && !info.self_signed);
        assert_eq!((info.key_type.as_str(), info.key_bits), ("rsa", Some(2048)));
        assert_eq!(info.fingerprint.len(), 64);
        let ca = cert_info(&cert("ca.pem")).unwrap();
        assert!(ca.is_ca && ca.self_signed && ca.emails.is_empty());
        let (a, ak) = alice();
        assert!(key_matches(&ak, &a));
        assert!(!key_matches(&ak, &cert("bob.pem")));
        let ec = read_private_key(&fixture("carol-ec.key"), None).unwrap();
        assert_eq!(ec.kind, KeyKind::P256);
        assert!(key_matches(&ec, &cert("carol-ec.pem")));
        assert!(read_private_key(b"-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n", None).is_err());
        assert!(read_certificates(b"garbage").is_empty());
    }

    #[test]
    fn pkcs12_modern_and_legacy() {
        for f in ["alice.p12", "alice-legacy.p12"] {
            let id = read_pkcs12(&fixture(f), "alice-p12").unwrap_or_else(|e| panic!("{f}: {e}"));
            assert_eq!(cert_info(&id.cert).unwrap().emails, vec!["alice@example.test"], "{f}");
            assert!(key_matches(&id.key, &id.cert));
            let err = read_pkcs12(&fixture(f), "wrong").unwrap_err();
            assert!(err.contains("password") || err.contains("could not be read"), "{f}: {err}");
        }
        let id = read_pkcs12(&fixture("alice.p12"), "alice-p12").unwrap();
        assert_eq!(id.chain.len(), 1, "the CA travels as the chain");
        assert!(read_pkcs12(b"not pkcs12", "x").is_err());
    }

    #[test]
    fn decrypts_openssl_messages() {
        let ids = [alice()];
        for (f, code) in [
            ("encrypted-aes.eml", "3456"),
            ("encrypted-3des.eml", "4567"),
            ("encrypted-oaep.eml", "5678"),
            ("encrypted-ber.eml", "8901"),
        ] {
            let Kind::Enveloped(der) = classify(&crlf(f)) else { panic!("{f} is enveloped") };
            let plain = decrypt(&der, &ids).unwrap_or_else(|e| panic!("{f}: {e}"));
            let text = String::from_utf8(plain).unwrap();
            assert!(text.contains(&format!("The launch code is {code}.")), "{f}: {text}");
        }
        // Not for Bob.
        let Kind::Enveloped(der) = classify(&crlf("encrypted-aes.eml")) else { panic!() };
        let b = bob();
        assert_eq!(decrypt(&der, &[(b.cert.clone(), b.key.clone())]), Err(DecryptError::NotForYou { recipients: 1 }));
        // Alice's certificate with Bob's key: the key transport fails, reported like corruption.
        let wrong = decrypt(&der, &[(cert("alice.pem"), b.key)]).unwrap_err();
        assert!(matches!(wrong, DecryptError::Failed(_)), "{wrong:?}");
        // Flipped ciphertext bits.
        let mut bad = der.clone();
        let n = bad.len();
        bad[n - 3] ^= 0x40;
        assert!(decrypt(&bad, &ids).is_err());
        assert!(matches!(decrypt(b"\x30\x03\x02\x01\x00", &ids), Err(DecryptError::Malformed(_))));
    }

    #[test]
    fn verifies_openssl_signatures() {
        let anchors = vec![cert("ca.pem")];
        for f in ["signed-detached.eml", "signed-opaque.eml", "signed-ec.eml"] {
            let (sig_der, detached) = match classify(&crlf(f)) {
                Kind::SignedDetached { content, signature } => (signature, Some(content)),
                Kind::SignedOpaque(der) => (der, None),
                other => panic!("{f}: {other:?}"),
            };
            let v = verify(&sig_der, detached.as_deref(), &[]);
            assert!(v.valid, "{f}: {:?}", v.problem);
            let signer = cert_info(v.signer.as_ref().unwrap()).unwrap();
            assert!(signer.emails[0] == "bob@example.test" || signer.emails[0] == "carol@example.test");
            assert!(v.signing_time.is_some());
            let content = detached.or(v.content.clone()).unwrap();
            assert!(String::from_utf8_lossy(&content).contains("The launch code is"));
            let chain = check_chain(v.signer.as_ref().unwrap(), &v.certificates, &anchors, Utc::now());
            assert_eq!(chain.status, "trusted", "{f}: {chain:?}");
            assert_eq!(chain.path.len(), 2);
            // Without the CA: untrusted.
            assert_eq!(check_chain(v.signer.as_ref().unwrap(), &v.certificates, &[], Utc::now()).status, "untrusted");
        }
    }

    #[test]
    fn tampered_signatures_fail() {
        let Kind::SignedDetached { content, signature } = classify(&crlf("signed-detached.eml")) else { panic!() };
        let changed = String::from_utf8(content.clone()).unwrap().replace("1234", "9999");
        let v = verify(&signature, Some(changed.as_bytes()), &[]);
        assert!(!v.valid && v.problem.as_deref().unwrap().contains("changed"), "{:?}", v.problem);
        // A flipped bit in the signature value itself.
        let mut sig = signature.clone();
        let n = sig.len();
        sig[n - 10] ^= 1;
        assert!(!verify(&sig, Some(&content), &[]).valid);
        // An attacker re-signs the content with their own self-signed key: the signature is
        // valid but the chain is not trusted.
        let eve = Identity {
            cert: cert("expired.pem"),
            chain: vec![],
            key: read_private_key(&fixture("expired.key"), None).unwrap(),
        };
        let forged = sign_detached(&content, &eve, Utc::now()).unwrap();
        let v = verify(&forged, Some(&content), &[]);
        assert!(v.valid);
        let chain = check_chain(v.signer.as_ref().unwrap(), &v.certificates, &[cert("ca.pem")], Utc::now());
        assert_eq!(chain.status, "expired", "{chain:?}");
        // Opaque signature with the content swapped inside is detected.
        let Kind::SignedOpaque(der) = classify(&crlf("signed-opaque.eml")) else { panic!() };
        let mut swapped = der.clone();
        let p = find(&swapped, b"2345").unwrap();
        swapped[p..p + 4].copy_from_slice(b"0000");
        assert!(!verify(&swapped, None, &[]).valid);
    }

    #[test]
    fn our_output_round_trips() {
        let entity = b"Content-Type: text/plain; charset=utf-8\r\n\r\nHello \xc3\xa9\r\n".to_vec();
        let b = bob();
        let sig_der = sign_detached(&entity, &b, Utc::now()).unwrap();
        let v = verify(&sig_der, Some(&entity), &[]);
        assert!(v.valid, "{:?}", v.problem);
        let signed = signed_entity(&entity, &sig_der, "sha-256");
        let Kind::SignedDetached { content, signature } = classify(&signed) else { panic!("signed entity") };
        assert_eq!(content, entity);
        assert!(verify(&signature, Some(&content), &[]).valid);
        let enc = encrypt(&signed, &[cert("alice.pem"), b.cert.clone()]).unwrap();
        let Kind::Enveloped(der) = classify(&enveloped_entity(&enc)) else { panic!("enveloped") };
        assert_eq!(decrypt(&der, &[alice()]).unwrap(), signed);
        assert_eq!(decrypt(&der, &[(b.cert.clone(), b.key.clone())]).unwrap(), signed);
        // EC certificates cannot be encrypted to (RSA key transport only).
        assert!(encrypt(&entity, &[cert("carol-ec.pem")]).is_err());
        assert!(encrypt(&entity, &[]).is_err());
    }

    #[test]
    fn interop_with_openssl_cli() {
        // Our output must verify and decrypt with `openssl cms` (skipped without openssl).
        if std::process::Command::new("openssl").arg("version").output().is_err() {
            eprintln!("openssl not found; skipping the CLI interop check");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let entity = b"Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 7bit\r\n\r\nInterop works.\r\n".to_vec();
        let b = bob();
        let msg = with_entity(
            b"From: bob@example.test\r\nTo: alice@example.test\r\nSubject: interop\r\n\r\n",
            &signed_entity(&entity, &sign_detached(&entity, &b, Utc::now()).unwrap(), "sha-256"),
        );
        let signed_path = dir.path().join("signed.eml");
        std::fs::write(&signed_path, &msg).unwrap();
        let out = std::process::Command::new("openssl")
            .args(["cms", "-verify", "-in"])
            .arg(&signed_path)
            .args(["-CAfile", &format!("{DIR}ca.pem")])
            .output()
            .unwrap();
        assert!(out.status.success(), "openssl cms -verify: {}", String::from_utf8_lossy(&out.stderr));
        assert!(String::from_utf8_lossy(&out.stdout).contains("Interop works."));
        let enc = with_entity(b"From: bob@example.test\r\nTo: alice@example.test\r\n\r\n", &enveloped_entity(&encrypt(&entity, &[cert("alice.pem")]).unwrap()));
        let enc_path = dir.path().join("enc.eml");
        std::fs::write(&enc_path, &enc).unwrap();
        let out = std::process::Command::new("openssl")
            .args(["cms", "-decrypt", "-in"])
            .arg(&enc_path)
            .args(["-recip", &format!("{DIR}alice.pem"), "-inkey", &format!("{DIR}alice.key")])
            .output()
            .unwrap();
        assert!(out.status.success(), "openssl cms -decrypt: {}", String::from_utf8_lossy(&out.stderr));
        assert!(String::from_utf8_lossy(&out.stdout).contains("Interop works."));
    }

    #[test]
    fn signed_and_encrypted() {
        let Kind::Enveloped(der) = classify(&crlf("signed-encrypted.eml")) else { panic!() };
        let inner = decrypt(&der, &[alice()]).unwrap();
        let Kind::SignedDetached { content, signature } = classify(&inner) else { panic!("inner signature: {}", String::from_utf8_lossy(&inner)) };
        let v = verify(&signature, Some(&content), &[]);
        assert!(v.valid, "{:?}", v.problem);
        assert!(String::from_utf8_lossy(&content).contains("6789"));
    }

    #[test]
    fn expired_and_unknown() {
        let c = check_chain(&cert("expired.pem"), &[], &[cert("ca.pem")], Utc::now());
        assert_eq!(c.status, "expired");
        let c = check_chain(&cert("ca.pem"), &[], &[], Utc::now());
        assert_eq!(c.status, "untrusted");
        assert!(c.detail.unwrap().contains("self-signed"));
        // A certificate the user trusts directly.
        assert_eq!(check_chain(&cert("bob.pem"), &[], &[cert("bob.pem")], Utc::now()).status, "trusted");
        // Bob cannot act as a CA for a certificate "issued" in his name.
        assert_eq!(check_chain(&cert("alice.pem"), &[cert("bob.pem")], &[], Utc::now()).status, "untrusted");
    }

    #[test]
    fn mime_helpers() {
        let raw = b"From: a@b.c\r\nSubject: x\r\nContent-Type: text/plain;\r\n charset=\"utf-8\"\r\nMIME-Version: 1.0\r\n\r\nbody\r\n";
        let e = entity_of(raw);
        assert_eq!(e, b"Content-Type: text/plain;\r\n charset=\"utf-8\"\r\n\r\nbody\r\n");
        let m = with_entity(raw, b"Content-Type: text/html\r\n\r\n<p>x</p>\r\n");
        let s = String::from_utf8(m).unwrap();
        assert!(s.starts_with("From: a@b.c\r\nSubject: x\r\nMIME-Version: 1.0\r\nContent-Type: text/html\r\n\r\n<p>x</p>"), "{s}");
        assert_eq!(content_type(b"Content-Type: Multipart/Signed; boundary=\"a;b\"; protocol=x\r\n").1[0], ("boundary".into(), "a;b".into()));
        let parts = multipart_parts(b"pre\r\n--b\r\nA\r\n\r\n--b\r\nB\r\n--b--\r\n", "b");
        assert_eq!(parts, vec![b"A\r\n".to_vec(), b"B".to_vec()]);
        assert_eq!(canonical_crlf(b"a\nb\r\nc"), b"a\r\nb\r\nc");
        assert_eq!(classify(b"Content-Type: text/plain\r\n\r\nhi"), Kind::Plain);
        // multipart/signed with another protocol (PGP) is not S/MIME.
        assert_eq!(classify(b"Content-Type: multipart/signed; protocol=\"application/pgp-signature\"; boundary=b\r\n\r\n--b\r\nx\r\n--b--"), Kind::Plain);
    }

    #[test]
    fn ber_normalization() {
        // SEQUENCE (indefinite) { OCTET STRING (constructed, indefinite) { "ab", "c" } }
        let ber = [0x30, 0x80, 0x24, 0x80, 0x04, 0x02, b'a', b'b', 0x04, 0x01, b'c', 0x00, 0x00, 0x00, 0x00];
        assert_eq!(ber_to_der(&ber).unwrap(), vec![0x30, 0x05, 0x04, 0x03, b'a', b'b', b'c']);
        assert!(ber_to_der(&[0x30, 0x80, 0x04, 0x01]).is_err());
        assert!(ber_to_der(&[0x30, 0x05, 0x04]).is_err());
        assert!(ber_to_der(&[0x04, 0x80, 0x00, 0x00]).is_err());
        // Deep nesting is refused, not a stack overflow.
        let mut deep = Vec::new();
        for _ in 0..200 {
            deep.extend_from_slice(&[0x30, 0x80]);
        }
        assert!(ber_to_der(&deep).is_err());
    }
}
