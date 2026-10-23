//! Anti-abuse alerts for sending: too many recipients in one message, or too many messages
//! from one user in 15 minutes, an hour or a day (sliding windows). Exceeding a threshold is
//! logged and every administrator is notified; it does not stop the mail (a compromised
//! account is for an administrator to judge). Each rule alerts at most once per window and
//! user, so a burst gives one notification, not hundreds.

use super::settings::AbuseSettings;
use crate::util::now;
use crate::{Core, Result};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

/// A threshold that was crossed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Violation {
    /// `recipients`, `15m`, `1h` or `1d`.
    pub rule: &'static str,
    pub observed: u32,
    pub threshold: u32,
}

/// The windows of the message-rate rules.
pub const WINDOWS: [(&str, i64); 3] = [("15m", 15 * 60), ("1h", 3600), ("1d", 86400)];

fn window_secs(rule: &str) -> i64 {
    WINDOWS.iter().find(|(r, _)| *r == rule).map(|(_, s)| *s).unwrap_or(86400)
}

/// Which rules a message with `recipients` sent `at` breaks, given the user's earlier sends
/// (`history`, any order; older than a day is ignored).
pub fn evaluate(cfg: &AbuseSettings, history: &[DateTime<Utc>], recipients: u32, at: DateTime<Utc>) -> Vec<Violation> {
    let mut out = Vec::new();
    if !cfg.enabled {
        return out;
    }
    if cfg.max_recipients > 0 && recipients > cfg.max_recipients {
        out.push(Violation {
            rule: "recipients",
            observed: recipients,
            threshold: cfg.max_recipients,
        });
    }
    for ((rule, secs), limit) in WINDOWS.iter().zip([cfg.per_15m, cfg.per_hour, cfg.per_day]) {
        if limit == 0 {
            continue;
        }
        let since = at - Duration::seconds(*secs);
        // This message counts too.
        let n = history.iter().filter(|t| **t > since && **t <= at).count() as u32 + 1;
        if n > limit {
            out.push(Violation {
                rule,
                observed: n,
                threshold: limit,
            });
        }
    }
    out
}

/// A recorded alert, for the admin page.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct AbuseAlert {
    pub id: i64,
    pub user_id: String,
    pub username: String,
    pub rule: String,
    pub observed: i64,
    pub threshold: i64,
    pub created_at: DateTime<Utc>,
}

impl Core {
    /// Records a sent message of `user_id` and raises the alerts it triggers. Never fails the
    /// send: problems are logged.
    pub async fn mail_record_send(&self, user_id: &str, recipients: u32) {
        if let Err(e) = self.record_send(user_id, recipients).await {
            tracing::warn!(error = %e, "anti-abuse bookkeeping failed");
        }
    }

    async fn record_send(&self, user_id: &str, recipients: u32) -> Result<()> {
        let cfg = self.mail_settings().await.abuse.clone();
        let at = now();
        let history: Vec<DateTime<Utc>> = if cfg.enabled {
            sqlx::query_scalar("SELECT sent_at FROM mail_send_log WHERE user_id = ? AND sent_at > ?")
                .bind(user_id)
                .bind(at - Duration::days(1))
                .fetch_all(&self.db)
                .await?
        } else {
            Vec::new()
        };
        sqlx::query("INSERT INTO mail_send_log (user_id, sent_at, recipients) VALUES (?, ?, ?)")
            .bind(user_id)
            .bind(at)
            .bind(recipients as i64)
            .execute(&self.db)
            .await?;
        for v in evaluate(&cfg, &history, recipients, at) {
            // Once per window and rule: skip if this rule already fired within its window.
            let window = if v.rule == "recipients" { 3600 } else { window_secs(v.rule) };
            let recent: Option<i64> = sqlx::query_scalar("SELECT id FROM mail_abuse_alerts WHERE user_id = ? AND rule = ? AND created_at > ? LIMIT 1")
                .bind(user_id)
                .bind(v.rule)
                .bind(at - Duration::seconds(window))
                .fetch_optional(&self.db)
                .await?;
            if recent.is_some() {
                continue;
            }
            sqlx::query("INSERT INTO mail_abuse_alerts (user_id, rule, observed, threshold, created_at) VALUES (?, ?, ?, ?, ?)")
                .bind(user_id)
                .bind(v.rule)
                .bind(v.observed as i64)
                .bind(v.threshold as i64)
                .bind(at)
                .execute(&self.db)
                .await?;
            let user = self.user(user_id).await?;
            let what = match v.rule {
                "recipients" => format!("sent one message to {} recipients (alert above {})", v.observed, v.threshold),
                rule => format!(
                    "sent {} messages within {} (alert above {})",
                    v.observed,
                    match rule {
                        "15m" => "15 minutes",
                        "1h" => "an hour",
                        _ => "a day",
                    },
                    v.threshold
                ),
            };
            tracing::warn!(user = %user.username, rule = v.rule, observed = v.observed, threshold = v.threshold, "suspicious mail sending");
            let admins: Vec<String> = sqlx::query_scalar("SELECT id FROM users WHERE is_admin = 1 AND disabled = 0")
                .fetch_all(&self.db)
                .await?;
            for admin in admins {
                if let Err(e) = self
                    .notify(
                        &admin,
                        "mail-abuse",
                        &format!("Suspicious sending by @{}", user.username),
                        &format!("{} {}.", user.display_name, what),
                        "/settings/admin-mail",
                    )
                    .await
                {
                    tracing::warn!(error = %e, "anti-abuse notification failed");
                }
            }
        }
        Ok(())
    }

