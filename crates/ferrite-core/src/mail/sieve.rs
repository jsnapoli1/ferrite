//! Sieve for a mail account (over ManageSieve): the active script as text, visual filters and
//! the autoresponder, all kept in the one active script (see `filters` for its layout).
//! When no script is active yet, one named `ferrite` is created and activated.
//!
//! Script changes of one account are serialized, so two edits (say, the filter list and the
//! autoresponder) cannot overwrite each other's half of the script.

use super::Account;
use super::filters::{self, Filter, Vacation};
use super::managesieve::SieveConn;
use crate::{Core, Error, Result};
use dashmap::DashMap;
use serde::Serialize;
use std::sync::{Arc, OnceLock};

/// Name of the script Ferrite creates when an account has none active.
pub const SCRIPT_NAME: &str = "ferrite";

fn script_lock(account_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<DashMap<String, Arc<tokio::sync::Mutex<()>>>> = OnceLock::new();
    LOCKS.get_or_init(DashMap::new).entry(account_id.to_string()).or_default().clone()
}

/// The active script of an account.
#[derive(Debug, Clone, Serialize)]
pub struct ActiveScript {
    /// None when no script is active (saving creates one).
    pub name: Option<String>,
    pub script: String,
    /// Sieve extensions the server supports.
    pub extensions: Vec<String>,
    pub implementation: String,
}

/// The autoresponder of an account, and what it follows.
#[derive(Debug, Clone, Serialize)]
pub struct VacationView {
    #[serde(flatten)]
    pub vacation: Vacation,
    pub follows_absence: bool,
}

async fn read_active(conn: &mut SieveConn) -> Result<(Option<String>, String)> {
    match conn.active_script().await? {
        Some(name) => {
            let script = conn.get_script(&name).await?;
            // Servers end stored scripts with CRLF; keep them from piling up on each save.
            Ok((Some(name), script.trim_end_matches(['\r', '\n']).to_string()))
        }
        None => Ok((None, String::new())),
    }
}

/// Checks and stores `script` as the active script (creating `ferrite` when none is active).
async fn write_active(conn: &mut SieveConn, name: Option<&str>, script: &str) -> Result<()> {
    let script = filters::normalize_script(script);
    conn.check_script(&script).await?;
    let name = name.unwrap_or(SCRIPT_NAME);
    conn.put_script(name, &script).await?;
    conn.set_active(name).await
}

impl Core {
    /// The account's active Sieve script.
    pub async fn sieve_script(&self, user_id: &str, account_id: &str) -> Result<ActiveScript> {
        let a = self.mail_account(user_id, account_id).await?;
        let mut conn = self.sieve_conn(&a).await?;
        let (name, script) = read_active(&mut conn).await?;
        let caps = conn.caps.clone();
        conn.logout().await;
        Ok(ActiveScript {
            name,
            script,
            extensions: caps.sieve,
            implementation: caps.implementation,
        })
    }

    /// Replaces the active script with hand-written text. The server checks it first; its
    /// error message (with the line number) is returned as a bad request.
    pub async fn set_sieve_script(&self, user_id: &str, account_id: &str, script: &str) -> Result<ActiveScript> {
        if script.len() > 1024 * 1024 {
            return Err(Error::bad("the script is too long"));
        }
        let a = self.mail_account(user_id, account_id).await?;
        // The hand-written text may contain filter and autoresponder sections; reject broken ones.
        filters::parse_filters(script)?;
        filters::parse_vacation(script)?;
        {
            let lock = script_lock(&a.id);
            let _g = lock.lock().await;
            let mut conn = self.sieve_conn(&a).await?;
            let (name, _) = read_active(&mut conn).await?;
            write_active(&mut conn, name.as_deref(), script).await?;
            conn.logout().await;
        }
        self.sieve_script(user_id, account_id).await
    }

    /// Reads the active script, changes it with `edit` and stores the result.
    async fn edit_script(&self, a: &Account, edit: impl FnOnce(&str) -> Result<String>) -> Result<String> {
        let lock = script_lock(&a.id);
        let _g = lock.lock().await;
        let mut conn = self.sieve_conn(a).await?;
        let (name, current) = read_active(&mut conn).await?;
        let next = edit(&current)?;
        if next.trim_end_matches(['\r', '\n']) != current {
            write_active(&mut conn, name.as_deref(), &next).await?;
        }
        conn.logout().await;
        Ok(next)
    }

    /// The account's visual filters, in order.
    pub async fn mail_filters(&self, user_id: &str, account_id: &str) -> Result<Vec<Filter>> {
        let a = self.mail_account(user_id, account_id).await?;
        let mut conn = self.sieve_conn(&a).await?;
        let (_, script) = read_active(&mut conn).await?;
        conn.logout().await;
        Ok(filters::parse_filters(&script)?.data)
    }

