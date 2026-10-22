//! Account passwords at rest: ChaCha20-Poly1305 with a key derived (HKDF-SHA256) from the
//! server secret. Each value gets a fresh random nonce and is bound to its account id, so a
//! sealed password copied onto another account row does not open.

use crate::{Error, Result};
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

const NONCE_LEN: usize = 12;
const INFO: &[u8] = b"ferrite mail account password v1";

/// Seals and opens account secrets. Cheap to build; holds only the derived key.
pub struct SecretBox {
    cipher: ChaCha20Poly1305,
}

impl SecretBox {
    pub fn new(server_secret: &str) -> Self {
        let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(b"ferrite-mail"), server_secret.as_bytes());
        let mut key = [0u8; 32];
        hk.expand(INFO, &mut key).expect("32 bytes is a valid HKDF-SHA256 output length");
        SecretBox {
            cipher: ChaCha20Poly1305::new(&Key::from(key)),
        }
    }

    /// Encrypts `plain` for the record `context` (the account id), as base64(nonce || ciphertext).
    pub fn seal(&self, context: &str, plain: &str) -> Result<String> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::fill(&mut nonce[..]);
        let ct = self
            .cipher
            .encrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: plain.as_bytes(),
                    aad: context.as_bytes(),
                },
            )
            .map_err(|_| Error::internal("sealing a mail password failed"))?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ct);
        Ok(base64::engine::general_purpose::STANDARD.encode(out))
    }

    pub fn open(&self, context: &str, sealed: &str) -> Result<String> {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(sealed)
            .map_err(|_| Error::internal("stored mail password is corrupt"))?;
        if raw.len() <= NONCE_LEN {
            return Err(Error::internal("stored mail password is corrupt"));
        }
        let (nonce, ct) = raw.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into().expect("split at the nonce length");
        let plain = self
            .cipher
            .decrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: ct,
                    aad: context.as_bytes(),
                },
            )
            .map_err(|_| Error::internal("stored mail password cannot be decrypted (was the server secret changed?)"))?;
        String::from_utf8(plain).map_err(|_| Error::internal("stored mail password is corrupt"))
    }
}

#[cfg(test)]
mod tests {
    use super::SecretBox;

    #[test]
    fn round_trip_and_tamper_detection() {
        let b = SecretBox::new("server-secret");
        let sealed = b.seal("account-1", "hunter2 ü").unwrap();
        assert!(!sealed.contains("hunter2"));
        assert_eq!(b.open("account-1", &sealed).unwrap(), "hunter2 ü");
        // Fresh nonce each time.
        assert_ne!(sealed, b.seal("account-1", "hunter2 ü").unwrap());
        // Bound to the account it was sealed for.
        assert!(b.open("account-2", &sealed).is_err());
        // Another server secret cannot open it.
        assert!(SecretBox::new("other-secret").open("account-1", &sealed).is_err());
        // Flipped bits are detected.
        let mut raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &sealed).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 1;
        let tampered = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, raw);
        assert!(b.open("account-1", &tampered).is_err());
        assert!(b.open("account-1", "bm9wZQ").is_err());
    }
}
