//! Single sign-on through OpenID Connect: provider settings (`[[oidc]]`), the identities
//! linked to local accounts, the authorization requests in flight, and how a verified
//! identity becomes a Ferrite user. The protocol itself (discovery, tokens, signatures)
//! lives in the server.

use crate::auth::{NewUser, User};
use crate::util::{now, random_token, sha256_hex};
use crate::{Core, Error, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// One OpenID Connect provider.
///
/// ```toml
/// [[oidc]]
/// id = "keycloak"
/// display_name = "Company login"
/// issuer = "https://id.example.org/realms/main"
/// client_id = "ferrite"
/// client_secret = "…"
/// auto_create = true
/// admin_group = "ferrite-admins"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcProvider {
    /// Short identifier used in URLs (`/api/auth/oidc/<id>/…`): letters, digits, `-`, `_`.
    pub id: String,
    /// Shown on the sign-in button.
    pub display_name: String,
    /// Issuer identifier; discovery is fetched from `<issuer>/.well-known/openid-configuration`.
    pub issuer: String,
    pub client_id: String,
    #[serde(default, skip_serializing)]
    pub client_secret: String,
    /// Space separated scopes; must include `openid`.
    #[serde(default = "default_scopes")]
    pub scopes: String,
    /// Claim that becomes the username of auto-created accounts.
    #[serde(default = "default_username_claim")]
    pub username_claim: String,
    /// Create an account on the first sign-in of an unknown identity.
    #[serde(default)]
    pub auto_create: bool,
    /// Members of this group (from the `groups` claim) are administrators.
    #[serde(default)]
    pub admin_group: Option<String>,
    /// Link an unknown identity to the account with the same email, when the provider says
    /// the email is verified.
    #[serde(default)]
    pub link_by_email: bool,
}

fn default_scopes() -> String {
    "openid email profile".into()
}

fn default_username_claim() -> String {
    "preferred_username".into()
}

impl OidcProvider {
    pub fn validate(&self) -> Result<()> {
        let id_ok = !self.id.is_empty() && self.id.len() <= 64 && self.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !id_ok {
            return Err(Error::bad(format!("oidc provider id {:?}: use letters, digits, '-' or '_'", self.id)));
        }
        if !(self.issuer.starts_with("https://") || self.issuer.starts_with("http://")) {
            return Err(Error::bad(format!("oidc provider {}: issuer must be an http(s) URL", self.id)));
        }
        if self.client_id.is_empty() {
            return Err(Error::bad(format!("oidc provider {}: client_id is required", self.id)));
        }
        if !self.scopes.split_whitespace().any(|s| s == "openid") {
            return Err(Error::bad(format!("oidc provider {}: scopes must include openid", self.id)));
        }
        Ok(())
    }
}

/// The verified facts about a person, taken from a validated ID token.
#[derive(Debug, Clone, Default)]
pub struct ExternalIdentity {
    pub subject: String,
    /// Value of the provider's username claim.
    pub username: Option<String>,
    pub email: Option<String>,
    pub email_verified: bool,
    pub name: Option<String>,
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct LinkedIdentity {
    pub provider: String,
    pub subject: String,
    pub email: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
}

/// An authorization request waiting for its callback.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OidcRequest {
    pub provider: String,
    pub nonce: String,
    pub verifier: String,
    pub browser_hash: String,
    pub return_to: String,
    pub expires_at: DateTime<Utc>,
}

/// How long the user may spend at the provider.
pub const REQUEST_TTL_SECS: i64 = 600;

impl Core {
    pub fn oidc_provider(&self, id: &str) -> Option<&OidcProvider> {
        self.config.oidc.iter().find(|p| p.id == id)
    }

