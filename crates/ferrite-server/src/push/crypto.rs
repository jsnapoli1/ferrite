//! Web Push cryptography.
//!
//! * Message encryption: RFC 8291 key derivation on top of the RFC 8188 `aes128gcm` content
//!   coding, as a single record (RFC 8291 §4).
//! * Application server identification: RFC 8292 VAPID, an ES256-signed JWT plus the signing
//!   key in the `Authorization: vapid t=…, k=…` header.

use anyhow::{Context, Result, anyhow, bail, ensure};
use aws_lc_rs::signature::{self, EcdsaKeyPair, KeyPair};
use aws_lc_rs::{aead, agreement, hkdf, rand};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Record size written in the content-coding header. Push messages are one record, so this only
/// has to exceed the record (RFC 8291 §4); 4096 is the maximum payload push services accept.
pub const RECORD_SIZE: u32 = 4096;
/// Longest plaintext that fits in one 4096-octet push body (RFC 8291 §4).
pub const MAX_PLAINTEXT: usize = 3993;
const HEADER_LEN: usize = 16 + 4 + 1 + 65;
const TAG_LEN: usize = 16;

/// Lengths for HKDF-Expand output.
struct Len(usize);

impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn expand(prk: &hkdf::Prk, info: &[&[u8]], len: usize) -> Result<Vec<u8>> {
    let mut out = vec![0u8; len];
    prk.expand(info, Len(len))
        .and_then(|okm| okm.fill(&mut out))
        .map_err(|_| anyhow!("HKDF expand failed"))?;
    Ok(out)
}

/// The RFC 8291 §3.4 derivation: content encryption key and nonce for one message.
fn derive(ecdh_secret: &[u8], auth_secret: &[u8], ua_public: &[u8], as_public: &[u8], salt: &[u8]) -> Result<([u8; 16], [u8; 12])> {
    // HKDF-Extract(salt=auth_secret, IKM=ecdh_secret), then expand with key_info to 32 octets.
    let prk_key = hkdf::Salt::new(hkdf::HKDF_SHA256, auth_secret).extract(ecdh_secret);
    let ikm = expand(&prk_key, &[b"WebPush: info\0", ua_public, as_public], 32)?;
    // RFC 8188 §2.2/§2.3 with the message salt.
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(&ikm);
    let mut cek = [0u8; 16];
    cek.copy_from_slice(&expand(&prk, &[b"Content-Encoding: aes128gcm\0"], 16)?);
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&expand(&prk, &[b"Content-Encoding: nonce\0"], 12)?);
    Ok((cek, nonce))
}

fn ecdh(private: &agreement::PrivateKey, peer_public: &[u8]) -> Result<Vec<u8>> {
    // `agree` parses the peer key, which rejects points that are not on P-256 (RFC 8291 §7).
    agreement::agree(
        private,
        agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, peer_public),
        anyhow!("invalid P-256 public key"),
        |secret| Ok(secret.to_vec()),
    )
}

fn public_point(private: &agreement::PrivateKey) -> Result<Vec<u8>> {
    let public = private.compute_public_key().map_err(|_| anyhow!("computing the public key failed"))?;
    Ok(public.as_ref().to_vec())
}

/// Encrypts `plaintext` for a subscription (its `p256dh` key and `auth` secret) with a fresh
/// ephemeral key and salt. The record is padded to a multiple of 64 octets so lengths reveal
/// less about the content.
pub fn encrypt(plaintext: &[u8], ua_public: &[u8], auth_secret: &[u8]) -> Result<Vec<u8>> {
    ensure!(plaintext.len() <= MAX_PLAINTEXT, "push payload too large ({} bytes)", plaintext.len());
    let as_private = agreement::PrivateKey::generate(&agreement::ECDH_P256).map_err(|_| anyhow!("key generation failed"))?;
    let mut salt = [0u8; 16];
    rand::fill(&mut salt).map_err(|_| anyhow!("no randomness"))?;
    let unpadded = plaintext.len() + 1;
    let padding = (unpadded.div_ceil(64) * 64).min(MAX_PLAINTEXT + 1) - unpadded;
    encrypt_with(plaintext, ua_public, auth_secret, &as_private, &salt, padding)
}

