//! Who may do what with a mail account: the one access check every mail operation goes
//! through, and account delegation (sharing an account with other Ferrite users).
//!
//! [`Core::mail_access`] resolves the caller's relation to an account (owner or delegate) and
//! checks one [`MailRight`] against the matrix in [`allowed`]:
//!
//! | Right | Owner | Owner, provisioned account | Delegate |
//! |---|---|---|---|
//! | `Read`, `Modify`, `Send` (mail itself) | yes | yes | yes |
//! | `Configure` (sender name, signature, Sent copy) | yes | yes | yes |
//! | `Authenticate` (servers, usernames, passwords, OAuth, address) | yes | no | no |
//! | `Remove` (the account) | yes | no | no |
//! | `Share` (manage delegations) | yes | yes | no |
//! | `Owner` (being the owner, nothing else) | yes | yes | no |
//!
//! Strangers get `NotFound` (accounts are not revealed); a delegate refused a right gets
//! `Forbidden`. Folders and messages are checked through their account
//! ([`Core::mail_folder_access`], [`Core::mail_message_access`]), so an id of another
//! account of the same owner never rides along on a delegated one.
//!
//! The pre-delegation [`Core::mail_account`] is kept as a thin wrapper for `MailRight::Owner`
//! (fail closed: code written before delegation keeps treating accounts as owner-only).

use super::store::{Folder, MessageSummary};
use super::{ACCOUNT_COLS, Account};
use crate::util::{new_id, now};
use crate::{Core, Error, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;

/// What an operation needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailRight {
    /// List folders, read messages, search.
    Read,
    /// Flags, tags, moving, deleting, drafts, folder changes.
    Modify,
    /// Send mail from the account.
    Send,
    /// Sender name, signature, whether a Sent copy is kept.
    Configure,
    /// How the account signs in: servers, usernames, passwords, OAuth, the address.
    Authenticate,
    /// Remove the account.
    Remove,
    /// Share the account with others and revoke shares.
    Share,
    /// Being the account's owner, with no further check (what `mail_account` always meant).
    Owner,
}

/// The caller's relation to an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MailRole {
    Owner,
    Delegate,
}

/// The rights matrix (see the module documentation).
pub fn allowed(role: MailRole, provisioned: bool, right: MailRight) -> bool {
    use MailRight::*;
    match right {
        Read | Modify | Send | Configure => true,
        Share | Owner => role == MailRole::Owner,
        Authenticate | Remove => role == MailRole::Owner && !provisioned,
    }
}

/// What the viewer may do with an account, for the UI.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct AccountPermissions {
    pub configure: bool,
    pub authenticate: bool,
    pub remove: bool,
    pub share: bool,
}

impl AccountPermissions {
    pub fn of(role: MailRole, provisioned: bool) -> Self {
        AccountPermissions {
            configure: allowed(role, provisioned, MailRight::Configure),
            authenticate: allowed(role, provisioned, MailRight::Authenticate),
            remove: allowed(role, provisioned, MailRight::Remove),
            share: allowed(role, provisioned, MailRight::Share),
        }
    }
}

/// One delegation as the owner sees it.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Delegation {
    pub id: String,
    pub account_id: String,
    pub user_id: String,
    pub username: String,
    pub display_name: String,
    pub created_at: DateTime<Utc>,
}

/// A delegation in the administrator's overview.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct DelegationOverview {
    pub id: String,
    pub account_id: String,
    pub account_email: String,
    pub owner_id: String,
    pub owner_username: String,
    pub user_id: String,
    pub username: String,
    pub created_at: DateTime<Utc>,
}

impl Account {
    /// Fills the viewer-dependent fields for `viewer`.
    pub(crate) fn for_viewer(mut self, role: MailRole, owner_name: Option<String>) -> Self {
        self.role = Some(role);
        self.delegated = role == MailRole::Delegate;
        self.owner_name = if self.delegated { owner_name } else { None };
        self.permissions = AccountPermissions::of(role, self.provisioning_id.is_some());
        self
    }
}