    /// Remembers an authorization request under its `state`.
    pub async fn save_oidc_request(&self, state: &str, req: &OidcRequest) -> Result<()> {
        sqlx::query("DELETE FROM oidc_requests WHERE expires_at < ?")
            .bind(now())
            .execute(&self.db)
            .await?;
        sqlx::query("INSERT INTO oidc_requests (state_hash, provider, nonce, verifier, browser_hash, return_to, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
            .bind(sha256_hex(state.as_bytes()))
            .bind(&req.provider)
            .bind(&req.nonce)
            .bind(&req.verifier)
            .bind(&req.browser_hash)
            .bind(&req.return_to)
            .bind(req.expires_at)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Removes and returns the request for `state` (each state is usable once).
    pub async fn take_oidc_request(&self, state: &str) -> Result<Option<OidcRequest>> {
        let req: Option<OidcRequest> =
            sqlx::query_as("DELETE FROM oidc_requests WHERE state_hash = ? RETURNING provider, nonce, verifier, browser_hash, return_to, expires_at")
                .bind(sha256_hex(state.as_bytes()))
                .fetch_optional(&self.db)
                .await?;
        Ok(req.filter(|r| r.expires_at >= now()))
    }

    pub fn new_oidc_request(&self, provider: &str, browser_secret: &str, return_to: &str) -> OidcRequest {
        OidcRequest {
            provider: provider.to_string(),
            nonce: random_token(24),
            // RFC 7636 §4.1: 43–128 characters of [A-Za-z0-9-._~]; 32 random bytes in
            // base64url give 43.
            verifier: random_token(32),
            browser_hash: sha256_hex(browser_secret.as_bytes()),
            return_to: return_to.to_string(),
            expires_at: now() + Duration::seconds(REQUEST_TTL_SECS),
        }
    }

    pub async fn linked_identities(&self, user_id: &str) -> Result<Vec<LinkedIdentity>> {
        Ok(
            sqlx::query_as("SELECT provider, subject, email, created_at, last_login_at FROM oidc_links WHERE user_id = ? ORDER BY created_at")
                .bind(user_id)
                .fetch_all(&self.db)
                .await?,
        )
    }

    async fn link_identity(&self, provider: &str, id: &ExternalIdentity, user_id: &str, auto_created: bool) -> Result<()> {
        sqlx::query("INSERT INTO oidc_links (provider, subject, user_id, email, auto_created, created_at, last_login_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
            .bind(provider)
            .bind(&id.subject)
            .bind(user_id)
            .bind(&id.email)
            .bind(auto_created)
            .bind(now())
            .bind(now())
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Maps a verified identity to a local user: an existing link, else (per the provider's
    /// settings) a link by verified email or a new account. Applies the admin group.
    pub async fn sso_user(&self, provider: &OidcProvider, id: &ExternalIdentity) -> Result<User> {
        if id.subject.is_empty() {
            return Err(Error::bad("the identity provider did not send a subject"));
        }
        let linked: Option<(String, bool)> = sqlx::query_as("SELECT user_id, auto_created FROM oidc_links WHERE provider = ? AND subject = ?")
            .bind(&provider.id)
            .bind(&id.subject)
            .fetch_optional(&self.db)
            .await?;
        let (user, auto_created) = match linked {
            Some((user_id, auto)) => {
                sqlx::query("UPDATE oidc_links SET last_login_at = ?, email = COALESCE(?, email) WHERE provider = ? AND subject = ?")
                    .bind(now())
                    .bind(&id.email)
                    .bind(&provider.id)
                    .bind(&id.subject)
                    .execute(&self.db)
                    .await?;
                (self.user(&user_id).await?, auto)
            }
            None => self.first_sso_sign_in(provider, id).await?,
        };
        if user.disabled {
            return Err(Error::bad("this account is disabled"));
        }
        if let Some(group) = &provider.admin_group {
            let member = id.groups.iter().any(|g| g == group);
            // The provider grants admin rights; it only takes them away from accounts it
            // created, so linking a local administrator never locks them out.
            if (member && !user.is_admin) || (!member && user.is_admin && auto_created) {
                sqlx::query("UPDATE users SET is_admin = ? WHERE id = ?")
                    .bind(member)
                    .bind(&user.id)
                    .execute(&self.db)
                    .await?;
                return self.user(&user.id).await;
            }
        }
        Ok(user)
    }

    async fn first_sso_sign_in(&self, provider: &OidcProvider, id: &ExternalIdentity) -> Result<(User, bool)> {
        if provider.link_by_email && id.email_verified {
            if let Some(email) = id.email.as_deref().filter(|e| !e.trim().is_empty()) {
                let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM users WHERE email = ? COLLATE NOCASE")
                    .bind(email.trim())
                    .fetch_all(&self.db)
                    .await?;
                if ids.len() > 1 {
                    return Err(Error::bad(format!(
                        "several accounts use {email}; ask an administrator to link your {} sign-in",
                        provider.display_name
                    )));
                }
                if let Some(user_id) = ids.first() {
                    self.link_identity(&provider.id, id, user_id, false).await?;
                    return Ok((self.user(user_id).await?, false));
                }
            }
        }
        if !provider.auto_create {
            return Err(Error::bad(format!(
                "no Ferrite account is linked to this {} sign-in. Ask an administrator to create your account.",
                provider.display_name
            )));
        }
        let username = id
            .username
            .clone()
            .filter(|u| !u.trim().is_empty())
            .ok_or_else(|| Error::bad(format!("the identity provider did not send the {} claim", provider.username_claim)))?;
        let user = self
            .create_user(NewUser {
                username: username.trim().to_string(),
                display_name: id.name.clone(),
                email: id.email.clone().filter(|_| id.email_verified),
                // Unknown to anyone: the account signs in through the provider. An admin can
                // set a password later.
                password: random_token(32),
                is_admin: false,
                quota_bytes: None,
            })
            .await
            .map_err(|e| match e {
                Error::Conflict(_) => Error::bad(format!(
                    "the username {username:?} is already taken by another account; ask an administrator to link your {} sign-in",
                    provider.display_name
                )),
                other => other,
            })?;
        self.link_identity(&provider.id, id, &user.id, true).await?;
        Ok((user, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_defaults_and_validation() {
        let p: OidcProvider = toml::from_str(
            r#"
            id = "corp"
            display_name = "Corp"
            issuer = "https://id.example.org"
            client_id = "ferrite"
            client_secret = "s3cret"
            "#,
        )
        .unwrap();
        assert_eq!(p.scopes, "openid email profile");
        assert_eq!(p.username_claim, "preferred_username");
        assert!(!p.auto_create && !p.link_by_email && p.admin_group.is_none());
        p.validate().unwrap();
        assert!(!serde_json::to_string(&p).unwrap().contains("s3cret"), "secret never serialized");
        let mut bad = p.clone();
        bad.id = "a/b".into();
        assert!(bad.validate().is_err());
        let mut bad = p.clone();
        bad.scopes = "email".into();
        assert!(bad.validate().is_err());
    }
}