/// [`encrypt`] with the sender key, salt and padding given (for test vectors).
pub fn encrypt_with(
    plaintext: &[u8],
    ua_public: &[u8],
    auth_secret: &[u8],
    as_private: &agreement::PrivateKey,
    salt: &[u8; 16],
    padding: usize,
) -> Result<Vec<u8>> {
    ensure!(
        ua_public.len() == 65 && ua_public[0] == 0x04,
        "the subscription key must be an uncompressed P-256 point"
    );
    ensure!(auth_secret.len() == 16, "the authentication secret must be 16 bytes");
    let as_public = public_point(as_private)?;
    let secret = ecdh(as_private, ua_public)?;
    let (cek, nonce) = derive(&secret, auth_secret, ua_public, &as_public, salt)?;
    ensure!(plaintext.len() + 1 + padding + TAG_LEN <= RECORD_SIZE as usize, "record too large");

    // The only (so last) record: plaintext, the 0x02 delimiter, then zero padding.
    let mut record = Vec::with_capacity(HEADER_LEN + plaintext.len() + 1 + padding + TAG_LEN);
    record.extend_from_slice(plaintext);
    record.push(0x02);
    record.resize(plaintext.len() + 1 + padding, 0);
    let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek).map_err(|_| anyhow!("bad key"))?);
    key.seal_in_place_append_tag(aead::Nonce::assume_unique_for_key(nonce), aead::Aad::empty(), &mut record)
        .map_err(|_| anyhow!("encryption failed"))?;

    // RFC 8188 §2.1 header: salt, record size, key id length, key id (the sender's public key).
    let mut out = Vec::with_capacity(HEADER_LEN + record.len());
    out.extend_from_slice(salt);
    out.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    out.push(as_public.len() as u8);
    out.extend_from_slice(&as_public);
    out.extend_from_slice(&record);
    Ok(out)
}

/// Decrypts a push message body as a user agent would (`ua_private` is the raw 32-byte P-256
/// scalar of the subscription key).
pub fn decrypt(body: &[u8], ua_private: &[u8], auth_secret: &[u8]) -> Result<Vec<u8>> {
    ensure!(body.len() >= 21, "body too short for a content-coding header");
    let salt = &body[..16];
    let rs = u32::from_be_bytes([body[16], body[17], body[18], body[19]]) as usize;
    let idlen = body[20] as usize;
    ensure!(body.len() >= 21 + idlen + TAG_LEN, "body too short");
    let as_public = &body[21..21 + idlen];
    let ciphertext = &body[21 + idlen..];
    ensure!(ciphertext.len() <= rs, "push messages must be a single record");
    let private = agreement::PrivateKey::from_private_key(&agreement::ECDH_P256, ua_private).map_err(|_| anyhow!("bad private key"))?;
    let ua_public = public_point(&private)?;
    let secret = ecdh(&private, as_public)?;
    let (cek, nonce) = derive(&secret, auth_secret, &ua_public, as_public, salt)?;
    let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek).map_err(|_| anyhow!("bad key"))?);
    let mut buf = ciphertext.to_vec();
    let plain = key
        .open_in_place(aead::Nonce::assume_unique_for_key(nonce), aead::Aad::empty(), &mut buf)
        .map_err(|_| anyhow!("decryption failed"))?;
    let end = plain.iter().rposition(|b| *b != 0).context("record has no padding delimiter")?;
    if plain[end] != 0x02 {
        bail!("the only record must end with the 0x02 delimiter");
    }
    Ok(plain[..end].to_vec())
}

/// The server's VAPID signing key.
pub struct Vapid {
    key: EcdsaKeyPair,
    subject: String,
}

/// Longest token lifetime RFC 8292 §2 allows.
pub const MAX_JWT_LIFETIME: i64 = 24 * 3600;
/// Lifetime of the tokens Ferrite signs (well inside the limit, allowing for clock skew).
pub const JWT_LIFETIME: i64 = 12 * 3600;

impl Vapid {
    /// A new key pair as PKCS#8, the form that is persisted.
    pub fn generate_pkcs8() -> Result<Vec<u8>> {
        let doc = EcdsaKeyPair::generate_pkcs8(&signature::ECDSA_P256_SHA256_FIXED_SIGNING, &rand::SystemRandom::new())
            .map_err(|_| anyhow!("VAPID key generation failed"))?;
        Ok(doc.as_ref().to_vec())
    }

    pub fn from_pkcs8(pkcs8: &[u8], subject: &str) -> Result<Self> {
        let key = EcdsaKeyPair::from_pkcs8(&signature::ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8).map_err(|e| anyhow!("invalid VAPID key: {e}"))?;
        Ok(Self {
            key,
            subject: subject.to_string(),
        })
    }

    /// Uncompressed public point: the `applicationServerKey` browsers subscribe with.
    pub fn public_key(&self) -> &[u8] {
        self.key.public_key().as_ref()
    }

