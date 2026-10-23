//! Background work for inbox management, batched and light: snoozed messages are woken once
//! a minute (one query when nothing is due), importance classifiers are retrained every ten
//! minutes when the user taught them something (or daily), and trash retention runs hourly,
//! one pooled connection per account at a time.

use crate::Core;
use std::time::Duration;

const WAKE_EVERY: Duration = Duration::from_secs(60);
/// Retraining is checked every this many wake ticks.
const RETRAIN_TICKS: u64 = 10;
/// Trash retention runs every this many wake ticks.
const RETENTION_TICKS: u64 = 60;

impl Core {
    pub(crate) fn spawn_inbox_background(&self) {
        let core = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(WAKE_EVERY);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut rounds: u64 = 0;
            loop {
                tick.tick().await;
                rounds += 1;
                match core.wake_snoozed().await {
                    Ok(n) if n > 0 => tracing::info!(woken = n, "snoozed mail came back"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "waking snoozed mail failed"),
                }
                if rounds % RETRAIN_TICKS == 2 {
                    if let Err(e) = core.retrain_classifiers().await {
                        tracing::warn!(error = %e, "retraining importance classifiers failed");
                    }
                }
                if rounds % RETENTION_TICKS == 5 {
                    core.trash_retention_all().await;
                }
            }
        });
    }

    async fn trash_retention_all(&self) {
        let ids: Vec<String> = match sqlx::query_scalar("SELECT account_id FROM mail_account_prefs WHERE trash_retention_days > 0")
            .fetch_all(&self.db)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "listing accounts with trash retention failed");
                return;
            }
        };
        for id in ids {
            let Ok(a) = self.mail_account_by_id(&id).await else { continue };
            match self.run_trash_retention(&a).await {
                Ok(n) if n > 0 => tracing::info!(account = %id, deleted = n, "trash retention"),
                Ok(_) => {}
                Err(e) => tracing::info!(account = %id, error = %e, "trash retention failed"),
            }
        }
    }
}