    /// Replaces the account's visual filters (in the given order), keeping the rest of the
    /// script. Folder names are checked against the account's folders.
    pub async fn set_mail_filters(&self, user_id: &str, account_id: &str, list: &[Filter]) -> Result<Vec<Filter>> {
        let a = self.mail_account(user_id, account_id).await?;
        let folders = self.mail_folders(user_id, &a.id).await?;
        let names: Vec<String> = folders
            .iter()
            .filter(|f| f.selectable)
            .map(|f| super::parse::decode_mailbox_name(&f.name))
            .collect();
        let mut ordered = Vec::with_capacity(list.len());
        for (i, f) in list.iter().enumerate() {
            f.validate()?;
            for act in f.actions.iter().filter(|x| x.kind == "fileinto") {
                let m = act.mailbox.as_deref().unwrap_or("");
                if !names.iter().any(|n| n == m) {
                    return Err(Error::bad(format!("there is no folder {m:?} in {}", a.email)));
                }
            }
            ordered.push(Filter {
                priority: i as i64 + 1,
                name: f.name.trim().to_string(),
                ..f.clone()
            });
        }
        self.edit_script(&a, |cur| {
            let parsed = filters::parse_filters(cur)?;
            Ok(filters::build_filters(&ordered, &parsed.untouched))
        })
        .await?;
        Ok(ordered)
    }

    /// The account's autoresponder (disabled defaults when the script has none).
    pub async fn mail_vacation(&self, user_id: &str, account_id: &str) -> Result<VacationView> {
        let a = self.mail_account(user_id, account_id).await?;
        let settings = self.integrations_of(&a).await?;
        let mut conn = self.sieve_conn(&a).await?;
        let (_, script) = read_active(&mut conn).await?;
        conn.logout().await;
        let vacation = filters::parse_vacation(&script)?.data.unwrap_or_else(|| Vacation {
            addresses: vec![a.email.clone()],
            subject: "Re: ${subject}".into(),
            ..Vacation::default()
        });
        Ok(VacationView {
            vacation,
            follows_absence: settings.ooo_follows_absence,
        })
    }

    /// Sets the account's autoresponder by hand. Not allowed while it follows the absence.
    pub async fn set_mail_vacation(&self, user_id: &str, account_id: &str, v: &Vacation) -> Result<VacationView> {
        let a = self.mail_account(user_id, account_id).await?;
        if self.integrations_of(&a).await?.ooo_follows_absence {
            return Err(Error::bad(
                "this autoresponder follows your absence in Settings → Profile; change the absence there or stop following it",
            ));
        }
        self.write_vacation(&a, v).await?;
        self.mail_vacation(user_id, account_id).await
    }

    pub(crate) async fn write_vacation(&self, a: &Account, v: &Vacation) -> Result<()> {
        let mut v = v.clone();
        if v.addresses.is_empty() {
            v.addresses = vec![a.email.clone()];
        }
        v.addresses.iter_mut().for_each(|x| *x = x.trim().to_string());
        v.validate()?;
        self.edit_script(a, |cur| {
            let parsed = filters::parse_vacation(cur)?;
            if !v.enabled && parsed.data.is_none() {
                // Nothing to turn off: leave the script alone.
                return Ok(cur.to_string());
            }
            Ok(filters::build_vacation(&v, &parsed.untouched))
        })
        .await?;
        Ok(())
    }

    /// Sets the autoresponder of one account from the user's absence (on while an absence is
    /// set, with its days; off without one).
    pub(crate) async fn apply_absence_to_account(&self, user_id: &str, a: &Account) -> Result<()> {
        let absence = self.absence(user_id).await?;
        let v = match absence {
            Some(ab) => Vacation {
                enabled: true,
                start: Some(ab.starts_at()),
                end: Some(ab.ends_at()),
                subject: "Re: ${subject}".into(),
                message: if ab.message.trim().is_empty() {
                    format!("I am away until {}.", ab.last_day)
                } else {
                    ab.message.clone()
                },
                addresses: vec![a.email.clone()],
                ..Vacation::default()
            },
            None => Vacation {
                enabled: false,
                ..self.current_vacation(a).await?.unwrap_or_default()
            },
        };
        self.write_vacation(a, &v).await
    }

    async fn current_vacation(&self, a: &Account) -> Result<Option<Vacation>> {
        let mut conn = self.sieve_conn(a).await?;
        let (_, script) = read_active(&mut conn).await?;
        conn.logout().await;
        Ok(filters::parse_vacation(&script)?.data)
    }

    /// Applies the user's absence to every account whose autoresponder follows it. Returns
    /// the accounts that could not be updated (with the reason), which do not stop the others.
    pub async fn apply_absence(&self, user_id: &str) -> Result<Vec<(String, String)>> {
        let mut failed = Vec::new();
        for a in self.mail_accounts(user_id).await? {
            let s = self.integrations_of(&a).await?;
            if !(s.ooo_follows_absence && s.sieve_enabled) {
                continue;
            }
            if let Err(e) = self.apply_absence_to_account(user_id, &a).await {
                tracing::warn!(account = %a.id, error = %e, "applying the absence to the autoresponder failed");
                failed.push((a.email.clone(), e.to_string()));
            }
        }
        Ok(failed)
    }
}