    /// The most recent alerts (administrators).
    pub async fn mail_abuse_alerts(&self, limit: i64) -> Result<Vec<AbuseAlert>> {
        Ok(sqlx::query_as(
            "SELECT a.id, a.user_id, u.username, a.rule, a.observed, a.threshold, a.created_at FROM mail_abuse_alerts a JOIN users u ON u.id = a.user_id
             ORDER BY a.created_at DESC LIMIT ?",
        )
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.db)
        .await?)
    }

    /// Drops send-log rows older than a day and alerts older than 90 days.
    pub(crate) async fn mail_prune_send_log(&self) {
        let _ = sqlx::query("DELETE FROM mail_send_log WHERE sent_at < ?")
            .bind(now() - Duration::days(1))
            .execute(&self.db)
            .await;
        let _ = sqlx::query("DELETE FROM mail_abuse_alerts WHERE created_at < ?")
            .bind(now() - Duration::days(90))
            .execute(&self.db)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AbuseSettings {
        AbuseSettings {
            enabled: true,
            max_recipients: 10,
            per_15m: 3,
            per_hour: 5,
            per_day: 8,
        }
    }

    #[test]
    fn windows_and_thresholds() {
        let at: DateTime<Utc> = "2026-10-23T12:00:00Z".parse().unwrap();
        let ago = |mins: i64| at - Duration::minutes(mins);
        // Nothing before: only the recipient rule can fire.
        assert!(evaluate(&cfg(), &[], 10, at).is_empty(), "at the threshold is fine");
        assert_eq!(
            evaluate(&cfg(), &[], 11, at),
            vec![Violation {
                rule: "recipients",
                observed: 11,
                threshold: 10
            }]
        );
        // Three in the last 15 minutes plus this one: the 15-minute rule.
        let burst = [ago(1), ago(5), ago(14)];
        let v = evaluate(&cfg(), &burst, 1, at);
        assert_eq!(v.iter().map(|x| x.rule).collect::<Vec<_>>(), vec!["15m"]);
        assert_eq!(v[0].observed, 4);
        // Sends just outside a window don't count for it.
        let spread = [ago(16), ago(20), ago(40), ago(50), ago(59)];
        let v = evaluate(&cfg(), &spread, 1, at);
        assert_eq!(v.iter().map(|x| (x.rule, x.observed)).collect::<Vec<_>>(), vec![("1h", 6)]);
        let day: Vec<_> = (1..=8).map(|h| at - Duration::hours(h) - Duration::minutes(1)).collect();
        let v = evaluate(&cfg(), &day, 1, at);
        assert_eq!(v.iter().map(|x| (x.rule, x.observed)).collect::<Vec<_>>(), vec![("1d", 9)]);
        let v = evaluate(&cfg(), &[at - Duration::hours(25)], 1, at);
        assert!(v.is_empty(), "older than a day");
        // Off, or a zero threshold, never fires.
        let off = AbuseSettings { enabled: false, ..cfg() };
        assert!(evaluate(&off, &burst, 99, at).is_empty());
        let no_rate = AbuseSettings { per_15m: 0, ..cfg() };
        assert!(evaluate(&no_rate, &burst, 1, at).is_empty());
    }
}
