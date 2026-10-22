//! Users, groups, sessions and app tokens.

use crate::util::{hash_password, new_id, now, random_token, sha256_hex, verify_password};
use crate::{Core, Error, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct User {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub email: Option<String>,
    pub is_admin: bool,
    pub quota_bytes: Option<i64>,
    pub ai_consent: bool,
    pub disabled: bool,
    pub created_at: DateTime<Utc>,
}

const USER_COLS: &str = "id, username, display_name, email, is_admin, quota_bytes, ai_consent, disabled, created_at";

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct TokenInfo {
    pub id: String,
    pub user_id: String,
    pub kind: String,
    pub name: String,
    pub scopes: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl TokenInfo {
    /// Scopes are a comma separated list; `*` grants everything. Known scopes:
    /// `files`, `talk`, `boards`, `docs`, `ai`, `mcp`, `admin`.
    pub fn allows(&self, scope: &str) -> bool {
        self.scopes.split(',').map(str::trim).any(|s| s == "*" || s == scope)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct NewUser {
    pub username: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub password: String,
    #[serde(default)]
    pub is_admin: bool,
    pub quota_bytes: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UserUpdate {
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub password: Option<String>,
    pub is_admin: Option<bool>,
    pub quota_bytes: Option<Option<i64>>,
    pub ai_consent: Option<bool>,
    pub disabled: Option<bool>,
}

pub const SESSION_TTL_DAYS: i64 = 30;

fn validate_username(u: &str) -> Result<()> {
    let ok = (2..=64).contains(&u.len()) && u.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if ok { Ok(()) } else { Err(Error::bad("username must be 2-64 characters of letters, digits, '.', '_' or '-'")) }
}

impl Core {
    pub async fn create_user(&self, nu: NewUser) -> Result<User> {
        validate_username(&nu.username)?;
        if nu.password.len() < 8 {
            return Err(Error::bad("password must be at least 8 characters"));
        }
        let id = new_id();
        let hash = hash_password(&nu.password)?;
        let quota = nu.quota_bytes.or(self.config.default_quota);
        sqlx::query(
            "INSERT INTO users (id, username, display_name, email, password_hash, is_admin, quota_bytes, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&nu.username)
        .bind(nu.display_name.clone().filter(|d| !d.trim().is_empty()).unwrap_or_else(|| nu.username.clone()))
        .bind(&nu.email)
        .bind(&hash)
        .bind(nu.is_admin)
        .bind(quota)
        .bind(now())
        .execute(&self.db)
        .await
        .map_err(|e| match Error::from(e) {
            Error::Conflict(_) => Error::conflict("username already taken"),
            other => other,
        })?;
        self.ensure_root(&id).await?;
        self.user(&id).await
    }

    pub async fn user(&self, id: &str) -> Result<User> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {USER_COLS} FROM users WHERE id = ?")))
            .bind(id)
            .fetch_one(&self.db)
            .await?)
    }

    pub async fn user_by_name(&self, username: &str) -> Result<User> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {USER_COLS} FROM users WHERE username = ?")))
            .bind(username)
            .fetch_one(&self.db)
            .await?)
    }

    pub async fn user_count(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT COUNT(*) FROM users").fetch_one(&self.db).await?)
    }

    pub async fn list_users(&self, search: Option<&str>, limit: i64) -> Result<Vec<User>> {
        let pattern = format!("%{}%", search.unwrap_or("").replace('%', "\\%").replace('_', "\\_"));
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {USER_COLS} FROM users WHERE (username LIKE ?1 ESCAPE '\\' OR display_name LIKE ?1 ESCAPE '\\')
             ORDER BY display_name COLLATE NOCASE LIMIT ?2"
        )))
        .bind(pattern)
        .bind(limit)
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn update_user(&self, id: &str, up: UserUpdate) -> Result<User> {
        let mut tx = self.db.begin().await?;
        if let Some(d) = up.display_name.filter(|d| !d.trim().is_empty()) {
            sqlx::query("UPDATE users SET display_name = ? WHERE id = ?").bind(d.trim()).bind(id).execute(&mut *tx).await?;
        }
        if let Some(e) = up.email {
            sqlx::query("UPDATE users SET email = ? WHERE id = ?").bind(Some(e).filter(|e| !e.is_empty())).bind(id).execute(&mut *tx).await?;
        }
        if let Some(p) = up.password {
            if p.len() < 8 {
                return Err(Error::bad("password must be at least 8 characters"));
            }
            sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?").bind(hash_password(&p)?).bind(id).execute(&mut *tx).await?;
            // A password change signs out every browser session.
            sqlx::query("DELETE FROM tokens WHERE user_id = ? AND kind = 'session'").bind(id).execute(&mut *tx).await?;
        }
        if let Some(a) = up.is_admin {
            sqlx::query("UPDATE users SET is_admin = ? WHERE id = ?").bind(a).bind(id).execute(&mut *tx).await?;
        }
        if let Some(q) = up.quota_bytes {
            sqlx::query("UPDATE users SET quota_bytes = ? WHERE id = ?").bind(q).bind(id).execute(&mut *tx).await?;
        }
        if let Some(c) = up.ai_consent {
            sqlx::query("UPDATE users SET ai_consent = ? WHERE id = ?").bind(c).bind(id).execute(&mut *tx).await?;
        }
        if let Some(d) = up.disabled {
            sqlx::query("UPDATE users SET disabled = ? WHERE id = ?").bind(d).bind(id).execute(&mut *tx).await?;
            if d {
                sqlx::query("DELETE FROM tokens WHERE user_id = ?").bind(id).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        self.user(id).await
    }

    pub async fn delete_user(&self, id: &str) -> Result<()> {
        // Release blob references held by the user's files and versions before the cascade.
        let hashes: Vec<String> = sqlx::query_scalar(
            "SELECT blob_hash FROM nodes WHERE owner_id = ? AND blob_hash IS NOT NULL
             UNION ALL SELECT v.blob_hash FROM versions v JOIN nodes n ON n.id = v.node_id WHERE n.owner_id = ?",
        )
        .bind(id)
        .bind(id)
        .fetch_all(&self.db)
        .await?;
        let mut tx = self.db.begin().await?;
        for h in &hashes {
            sqlx::query("UPDATE blobs SET refcount = refcount - 1 WHERE hash = ?").bind(h).execute(&mut *tx).await?;
        }
        // Delete children first so the self referencing cascade never trips the unique root index.
        sqlx::query("DELETE FROM nodes WHERE owner_id = ? AND parent_id IS NOT NULL").bind(id).execute(&mut *tx).await?;
        let res = sqlx::query("DELETE FROM users WHERE id = ?").bind(id).execute(&mut *tx).await?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        tx.commit().await?;
        self.blobs.collect_garbage(&self.db).await?;
        Ok(())
    }

    /// Verifies credentials. Constant work whether or not the user exists.
    pub async fn authenticate(&self, username: &str, password: &str) -> Result<User> {
        let row: Option<(String, String, bool)> =
            sqlx::query_as("SELECT id, password_hash, disabled FROM users WHERE username = ?")
                .bind(username)
                .fetch_optional(&self.db)
                .await?;
        let pw = password.to_string();
        let stored = row.as_ref().map(|r| r.1.clone());
        let ok = tokio::task::spawn_blocking(move || match stored {
            Some(h) => verify_password(&pw, &h),
            None => {
                // Burn comparable time so usernames can't be enumerated by timing.
                static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
                let dummy = DUMMY.get_or_init(|| hash_password("ferrite-timing-dummy").unwrap_or_default());
                let _ = verify_password(&pw, dummy);
                false
            }
        })
        .await
        .map_err(|e| Error::internal(e))?;
        match row {
            Some((id, _, false)) if ok => self.user(&id).await,
            _ => Err(Error::Unauthorized),
        }
    }

    /// Creates a session or app token and returns `(info, secret)`. The secret is shown once.
    pub async fn create_token(&self, user_id: &str, kind: &str, name: &str, scopes: &str, ttl: Option<Duration>) -> Result<(TokenInfo, String)> {
        if kind != "session" && kind != "app" {
            return Err(Error::bad("token kind must be session or app"));
        }
        let secret = format!("ft_{}", random_token(32));
        let id = new_id();
        let created = now();
        let expires = ttl.map(|t| created + t);
        sqlx::query("INSERT INTO tokens (id, user_id, kind, name, secret_hash, scopes, created_at, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(&id)
            .bind(user_id)
            .bind(kind)
            .bind(name)
            .bind(sha256_hex(secret.as_bytes()))
            .bind(if scopes.is_empty() { "*" } else { scopes })
            .bind(created)
            .bind(expires)
            .execute(&self.db)
            .await?;
        let info = sqlx::query_as("SELECT id, user_id, kind, name, scopes, created_at, last_used_at, expires_at FROM tokens WHERE id = ?")
            .bind(&id)
            .fetch_one(&self.db)
            .await?;
        Ok((info, secret))
    }

    /// Resolves a bearer/session secret to its user. Updates `last_used_at` at most once a minute
    /// so authenticated reads don't each turn into a database write.
    pub async fn resolve_token(&self, secret: &str) -> Result<(User, TokenInfo)> {
        let info: TokenInfo = sqlx::query_as(
            "SELECT id, user_id, kind, name, scopes, created_at, last_used_at, expires_at FROM tokens WHERE secret_hash = ?",
        )
        .bind(sha256_hex(secret.as_bytes()))
        .fetch_optional(&self.db)
        .await?
        .ok_or(Error::Unauthorized)?;
        let t = now();
        if info.expires_at.is_some_and(|e| e < t) {
            sqlx::query("DELETE FROM tokens WHERE id = ?").bind(&info.id).execute(&self.db).await?;
            return Err(Error::Unauthorized);
        }
        if info.last_used_at.is_none_or(|l| t - l > Duration::seconds(60)) {
            sqlx::query("UPDATE tokens SET last_used_at = ? WHERE id = ?").bind(t).bind(&info.id).execute(&self.db).await?;
        }
        let user = self.user(&info.user_id).await.map_err(|_| Error::Unauthorized)?;
        if user.disabled {
            return Err(Error::Unauthorized);
        }
        Ok((user, info))
    }

    pub async fn list_tokens(&self, user_id: &str) -> Result<Vec<TokenInfo>> {
        Ok(sqlx::query_as(
            "SELECT id, user_id, kind, name, scopes, created_at, last_used_at, expires_at FROM tokens
             WHERE user_id = ? ORDER BY created_at DESC",
        )
        .bind(user_id)
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn revoke_token(&self, user_id: &str, token_id: &str) -> Result<()> {
        let r = sqlx::query("DELETE FROM tokens WHERE id = ? AND user_id = ?").bind(token_id).bind(user_id).execute(&self.db).await?;
        if r.rows_affected() == 0 { Err(Error::NotFound) } else { Ok(()) }
    }

    pub async fn revoke_secret(&self, secret: &str) -> Result<()> {
        sqlx::query("DELETE FROM tokens WHERE secret_hash = ?").bind(sha256_hex(secret.as_bytes())).execute(&self.db).await?;
        Ok(())
    }

    // Groups ---------------------------------------------------------------

    pub async fn create_group(&self, name: &str) -> Result<Group> {
        let name = name.trim();
        if name.is_empty() || name.len() > 64 {
            return Err(Error::bad("group name must be 1-64 characters"));
        }
        let id = new_id();
        sqlx::query("INSERT INTO groups (id, name, created_at) VALUES (?, ?, ?)").bind(&id).bind(name).bind(now()).execute(&self.db).await?;
        Ok(sqlx::query_as("SELECT id, name, created_at FROM groups WHERE id = ?").bind(&id).fetch_one(&self.db).await?)
    }

    pub async fn list_groups(&self) -> Result<Vec<Group>> {
        Ok(sqlx::query_as("SELECT id, name, created_at FROM groups ORDER BY name").fetch_all(&self.db).await?)
    }

    pub async fn delete_group(&self, id: &str) -> Result<()> {
        sqlx::query("DELETE FROM groups WHERE id = ?").bind(id).execute(&self.db).await?;
        Ok(())
    }

    pub async fn add_to_group(&self, group_id: &str, user_id: &str) -> Result<()> {
        sqlx::query("INSERT OR IGNORE INTO group_members (group_id, user_id) VALUES (?, ?)").bind(group_id).bind(user_id).execute(&self.db).await?;
        Ok(())
    }

    pub async fn remove_from_group(&self, group_id: &str, user_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM group_members WHERE group_id = ? AND user_id = ?").bind(group_id).bind(user_id).execute(&self.db).await?;
        Ok(())
    }

    pub async fn group_members(&self, group_id: &str) -> Result<Vec<User>> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {} FROM users u JOIN group_members g ON g.user_id = u.id WHERE g.group_id = ? ORDER BY u.display_name",
            USER_COLS.split(", ").map(|c| format!("u.{c}")).collect::<Vec<_>>().join(", ")
        )))
        .bind(group_id)
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn user_groups(&self, user_id: &str) -> Result<Vec<Group>> {
        Ok(sqlx::query_as(
            "SELECT g.id, g.name, g.created_at FROM groups g JOIN group_members m ON m.group_id = g.id WHERE m.user_id = ? ORDER BY g.name",
        )
        .bind(user_id)
        .fetch_all(&self.db)
        .await?)
    }
}