    pub fn public_key_b64(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.public_key())
    }

    /// A signed JWT for one push service origin, valid until `exp` (Unix seconds).
    pub fn jwt(&self, audience: &str, exp: i64) -> Result<String> {
        let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&serde_json::json!({"aud": audience, "exp": exp, "sub": self.subject}))?);
        let signing_input = format!("{header}.{claims}");
        // Fixed-length r || s, as JWS ES256 requires (RFC 7518 §3.4).
        let sig = self
            .key
            .sign(&rand::SystemRandom::new(), signing_input.as_bytes())
            .map_err(|_| anyhow!("signing failed"))?;
        Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(sig.as_ref())))
    }

    /// The `Authorization` header value for a push to an endpoint whose origin is `audience`.
    pub fn authorization(&self, audience: &str, now: i64) -> Result<String> {
        Ok(format!("vapid t={}, k={}", self.jwt(audience, now + JWT_LIFETIME)?, self.public_key_b64()))
    }
}

/// Verifies an ES256 JWT against an uncompressed P-256 public key and returns its claims (what a
/// push service does with the `t` and `k` parameters).
pub fn verify_jwt(token: &str, public_key: &[u8]) -> Result<serde_json::Value> {
    let mut parts = token.split('.');
    let (Some(h), Some(c), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        bail!("a JWT has three parts");
    };
    let header: serde_json::Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(h)?)?;
    ensure!(header["alg"] == "ES256", "alg must be ES256");
    let sig = URL_SAFE_NO_PAD.decode(s)?;
    signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, public_key)
        .verify(format!("{h}.{c}").as_bytes(), &sig)
        .map_err(|_| anyhow!("bad signature"))?;
    Ok(serde_json::from_slice(&URL_SAFE_NO_PAD.decode(c)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private_scalar(key: &agreement::PrivateKey) -> Vec<u8> {
        use aws_lc_rs::encoding::{AsBigEndian, EcPrivateKeyBin};
        let bin: EcPrivateKeyBin = key.as_be_bytes().unwrap();
        bin.as_ref().to_vec()
    }

    fn b64(s: &str) -> Vec<u8> {
        let compact: String = s.split_whitespace().collect();
        URL_SAFE_NO_PAD.decode(compact).unwrap()
    }

    /// RFC 8291 Section 5 and Appendix A, value for value.
    #[test]
    fn rfc8291_appendix_a() {
        let plaintext = b64("V2hlbiBJIGdyb3cgdXAsIEkgd2FudCB0byBiZSBhIHdhdGVybWVsb24");
        assert_eq!(plaintext, b"When I grow up, I want to be a watermelon");
        let as_public = b64("BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIg Dll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8");
        let as_private = b64("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw");
        let ua_public = b64("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcx aOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4");
        let ua_private = b64("q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94");
        let salt: [u8; 16] = b64("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();
        let auth = b64("BTBZMqHH6r4Tts7J_aSIgg");

        // Intermediate values.
        let sender = agreement::PrivateKey::from_private_key(&agreement::ECDH_P256, &as_private).unwrap();
        assert_eq!(public_point(&sender).unwrap(), as_public);
        let secret = ecdh(&sender, &ua_public).unwrap();
        assert_eq!(secret, b64("kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs"));
        let receiver = agreement::PrivateKey::from_private_key(&agreement::ECDH_P256, &ua_private).unwrap();
        assert_eq!(public_point(&receiver).unwrap(), ua_public);
        assert_eq!(ecdh(&receiver, &as_public).unwrap(), secret);
        let key_info = [&b"WebPush: info\0"[..], &ua_public, &as_public].concat();
        assert_eq!(
            key_info,
            b64("V2ViUHVzaDogaW5mbwAEJXGyvs3942BVGq8e0PTNNmwR zr5VX4m8t7GGpTM5FzFo7OLr4BhZe9MEebhuPI-OztV3 \
                 ylkYfpJGmQ22ggCLDgT-M_SrDepxkU21WCP3O1SUj0Ew bZIHMtu5pZpTKGSCIA5Zent7wmC6HCJ5mFgJkuk5cwAv MBKiiujwa7t45ewP")
        );
        let (cek, nonce) = derive(&secret, &auth, &ua_public, &as_public, &salt).unwrap();
        assert_eq!(cek.to_vec(), b64("oIhVW04MRdy2XN9CiKLxTg"));
        assert_eq!(nonce.to_vec(), b64("4h_95klXJ5E_qnoN"));

        // The complete message body of Section 5.
        let body = encrypt_with(&plaintext, &ua_public, &auth, &sender, &salt, 0).unwrap();
        let expected = b64("DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml \
                            mlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPT \
                            pK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN");
        assert_eq!(body.len(), 144);
        assert_eq!(body, expected);
        assert_eq!(
            &body[..86],
            &b64("DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z 9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml mlMoZIIgDll6e3vCYLocInmYWAmS6Tlz AC8wEqKK6PBru3jl7A8")[..]
        );
        assert_eq!(
            &body[86..],
            &b64("8pfeW0KbunFT06SuDKoJH9Ql87S1QUrd irN6GcG7sFz1y1sqLgVi1VhjVkHsUoEs bI_0LpXMuGvnzQ")[..]
        );
        // And the receiving side.
        assert_eq!(decrypt(&expected, &ua_private, &auth).unwrap(), plaintext);
    }

    #[test]
    fn round_trip_with_padding_and_tamper_detection() {
        let ua = agreement::PrivateKey::generate(&agreement::ECDH_P256).unwrap();
        let ua_public = public_point(&ua).unwrap();
        let ua_private = private_scalar(&ua);
        let mut auth = [0u8; 16];
        rand::fill(&mut auth).unwrap();
        let msg = br#"{"title":"Alice mentioned you"}"#;
        let body = encrypt(msg, &ua_public, &auth).unwrap();
        assert_eq!((body.len() - HEADER_LEN - TAG_LEN) % 64, 0, "padded to 64-octet buckets");
        assert_eq!(decrypt(&body, &ua_private, &auth).unwrap(), msg);
        let mut bad = body.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(decrypt(&bad, &ua_private, &auth).is_err());
        assert!(decrypt(&body, &ua_private, &[0u8; 16]).is_err());
        assert!(encrypt(&vec![b'x'; MAX_PLAINTEXT + 1], &ua_public, &auth).is_err());
        assert!(encrypt(&vec![b'x'; MAX_PLAINTEXT], &ua_public, &auth).unwrap().len() <= 4096);
        // A key that is not on the curve is refused.
        let mut off_curve = ua_public.clone();
        off_curve[64] ^= 1;
        assert!(encrypt(msg, &off_curve, &auth).is_err());
    }

    #[test]
    fn vapid_jwt_verifies_and_is_bounded() {
        let vapid = Vapid::from_pkcs8(&Vapid::generate_pkcs8().unwrap(), "mailto:admin@example.org").unwrap();
        assert_eq!(vapid.public_key().len(), 65);
        assert_eq!(vapid.public_key()[0], 0x04);
        let now = chrono::Utc::now().timestamp();
        let header = vapid.authorization("https://push.example.net", now).unwrap();
        let rest = header.strip_prefix("vapid t=").unwrap();
        let (token, k) = rest.split_once(", k=").unwrap();
        assert_eq!(k, vapid.public_key_b64());
        let claims = verify_jwt(token, &URL_SAFE_NO_PAD.decode(k).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://push.example.net");
        assert_eq!(claims["sub"], "mailto:admin@example.org");
        let exp = claims["exp"].as_i64().unwrap();
        assert!(exp > now && exp - now <= MAX_JWT_LIFETIME);
        let jwt_header: serde_json::Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(token.split('.').next().unwrap()).unwrap()).unwrap();
        assert_eq!(jwt_header, serde_json::json!({"typ": "JWT", "alg": "ES256"}));
        // Signed by this key only.
        let other = Vapid::from_pkcs8(&Vapid::generate_pkcs8().unwrap(), "mailto:x@example.org").unwrap();
        assert!(verify_jwt(token, other.public_key()).is_err());
    }

    /// The JWT of RFC 8292 Section 2.4 verifies with its "k" key, so tokens are encoded and
    /// checked exactly as the specification shows.
    #[test]
    fn rfc8292_example_token() {
        let token: String = "eyJ0eXAiOiJKV1QiLCJhbGciOiJFUzI1NiJ9.eyJhdWQiOiJodHRwczovL3
            B1c2guZXhhbXBsZS5uZXQiLCJleHAiOjE0NTM1MjM3NjgsInN1YiI6Im1ha
            Wx0bzpwdXNoQGV4YW1wbGUuY29tIn0.i3CYb7t4xfxCDquptFOepC9GAu_H
            LGkMlMuCGSK2rpiUfnK9ojFwDXb1JrErtmysazNjjvW2L9OkSSHzvoD1oA"
            .split_whitespace()
            .collect();
        let k = b64("BA1Hxzyi1RUM1b5wjxsn7nGxAszw2u61m164i3MrAIxHF6YK5h4SDYic-dR uU_RCPCfA5aq9ojSwk5Y2EmClBPs");
        // The JWK coordinates of Figure 2 are the same key.
        assert_eq!(&k[1..33], &b64("DUfHPKLVFQzVvnCPGyfucbECzPDa7rWbXriLcysAjEc")[..]);
        assert_eq!(&k[33..], &b64("F6YK5h4SDYic-dRuU_RCPCfA5aq9ojSwk5Y2EmClBPs")[..]);
        let claims = verify_jwt(&token, &k).unwrap();
        assert_eq!(
            claims,
            serde_json::json!({"aud": "https://push.example.net", "exp": 1453523768, "sub": "mailto:push@example.com"})
        );
        let mut forged = token.clone();
        forged.replace_range(40..41, if &token[40..41] == "A" { "B" } else { "A" });
        assert!(verify_jwt(&forged, &k).is_err());
    }
}
