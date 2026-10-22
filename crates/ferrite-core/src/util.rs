use base64::Engine;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

pub fn now() -> DateTime<Utc> {
    Utc::now()
}

/// Time-ordered unique id (UUIDv7) as text.
pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// URL-safe random secret with `bytes` bytes of entropy.
pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::fill(&mut buf[..]);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// Short human-friendly token (lowercase alphanumerics) for room and share URLs.
pub fn short_token(len: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut buf = vec![0u8; len];
    rand::fill(&mut buf[..]);
    buf.iter().map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char).collect()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub fn hash_password(password: &str) -> crate::Result<String> {
    use argon2::password_hash::PasswordHasher;
    argon2::Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| crate::Error::internal(format!("hashing password: {e}")))
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    use argon2::password_hash::{PasswordVerifier, phc::PasswordHash};
    match PasswordHash::new(hash) {
        Ok(parsed) => argon2::Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok(),
        Err(_) => false,
    }
}

/// Validates a single path segment (file name, folder name).
pub fn validate_name(name: &str) -> crate::Result<()> {
    if name.is_empty() || name.len() > 255 || name == "." || name == ".." || name.contains('/') || name.contains('\0') || name.contains('\\') {
        return Err(crate::Error::bad(format!("invalid name: {name:?}")));
    }
    Ok(())
}

/// Splits a slash separated path into validated segments, ignoring empty segments.
pub fn split_path(path: &str) -> crate::Result<Vec<&str>> {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    for p in &parts {
        validate_name(p)?;
    }
    Ok(parts)
}

pub fn guess_mime(name: &str) -> String {
    if let Some(app) = crate::craft::app_for_name(name) {
        if let Some(m) = app.mime_for_name(name) {
            return m.to_string();
        }
    }
    match name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()) {
        Some(ref e) if e == "md" || e == "markdown" => "text/markdown".into(),
        Some(ref e) if e == "fmind" => crate::collab::MIME_MINDMAP.into(),
        Some(ref e) if e == "fcanvas" => crate::collab::MIME_CANVAS.into(),
        _ => mime_guess::from_path(name).first_or_octet_stream().essence_str().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_round_trip() {
        let h = hash_password("correct horse").unwrap();
        assert!(verify_password("correct horse", &h));
        assert!(!verify_password("wrong", &h));
    }

    #[test]
    fn names_are_validated() {
        assert!(validate_name("a.txt").is_ok());
        assert!(validate_name("..").is_err());
        assert!(validate_name("a/b").is_err());
        assert_eq!(split_path("/a//b/").unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn tokens_are_distinct() {
        assert_ne!(random_token(16), random_token(16));
        assert_eq!(short_token(10).len(), 10);
    }
}