impl Core {
    /// The central access check: `user_id`'s account `account_id`, if they may exercise
    /// `right` on it. `NotFound` when they have no relation to it, `Forbidden` when they have
    /// one but not this right.
    pub async fn mail_access(&self, user_id: &str, account_id: &str, right: MailRight) -> Result<Account> {
        let a = self.mail_account_by_id(account_id).await?;
        let role = if a.user_id == user_id {
            MailRole::Owner
        } else if self.mail_is_delegate(user_id, &a.id).await? {
            MailRole::Delegate
        } else {
            return Err(Error::NotFound);
        };
        if !allowed(role, a.provisioning_id.is_some(), right) {
            return Err(Error::Forbidden);
        }
        let owner_name = match role {
            MailRole::Delegate => self.user(&a.user_id).await.ok().map(|u| u.display_name),
            MailRole::Owner => None,
        };
        Ok(a.for_viewer(role, owner_name))
    }

    /// A folder, checked through its account.
    pub async fn mail_folder_access(&self, user_id: &str, folder_id: &str, right: MailRight) -> Result<(Account, Folder)> {
        let f = self.folder_row(folder_id).await?;
        let a = self.mail_access(user_id, &f.account_id, right).await?;
        Ok((a, f))
    }

    /// A cached message, checked through its account.
    pub async fn mail_message_access(&self, user_id: &str, message_id: &str, right: MailRight) -> Result<(Account, MessageSummary)> {
        let m = self.message_summary(message_id).await?;
        let a = self.mail_access(user_id, &m.account_id, right).await?;
        Ok((a, m))
    }

    async fn mail_is_delegate(&self, user_id: &str, account_id: &str) -> Result<bool> {
        let found: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM mail_delegations d JOIN users u ON u.id = d.user_id WHERE d.account_id = ? AND d.user_id = ? AND u.disabled = 0")
                .bind(account_id)
                .bind(user_id)
                .fetch_optional(&self.db)
                .await?;
        Ok(found.is_some())
    }

