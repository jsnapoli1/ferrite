//! Two-factor authentication: time-based one-time passwords (TOTP, RFC 6238 over the HOTP
//! algorithm of RFC 4226: HMAC-SHA-1, 6 digits, 30-second steps), one-time recovery codes,
//! and the short-lived challenge that sits between a correct password and a session.
//!
//! * TOTP secrets are encrypted at rest (AES-256-GCM, bound to the user id) with a key
//!   derived by HKDF from the server secret.
//! * A code is accepted for the current step and one step either side (clock drift), and
//!   never for a step at or before the last one accepted, so a code cannot be replayed.
//! * Recovery codes carry 80 random bits each and are stored as keyed hashes.

use crate::auth::User;
use crate::mailer::OutgoingMail;
use crate::util::{now, random_token, sha256_hex, short_token};
use crate::{Core, Error, Result};
use aws_lc_rs::{aead, constant_time, hkdf, hmac};
use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

/// Digits per code.
pub const DIGITS: u32 = 6;
/// Seconds per time step.
pub const PERIOD: u64 = 30;
/// Recovery codes issued at once.
pub const RECOVERY_CODES: usize = 10;
/// How long the second step of a sign-in may take.
pub const CHALLENGE_TTL_SECS: i64 = 300;
/// Wrong codes allowed on one challenge before it is discarded.
pub const CHALLENGE_ATTEMPTS: i64 = 5;
/// Issuer shown in authenticator apps.
pub const ISSUER: &str = "Ferrite";

/// HOTP (RFC 4226 §5.3): HMAC-SHA-1 of the big-endian counter, dynamic truncation, modulo
/// 10^digits.
pub fn hotp(key: &[u8], counter: u64, digits: u32) -> u32 {
    let k = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, key);
    let mac = hmac::sign(&k, &counter.to_be_bytes());
    let d = mac.as_ref();
    let offset = (d[d.len() - 1] & 0x0f) as usize;
    let binary = u32::from_be_bytes([d[offset] & 0x7f, d[offset + 1], d[offset + 2], d[offset + 3]]);
    binary % 10u32.pow(digits)
}

/// TOTP (RFC 6238 §4): HOTP with the number of whole `PERIOD`s since the Unix epoch.
pub fn totp(key: &[u8], unix_secs: u64, digits: u32) -> u32 {
    hotp(key, unix_secs / PERIOD, digits)
}

/// A code as typed: zero-padded to `digits`.
pub fn format_code(code: u32, digits: u32) -> String {
    format!("{code:0width$}", width = digits as usize)
}

/// Finds the time step `code` belongs to: the current step at `unix_secs` or one either side,
/// and strictly after `last_step` (replay protection). Comparison is constant-time.
pub fn match_step(key: &[u8], code: &str, unix_secs: u64, last_step: Option<i64>) -> Option<u64> {
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if code.len() != DIGITS as usize || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let now = unix_secs / PERIOD;
    let mut found = None;
    for step in [now.checked_sub(1), Some(now), now.checked_add(1)].into_iter().flatten() {
        let expected = format_code(hotp(key, step, DIGITS), DIGITS);
        let equal = constant_time::verify_slices_are_equal(expected.as_bytes(), code.as_bytes()).is_ok();
        let fresh = last_step.is_none_or(|l| i64::try_from(step).is_ok_and(|s| s > l));
        if equal && fresh && found.is_none() {
            found = Some(step);
        }
    }
    found
}

