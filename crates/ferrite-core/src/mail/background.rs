//! Background sync of every account, whether or not anyone is online, at the interval the
//! administrator set (Settings → Administration → Mail).
//!
//! A round refreshes each account's folder list and counts (LIST, STATUS, QUOTA) and syncs
//! its inbox plus every folder that was opened before; on CONDSTORE servers an unchanged
//! folder costs one SELECT. At most [`CONCURRENCY`] accounts sync at once, each through its
//! normal connection pool. New inbox mail found while nobody watches the account over IDLE
//! is notified like live mail. Provisioning is reconciled at the start of each round.

use super::Account;
use super::store::Folder;
use crate::{Core, Error, Result};
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Accounts synced at the same time.
pub const CONCURRENCY: usize = 3;
/// The first round runs this long after start (start-up stays light).
const FIRST_ROUND: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Default, Serialize)]
pub struct RoundReport {
    pub accounts: usize,
    pub failed: usize,
    /// New messages found (all folders).
    pub new_messages: usize,
}

impl Core {
    /// Starts the periodic rounds for the life of the process.
    pub(crate) fn spawn_mail_sync_all(&self) {
        let core = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(FIRST_ROUND).await;
            loop {
                let started = std::time::Instant::now();
                let r = core.mail_background_round().await;
                tracing::debug!(accounts = r.accounts, failed = r.failed, new = r.new_messages, took = ?started.elapsed(), "mail background sync");
                // The interval may change between rounds.
                tokio::time::sleep(core.mail_sync_interval().await).await;
            }
        });
    }

    /// One round over every account (skipped when one is already running).
    pub async fn mail_background_round(&self) -> RoundReport {
        if self.mail.background_running.swap(true, Ordering::SeqCst) {
            return RoundReport::default();
        }
        let report = self.background_round_inner().await;
        self.mail.background_running.store(false, Ordering::SeqCst);
        report
    }

    async fn background_round_inner(&self) -> RoundReport {
        if let Err(e) = self.mail_provision_all().await {
            tracing::warn!(error = %e, "mail provisioning failed");
        }
        let ids: Vec<String> = match sqlx::query_scalar("SELECT id FROM mail_accounts ORDER BY synced_at IS NOT NULL, synced_at")
            .fetch_all(&self.db)
            .await
        {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(error = %e, "listing mail accounts failed");
                return RoundReport::default();
            }
        };
        let slots = Arc::new(tokio::sync::Semaphore::new(CONCURRENCY));
        let mut tasks = Vec::with_capacity(ids.len());
        for id in ids {
            let (core, slots) = (self.clone(), slots.clone());
            tasks.push(tokio::spawn(async move {
                let _permit = slots.acquire_owned().await.ok()?;
                let a = core.mail_account_by_id(&id).await.ok()?;
                Some(core.background_sync_account(&a).await)
            }));
        }
        let mut report = RoundReport::default();
        for t in tasks {
            if let Ok(Some(r)) = t.await {
                report.accounts += 1;
                match r {
                    Ok(n) => report.new_messages += n,
                    Err(_) => report.failed += 1,
                }
            }
        }
        report
    }

    /// Syncs one account's folder list and its opened folders; returns how many new messages
    /// were found.
    async fn background_sync_account(&self, a: &Account) -> Result<usize> {
        let r = async {
            let mut conn = self.imap_conn(a).await?;
            self.sync_folders_on(&mut conn, a).await?;
            let folders: Vec<Folder> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT {} FROM mail_folders WHERE account_id = ? AND selectable = 1 AND (role = 'inbox' OR synced_at IS NOT NULL)",
                super::store::FOLDER_COLS
            )))
            .bind(&a.id)
            .fetch_all(&self.db)
            .await?;
            let watched = self.mail.idlers.get(&a.id).is_some_and(|t| !t.handle.is_finished());
            let mut new = 0;
            for f in folders {
                let had_cache = f.synced_at.is_some();
                let report = match self.sync_folder_on(&mut conn, a, &f).await {
                    Ok(r) => r,
                    Err(e) if conn.is_broken() => return Err(e),
                    Err(e) => {
                        tracing::debug!(account = %a.id, folder = %f.name, error = %e, "background folder sync failed");
                        continue;
                    }
                };
                new += report.new.len();
                // The first sync of a folder fetches its existing mail: that is not news.
                if f.role.as_deref() == Some("inbox") && had_cache && !watched {
                    self.notify_new_mail(a, &report).await;
                }
            }
            Ok::<_, Error>(new)
        }
        .await;
        self.set_mail_error(&a.id, r.as_ref().err().map(|e| e.to_string()).as_deref()).await;
        r
    }
}