    /// Whether `user_id` may read the cached message `message_id` (search results).
    pub async fn mail_can_read_message(&self, user_id: &str, message_id: &str) -> Result<bool> {
        match self.mail_message_access(user_id, message_id, MailRight::Read).await {
            Ok(_) => Ok(true),
            Err(Error::NotFound) | Err(Error::Forbidden) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Every account `user_id` sees in Mail: their own and those delegated to them, in their
    /// chosen order (then oldest first), with the viewer-dependent fields filled in.
    pub async fn mail_accounts_for(&self, user_id: &str) -> Result<Vec<Account>> {
        let cols = ACCOUNT_COLS.split(", ").map(|c| format!("a.{c}")).collect::<Vec<_>>().join(", ");
        let rows: Vec<Account> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {cols} FROM mail_accounts a LEFT JOIN mail_account_order o ON o.account_id = a.id AND o.user_id = ?
             WHERE a.user_id = ? OR a.id IN (SELECT d.account_id FROM mail_delegations d JOIN users u ON u.id = d.user_id WHERE d.user_id = ? AND u.disabled = 0)
             ORDER BY o.position IS NULL, o.position, a.created_at"
        )))
        .bind(user_id)
        .bind(user_id)
        .bind(user_id)
        .fetch_all(&self.db)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for a in rows {
            if a.user_id == user_id {
                out.push(a.for_viewer(MailRole::Owner, None));
            } else {
                let owner = self.user(&a.user_id).await.ok().map(|u| u.display_name);
                out.push(a.for_viewer(MailRole::Delegate, owner));
            }
        }
        Ok(out)
    }

    /// Stores `user_id`'s order of their accounts; ids they cannot see are refused.
    pub async fn mail_set_account_order(&self, user_id: &str, ids: &[String]) -> Result<()> {
        let visible: Vec<String> = self.mail_accounts_for(user_id).await?.into_iter().map(|a| a.id).collect();
        if let Some(bad) = ids.iter().find(|i| !visible.contains(i)) {
            return Err(Error::bad(format!("no mail account {bad}")));
        }
        let mut tx = self.db.begin().await?;
        sqlx::query("DELETE FROM mail_account_order WHERE user_id = ?")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        for (pos, id) in ids.iter().enumerate() {
            sqlx::query("INSERT OR IGNORE INTO mail_account_order (user_id, account_id, position) VALUES (?, ?, ?)")
                .bind(user_id)
                .bind(id)
                .bind(pos as i64)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        self.hub.publish([user_id], &serde_json::json!({"type": "mail.changed", "what": "accounts"}));
        Ok(())
    }

    // ---- delegation ---------------------------------------------------------------------

    /// Loads who receives live events for which account (called once at start).
    pub(crate) async fn mail_load_delegations(&self) -> Result<()> {
        let rows: Vec<(String, String)> = sqlx::query_as("SELECT account_id, user_id FROM mail_delegations").fetch_all(&self.db).await?;
        self.mail.delegates.clear();
        for (account, user) in rows {
            self.mail.delegates.entry(account).or_default().push(user);
        }
        Ok(())
    }

    async fn refresh_delegates(&self, account_id: &str) -> Result<()> {
        let users: Vec<String> = sqlx::query_scalar("SELECT user_id FROM mail_delegations WHERE account_id = ?")
            .bind(account_id)
            .fetch_all(&self.db)
            .await?;
        if users.is_empty() {
            self.mail.delegates.remove(account_id);
        } else {
            self.mail.delegates.insert(account_id.to_string(), users);
        }
        Ok(())
    }

    /// The users who see an account: its owner and its delegates.
    pub fn mail_audience(&self, a: &Account) -> Vec<String> {
        let mut v = vec![a.user_id.clone()];
        if let Some(d) = self.mail.delegates.get(&a.id) {
            v.extend(d.iter().cloned());
        }
        v
    }

    /// Publishes a mail event to everyone who sees the account.
    pub fn mail_publish(&self, a: &Account, event: &serde_json::Value) {
        let audience = self.mail_audience(a);
        self.hub.publish(audience.iter().map(String::as_str), event);
    }

    /// The delegations of an account (owner only).
    pub async fn mail_delegations(&self, user_id: &str, account_id: &str) -> Result<Vec<Delegation>> {
        let a = self.mail_access(user_id, account_id, MailRight::Share).await?;
        Ok(sqlx::query_as(
            "SELECT d.id, d.account_id, d.user_id, u.username, u.display_name, d.created_at FROM mail_delegations d JOIN users u ON u.id = d.user_id
             WHERE d.account_id = ? ORDER BY d.created_at",
        )
        .bind(&a.id)
        .fetch_all(&self.db)
        .await?)
    }

    /// Shares an account with another user (by username or id). Owner only; not with
    /// yourself, a disabled user, or twice.
    pub async fn mail_delegate(&self, user_id: &str, account_id: &str, to: &str) -> Result<Delegation> {
        let a = self.mail_access(user_id, account_id, MailRight::Share).await?;
        let target = match self.user_by_name(to.trim()).await {
            Ok(u) => u,
            Err(Error::NotFound) => self.user(to.trim()).await.map_err(|_| Error::bad(format!("there is no user {to:?}")))?,
            Err(e) => return Err(e),
        };
        if target.id == a.user_id {
            return Err(Error::bad("an account cannot be shared with its owner"));
        }
        if target.disabled {
            return Err(Error::bad(format!("@{} is disabled", target.username)));
        }
        let id = new_id();
        let inserted =
            sqlx::query("INSERT INTO mail_delegations (id, account_id, user_id, created_by, created_at) VALUES (?, ?, ?, ?, ?) ON CONFLICT DO NOTHING")
                .bind(&id)
                .bind(&a.id)
                .bind(&target.id)
                .bind(user_id)
                .bind(now())
                .execute(&self.db)
                .await?;
        if inserted.rows_affected() == 0 {
            return Err(Error::conflict(format!("{} is already shared with @{}", a.email, target.username)));
        }
        self.refresh_delegates(&a.id).await?;
        let owner = self.user(&a.user_id).await?;
        tracing::info!(account = %a.id, owner = %owner.username, delegate = %target.username, "mail account delegated");
        if let Err(e) = self
            .notify(
                &target.id,
                "mail",
                &format!("{} shared a mail account with you", owner.display_name),
                &format!("You can now read and send mail as {} in Mail.", a.email),
                &format!("/mail?account={}", a.id),
            )
            .await
        {
            tracing::warn!(error = %e, "delegation notification failed");
        }
        self.hub.publish(
            [target.id.as_str(), user_id],
            &serde_json::json!({"type": "mail.changed", "account_id": a.id, "what": "accounts"}),
        );
        self.ensure_mail_idle(&target.id).await;
        self.mail_delegations(user_id, &a.id)
            .await?
            .into_iter()
            .find(|d| d.id == id)
            .ok_or(Error::NotFound)
    }

    /// Revokes a delegation: by the account's owner, by an administrator (`as_admin`), or by
    /// the delegate giving the account back.
    pub async fn mail_undelegate(&self, user_id: &str, delegation_id: &str, as_admin: bool) -> Result<()> {
        let row: Option<(String, String, String)> =
            sqlx::query_as("SELECT d.account_id, d.user_id, a.user_id FROM mail_delegations d JOIN mail_accounts a ON a.id = d.account_id WHERE d.id = ?")
                .bind(delegation_id)
                .fetch_optional(&self.db)
                .await?;
        let (account_id, delegate, owner) = row.ok_or(Error::NotFound)?;
        if !(as_admin || owner == user_id || delegate == user_id) {
            return Err(Error::NotFound);
        }
        sqlx::query("DELETE FROM mail_delegations WHERE id = ?")
            .bind(delegation_id)
            .execute(&self.db)
            .await?;
        sqlx::query("DELETE FROM mail_account_order WHERE user_id = ? AND account_id = ?")
            .bind(&delegate)
            .bind(&account_id)
            .execute(&self.db)
            .await?;
        self.refresh_delegates(&account_id).await?;
        tracing::info!(account = %account_id, delegate = %delegate, by = %user_id, "mail delegation revoked");
        self.hub.publish(
            [delegate.as_str(), owner.as_str()],
            &serde_json::json!({"type": "mail.changed", "account_id": account_id, "what": "accounts"}),
        );
        Ok(())
    }

    /// Every delegation on the server (administrators).
    pub async fn mail_all_delegations(&self) -> Result<Vec<DelegationOverview>> {
        Ok(sqlx::query_as(
            "SELECT d.id, d.account_id, a.email AS account_email, a.user_id AS owner_id, o.username AS owner_username, d.user_id, u.username, d.created_at
             FROM mail_delegations d JOIN mail_accounts a ON a.id = d.account_id JOIN users o ON o.id = a.user_id JOIN users u ON u.id = d.user_id
             ORDER BY o.username, a.email, u.username",
        )
        .fetch_all(&self.db)
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use MailRight::*;

    #[test]
    fn rights_matrix() {
        let all = [Read, Modify, Send, Configure, Authenticate, Remove, Share, Owner];
        for r in all {
            assert!(allowed(MailRole::Owner, false, r), "owners may do everything with their own accounts: {r:?}");
        }
        for r in [Read, Modify, Send, Configure] {
            assert!(allowed(MailRole::Delegate, false, r), "{r:?}");
            assert!(allowed(MailRole::Delegate, true, r), "{r:?}");
            assert!(allowed(MailRole::Owner, true, r), "{r:?}");
        }
        // Delegates never remove, re-authenticate, re-share, or count as the owner.
        for r in [Authenticate, Remove, Share, Owner] {
            assert!(!allowed(MailRole::Delegate, false, r), "{r:?}");
            assert!(!allowed(MailRole::Delegate, true, r), "{r:?}");
        }
        // Provisioned accounts are managed by the administrator.
        assert!(!allowed(MailRole::Owner, true, Authenticate));
        assert!(!allowed(MailRole::Owner, true, Remove));
        assert!(allowed(MailRole::Owner, true, Share));
        assert!(allowed(MailRole::Owner, true, Owner));
        assert_eq!(
            AccountPermissions::of(MailRole::Delegate, false),
            AccountPermissions {
                configure: true,
                authenticate: false,
                remove: false,
                share: false
            }
        );
    }
}