const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// Base32 (RFC 4648 §6) without padding, as authenticator apps expect.
pub fn base32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    let (mut buf, mut bits) = (0u32, 0u32);
    for &b in data {
        buf = (buf << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(B32[((buf >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(B32[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// Decodes base32, ignoring case, spaces and padding.
pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    let (mut buf, mut bits) = (0u32, 0u32);
    for c in s.chars().filter(|c| !c.is_whitespace() && *c != '=') {
        let v = B32.iter().position(|&x| x as char == c.to_ascii_uppercase())? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

fn uri_component(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The `otpauth://` URI authenticator apps scan (Key Uri Format): label `issuer:account`.
pub fn otpauth_uri(issuer: &str, account: &str, secret_b32: &str) -> String {
    format!(
        "otpauth://totp/{}:{}?secret={secret_b32}&issuer={}&algorithm=SHA1&digits={DIGITS}&period={PERIOD}",
        uri_component(issuer),
        uri_component(account),
        uri_component(issuer)
    )
}

/// Normalizes a typed recovery code: lowercase, without spaces or dashes.
pub fn normalize_recovery_code(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect()
}

/// A fresh recovery code: 16 characters from a 32-letter alphabet (80 bits), shown in
/// groups of four.
pub fn new_recovery_code() -> String {
    let raw = short_token(16);
    raw.as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("-")
}

#[derive(Debug, Clone, Serialize)]
pub struct TwoFactorStatus {
    pub enabled: bool,
    pub enabled_at: Option<DateTime<Utc>>,
    pub recovery_codes_left: i64,
    /// Required for everyone on this server.
    pub required: bool,
}

/// What an authenticator app needs: the secret (for typing in) and the `otpauth://` URI
/// (for the QR code).
#[derive(Debug, Clone, Serialize)]
pub struct Enrollment {
    pub secret: String,
    pub otpauth_uri: String,
}

/// Which second factor a sign-in used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Factor {
    Totp,
    RecoveryCode,
}

/// The second step of a password sign-in.
#[derive(Debug, Clone, Serialize)]
pub struct LoginChallenge {
    /// Single-use secret the client sends back with the code.
    pub challenge: String,
    /// `totp`: enter a code; `enroll`: 2FA is required, set it up first.
    pub kind: String,
    /// For `enroll`: the secret to add to an authenticator app.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enrollment: Option<Enrollment>,
    pub expires_at: DateTime<Utc>,
}

/// A completed second step: the user, plus fresh recovery codes when it enrolled them.
pub struct ChallengeOutcome {
    pub user: User,
    pub factor: Factor,
    pub recovery_codes: Option<Vec<String>>,
}

fn invalid_code() -> Error {
    Error::bad("that code is not valid")
}

fn unix_now() -> u64 {
    u64::try_from(now().timestamp()).unwrap_or(0)
}

impl Core {
    /// A 32-byte key for `purpose`, derived from the server secret.
    fn derived_key(&self, purpose: &[u8]) -> Result<[u8; 32]> {
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, b"ferrite auth security").extract(self.secret.as_bytes());
        let info = [purpose];
        let okm = prk.expand(&info, hkdf::HKDF_SHA256).map_err(|_| Error::internal("key derivation failed"))?;
        let mut key = [0u8; 32];
        okm.fill(&mut key).map_err(|_| Error::internal("key derivation failed"))?;
        Ok(key)
    }

    fn secret_cipher(&self) -> Result<aead::RandomizedNonceKey> {
        aead::RandomizedNonceKey::new(&aead::AES_256_GCM, &self.derived_key(b"totp secret encryption v1")?).map_err(|_| Error::internal("cipher setup failed"))
    }

    /// Encrypts a TOTP secret for `user_id`: base64url(nonce ‖ ciphertext ‖ tag).
    fn seal_secret(&self, user_id: &str, secret: &[u8]) -> Result<String> {
        let mut data = secret.to_vec();
        let nonce = self
            .secret_cipher()?
            .seal_in_place_append_tag(aead::Aad::from(user_id.as_bytes()), &mut data)
            .map_err(|_| Error::internal("encryption failed"))?;
        let mut out = nonce.as_ref().to_vec();
        out.extend_from_slice(&data);
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(out))
    }

    fn open_secret(&self, user_id: &str, sealed: &str) -> Result<Vec<u8>> {
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(sealed)
            .map_err(|_| Error::internal("stored TOTP secret is corrupt"))?;
        if raw.len() < aead::NONCE_LEN {
            return Err(Error::internal("stored TOTP secret is corrupt"));
        }
        let (nonce, ct) = raw.split_at(aead::NONCE_LEN);
        let nonce = aead::Nonce::try_assume_unique_for_key(nonce).map_err(|_| Error::internal("stored TOTP secret is corrupt"))?;
        let mut ct = ct.to_vec();
        let plain = self
            .secret_cipher()?
            .open_in_place(nonce, aead::Aad::from(user_id.as_bytes()), &mut ct)
            .map_err(|_| Error::internal("stored TOTP secret cannot be decrypted (was the server secret changed?)"))?;
        Ok(plain.to_vec())
    }

    fn recovery_hash(&self, user_id: &str, code: &str) -> Result<String> {
        let key = hmac::Key::new(hmac::HMAC_SHA256, &self.derived_key(b"recovery codes v1")?);
        let tag = hmac::sign(&key, format!("{user_id}:{}", normalize_recovery_code(code)).as_bytes());
        Ok(hex::encode(tag.as_ref()))
    }

    fn enrollment_for(&self, user: &User, secret: &[u8]) -> Enrollment {
        let host = self
            .config
            .public_url
            .split("://")
            .nth(1)
            .unwrap_or(&self.config.public_url)
            .split('/')
            .next()
            .unwrap_or_default()
            .to_string();
        let account = if host.is_empty() {
            user.username.clone()
        } else {
            format!("{}@{host}", user.username)
        };
        let b32 = base32_encode(secret);
        Enrollment {
            otpauth_uri: otpauth_uri(ISSUER, &account, &b32),
            secret: b32,
        }
    }

    pub async fn two_factor_enabled(&self, user_id: &str) -> Result<bool> {
        Ok(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM user_totp WHERE user_id = ? AND enabled = 1")
            .bind(user_id)
            .fetch_one(&self.db)
            .await?
            > 0)
    }

    /// Ids of users with 2FA enabled (for the admin's user list).
    pub async fn two_factor_users(&self) -> Result<std::collections::HashSet<String>> {
        Ok(sqlx::query_scalar("SELECT user_id FROM user_totp WHERE enabled = 1")
            .fetch_all(&self.db)
            .await?
            .into_iter()
            .collect())
    }

    pub async fn two_factor_status(&self, user_id: &str) -> Result<TwoFactorStatus> {
        let row: Option<(bool, Option<DateTime<Utc>>)> = sqlx::query_as("SELECT enabled, enabled_at FROM user_totp WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(&self.db)
            .await?;
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM recovery_codes WHERE user_id = ? AND used_at IS NULL")
            .bind(user_id)
            .fetch_one(&self.db)
            .await?;
        let enabled = row.as_ref().is_some_and(|r| r.0);
        Ok(TwoFactorStatus {
            enabled,
            enabled_at: row.and_then(|r| r.1).filter(|_| enabled),
            recovery_codes_left: if enabled { left } else { 0 },
            required: self.config.require_two_factor,
        })
    }

    /// Starts (or restarts) enrollment: a new secret, stored pending until a code confirms it.
    pub async fn begin_totp_enrollment(&self, user: &User) -> Result<Enrollment> {
        if self.two_factor_enabled(&user.id).await? {
            return Err(Error::conflict("two-factor authentication is already enabled"));
        }
        let mut secret = [0u8; 20];
        rand::fill(&mut secret[..]);
        sqlx::query(
            "INSERT INTO user_totp (user_id, secret_enc, enabled, created_at) VALUES (?, ?, 0, ?)
             ON CONFLICT(user_id) DO UPDATE SET secret_enc = excluded.secret_enc, enabled = 0, last_step = NULL,
             created_at = excluded.created_at, enabled_at = NULL",
        )
        .bind(&user.id)
        .bind(self.seal_secret(&user.id, &secret)?)
        .bind(now())
        .execute(&self.db)
        .await?;
        Ok(self.enrollment_for(user, &secret))
    }

    /// Confirms a pending enrollment with a code from the app. Returns the recovery codes,
    /// which are shown once.
    pub async fn confirm_totp_enrollment(&self, user: &User, code: &str) -> Result<Vec<String>> {
        let row: Option<(String, bool)> = sqlx::query_as("SELECT secret_enc, enabled FROM user_totp WHERE user_id = ?")
            .bind(&user.id)
            .fetch_optional(&self.db)
            .await?;
        let (sealed, enabled) = row.ok_or_else(|| Error::bad("start setting up two-factor authentication first"))?;
        if enabled {
            return Err(Error::conflict("two-factor authentication is already enabled"));
        }
        let secret = self.open_secret(&user.id, &sealed)?;
        let step = match_step(&secret, code, unix_now(), None).ok_or_else(invalid_code)?;
        self.enable_totp(user, &sealed, step).await
    }

    /// Marks the secret enabled (with `step` as the last accepted step), issues recovery
    /// codes and sends the security notice.
    async fn enable_totp(&self, user: &User, sealed: &str, step: u64) -> Result<Vec<String>> {
        let mut tx = self.db.begin().await?;
        sqlx::query(
            "INSERT INTO user_totp (user_id, secret_enc, enabled, last_step, created_at, enabled_at) VALUES (?, ?, 1, ?, ?, ?)
             ON CONFLICT(user_id) DO UPDATE SET secret_enc = excluded.secret_enc, enabled = 1, last_step = excluded.last_step,
             enabled_at = excluded.enabled_at",
        )
        .bind(&user.id)
        .bind(sealed)
        .bind(i64::try_from(step).unwrap_or(i64::MAX))
        .bind(now())
        .bind(now())
        .execute(&mut *tx)
        .await?;
        let codes = self.replace_recovery_codes(&mut tx, &user.id).await?;
        tx.commit().await?;
        self.security_notice(
            user,
            "Two-factor authentication was turned on",
            "Two-factor authentication is now on for your account. Signing in with your password now also needs a code from your authenticator app (or one of your recovery codes).",
        );
        Ok(codes)
    }

    async fn replace_recovery_codes(&self, tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, user_id: &str) -> Result<Vec<String>> {
        sqlx::query("DELETE FROM recovery_codes WHERE user_id = ?")
            .bind(user_id)
            .execute(&mut **tx)
            .await?;
        let mut codes = Vec::with_capacity(RECOVERY_CODES);
        for _ in 0..RECOVERY_CODES {
            let code = new_recovery_code();
            sqlx::query("INSERT INTO recovery_codes (user_id, code_hash) VALUES (?, ?)")
                .bind(user_id)
                .bind(self.recovery_hash(user_id, &code)?)
                .execute(&mut **tx)
                .await?;
            codes.push(code);
        }
        Ok(codes)
    }

    /// Issues a new set of recovery codes; the old ones stop working.
    pub async fn regenerate_recovery_codes(&self, user_id: &str) -> Result<Vec<String>> {
        if !self.two_factor_enabled(user_id).await? {
            return Err(Error::bad("two-factor authentication is not enabled"));
        }
        let mut tx = self.db.begin().await?;
        let codes = self.replace_recovery_codes(&mut tx, user_id).await?;
        tx.commit().await?;
        Ok(codes)
    }

    /// Turns 2FA off (after the caller checked the password and a code, or by an admin).
    /// Returns whether it was on. `by_admin` only changes the wording of the notice.
    pub async fn disable_two_factor(&self, user: &User, by_admin: bool) -> Result<bool> {
        let was = self.two_factor_enabled(&user.id).await?;
        let mut tx = self.db.begin().await?;
        for sql in [
            "DELETE FROM user_totp WHERE user_id = ?",
            "DELETE FROM recovery_codes WHERE user_id = ?",
            "DELETE FROM login_challenges WHERE user_id = ?",
        ] {
            sqlx::query(sql).bind(&user.id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        if was {
            let text = if by_admin {
                "An administrator turned off two-factor authentication for your account. Your password alone now signs you in; turn two-factor authentication back on in Settings → Security."
            } else {
                "Two-factor authentication was turned off for your account. Your password alone now signs you in. If this wasn't you, change your password and turn it back on in Settings → Security."
            };
            self.security_notice(user, "Two-factor authentication was turned off", text);
        }
        Ok(was)
    }

    /// Checks a second factor: a 6-digit TOTP code (each time step is accepted once) or an
    /// unused recovery code (consumed).
    pub async fn verify_second_factor(&self, user_id: &str, code: &str) -> Result<Factor> {
        let digits: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        if digits.len() == DIGITS as usize && digits.bytes().all(|b| b.is_ascii_digit()) {
            let row: Option<(String, Option<i64>)> = sqlx::query_as("SELECT secret_enc, last_step FROM user_totp WHERE user_id = ? AND enabled = 1")
                .bind(user_id)
                .fetch_optional(&self.db)
                .await?;
            let (sealed, last) = row.ok_or_else(invalid_code)?;
            let secret = self.open_secret(user_id, &sealed)?;
            let step = match_step(&secret, &digits, unix_now(), last).ok_or_else(invalid_code)?;
            let step = i64::try_from(step).unwrap_or(i64::MAX);
            // Conditional update: two concurrent requests with the same code can't both win.
            let r = sqlx::query("UPDATE user_totp SET last_step = ? WHERE user_id = ? AND enabled = 1 AND (last_step IS NULL OR last_step < ?)")
                .bind(step)
                .bind(user_id)
                .bind(step)
                .execute(&self.db)
                .await?;
            return if r.rows_affected() == 1 { Ok(Factor::Totp) } else { Err(invalid_code()) };
        }
        if normalize_recovery_code(code).len() != 16 || !self.two_factor_enabled(user_id).await? {
            return Err(invalid_code());
        }
        let r = sqlx::query("UPDATE recovery_codes SET used_at = ? WHERE user_id = ? AND code_hash = ? AND used_at IS NULL")
            .bind(now())
            .bind(user_id)
            .bind(self.recovery_hash(user_id, code)?)
            .execute(&self.db)
            .await?;
        if r.rows_affected() == 1 {
            Ok(Factor::RecoveryCode)
        } else {
            Err(invalid_code())
        }
    }

    /// After a correct password: the challenge for the second step, or `None` when the user
    /// can have a session right away (2FA neither enabled nor required).
    pub async fn begin_login_challenge(&self, user: &User) -> Result<Option<LoginChallenge>> {
        let enabled = self.two_factor_enabled(&user.id).await?;
        if !enabled && !self.config.require_two_factor {
            return Ok(None);
        }
        sqlx::query("DELETE FROM login_challenges WHERE expires_at < ?")
            .bind(now())
            .execute(&self.db)
            .await?;
        let token = format!("fc_{}", random_token(32));
        let expires_at = now() + Duration::seconds(CHALLENGE_TTL_SECS);
        let (kind, sealed, enrollment) = if enabled {
            ("totp", None, None)
        } else {
            let mut secret = [0u8; 20];
            rand::fill(&mut secret[..]);
            ("enroll", Some(self.seal_secret(&user.id, &secret)?), Some(self.enrollment_for(user, &secret)))
        };
        sqlx::query("INSERT INTO login_challenges (id_hash, user_id, kind, secret_enc, expires_at) VALUES (?, ?, ?, ?, ?)")
            .bind(sha256_hex(token.as_bytes()))
            .bind(&user.id)
            .bind(kind)
            .bind(sealed)
            .bind(expires_at)
            .execute(&self.db)
            .await?;
        Ok(Some(LoginChallenge {
            challenge: token,
            kind: kind.into(),
            enrollment,
            expires_at,
        }))
    }

    /// The user a live challenge belongs to (for throttling by account before checking the code).
    pub async fn login_challenge_user(&self, challenge: &str) -> Result<String> {
        sqlx::query_scalar("SELECT user_id FROM login_challenges WHERE id_hash = ? AND expires_at >= ?")
            .bind(sha256_hex(challenge.as_bytes()))
            .bind(now())
            .fetch_optional(&self.db)
            .await?
            .ok_or_else(|| Error::bad("this sign-in has expired; enter your password again"))
    }

    /// Completes the second step. The challenge is consumed on success, and discarded after
    /// `CHALLENGE_ATTEMPTS` wrong codes.
    pub async fn complete_login_challenge(&self, challenge: &str, code: &str) -> Result<ChallengeOutcome> {
        let id_hash = sha256_hex(challenge.as_bytes());
        let row: Option<(String, String, Option<String>, DateTime<Utc>)> =
            sqlx::query_as("SELECT user_id, kind, secret_enc, expires_at FROM login_challenges WHERE id_hash = ?")
                .bind(&id_hash)
                .fetch_optional(&self.db)
                .await?;
        let expired = || Error::bad("this sign-in has expired; enter your password again");
        let (user_id, kind, sealed, expires_at) = row.ok_or_else(expired)?;
        if expires_at < now() {
            sqlx::query("DELETE FROM login_challenges WHERE id_hash = ?")
                .bind(&id_hash)
                .execute(&self.db)
                .await?;
            return Err(expired());
        }
        let user = self.user(&user_id).await?;
        if user.disabled {
            return Err(Error::Unauthorized);
        }
        let result = match (kind.as_str(), sealed) {
            ("enroll", Some(sealed)) => {
                let secret = self.open_secret(&user.id, &sealed)?;
                match match_step(&secret, code, unix_now(), None) {
                    Some(step) => self.enable_totp(&user, &sealed, step).await.map(|codes| (Factor::Totp, Some(codes))),
                    None => Err(invalid_code()),
                }
            }
            ("totp", _) => self.verify_second_factor(&user.id, code).await.map(|f| (f, None)),
            _ => Err(expired()),
        };
        match result {
            Ok((factor, recovery_codes)) => {
                // Single use: whoever deletes the row first owns the sign-in.
                let r = sqlx::query("DELETE FROM login_challenges WHERE id_hash = ?")
                    .bind(&id_hash)
                    .execute(&self.db)
                    .await?;
                if r.rows_affected() == 0 {
                    return Err(expired());
                }
                Ok(ChallengeOutcome { user, factor, recovery_codes })
            }
            Err(e) => {
                sqlx::query("UPDATE login_challenges SET attempts = attempts + 1 WHERE id_hash = ?")
                    .bind(&id_hash)
                    .execute(&self.db)
                    .await?;
                sqlx::query("DELETE FROM login_challenges WHERE id_hash = ? AND attempts >= ?")
                    .bind(&id_hash)
                    .bind(CHALLENGE_ATTEMPTS)
                    .execute(&self.db)
                    .await?;
                Err(e)
            }
        }
    }

    /// Mails a security notice to the user, when the server can send mail and the user has an
    /// address. Sent in the background; failures are logged.
    pub fn security_notice(&self, user: &User, subject: &str, text: &str) {
        let Some(to) = user.email.clone().filter(|e| !e.trim().is_empty()) else {
            return;
        };
        if !self.mail_configured() {
            return;
        }
        let body = format!(
            "Hello {},\n\n{text}\n\nAccount: {}\nServer: {}\nTime: {}\n\nThis is an automatic security notice from Ferrite.\n",
            user.display_name,
            user.username,
            self.config.public_url,
            now().format("%Y-%m-%d %H:%M UTC")
        );
        let mail = OutgoingMail {
            to: vec![to],
            subject: format!("Ferrite: {subject}"),
            text: body,
            ..Default::default()
        };
        let core = self.clone();
        tokio::spawn(async move {
            if let Err(e) = core.send_system_mail(&mail).await {
                tracing::warn!(error = %e, "security notice could not be sent");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RFC_SEED: &[u8] = b"12345678901234567890";

    #[test]
    fn rfc4226_appendix_d() {
        let expected = [755224, 287082, 359152, 969429, 338314, 254676, 287922, 162583, 399871, 520489];
        for (count, want) in expected.iter().enumerate() {
            assert_eq!(hotp(RFC_SEED, count as u64, 6), *want, "count {count}");
        }
    }

    #[test]
    fn rfc6238_appendix_b_sha1() {
        // (time, TOTP) rows of the SHA1 column; the RFC prints 8-digit values.
        let rows: [(u64, &str); 6] = [
            (59, "94287082"),
            (1111111109, "07081804"),
            (1111111111, "14050471"),
            (1234567890, "89005924"),
            (2000000000, "69279037"),
            (20000000000, "65353130"),
        ];
        for (t, want) in rows {
            assert_eq!(format_code(totp(RFC_SEED, t, 8), 8), want, "T = {t}");
            // Six digits are the last six of the eight-digit value (same truncation).
            assert_eq!(format_code(totp(RFC_SEED, t, 6), 6), want[2..], "T = {t}");
        }
    }

    #[test]
    fn window_and_replay() {
        let t = 1_234_567_890u64;
        let step = t / PERIOD;
        let code = |s: u64| format_code(hotp(RFC_SEED, s, DIGITS), DIGITS);
        assert_eq!(match_step(RFC_SEED, &code(step), t, None), Some(step));
        assert_eq!(match_step(RFC_SEED, &code(step - 1), t, None), Some(step - 1));
        assert_eq!(match_step(RFC_SEED, &code(step + 1), t, None), Some(step + 1));
        assert_eq!(match_step(RFC_SEED, &code(step - 2), t, None), None);
        assert_eq!(match_step(RFC_SEED, &code(step + 2), t, None), None);
        // The last accepted step (and anything before it) is refused.
        assert_eq!(match_step(RFC_SEED, &code(step), t, Some(step as i64)), None);
        assert_eq!(match_step(RFC_SEED, &code(step - 1), t, Some(step as i64 - 1)), None);
        assert_eq!(match_step(RFC_SEED, &code(step + 1), t, Some(step as i64)), Some(step + 1));
        // Formatting: spaces tolerated, anything else refused.
        let spaced = format!("{} {}", &code(step)[..3], &code(step)[3..]);
        assert_eq!(match_step(RFC_SEED, &spaced, t, None), Some(step));
        assert_eq!(match_step(RFC_SEED, "12345", t, None), None);
        assert_eq!(match_step(RFC_SEED, "abcdef", t, None), None);
    }

    #[test]
    fn base32_round_trip() {
        // RFC 4648 §10 test vectors.
        for (raw, enc) in [
            ("", ""),
            ("f", "MY"),
            ("fo", "MZXQ"),
            ("foo", "MZXW6"),
            ("foob", "MZXW6YQ"),
            ("fooba", "MZXW6YTB"),
            ("foobar", "MZXW6YTBOI"),
        ] {
            assert_eq!(base32_encode(raw.as_bytes()), enc);
            assert_eq!(base32_decode(enc).unwrap(), raw.as_bytes());
        }
        assert_eq!(base32_encode(RFC_SEED), "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
        assert_eq!(base32_decode("gezd gnbv gy3t qojq gezd gnbv gy3t qojq").unwrap(), RFC_SEED);
        assert!(base32_decode("not base32!").is_none());
    }

    #[test]
    fn otpauth_uri_format() {
        let uri = otpauth_uri("Ferrite", "ada@example.org", "GEZDGNBVGY3TQOJQ");
        assert_eq!(
            uri,
            "otpauth://totp/Ferrite:ada%40example.org?secret=GEZDGNBVGY3TQOJQ&issuer=Ferrite&algorithm=SHA1&digits=6&period=30"
        );
    }

    #[test]
    fn recovery_codes() {
        let a = new_recovery_code();
        assert_eq!(a.len(), 19);
        assert_eq!(a.matches('-').count(), 3);
        assert_eq!(normalize_recovery_code(&a).len(), 16);
        assert_eq!(normalize_recovery_code(&a.to_uppercase().replace('-', " ")), normalize_recovery_code(&a));
        let all: std::collections::HashSet<_> = (0..200).map(|_| new_recovery_code()).collect();
        assert_eq!(all.len(), 200);
    }
}
