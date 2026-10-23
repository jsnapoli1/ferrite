//! Importance classification for the priority inbox, learned per account from the user's own
//! behaviour, like Nextcloud Mail's classifier.
//!
//! Features of a message come from its sender's history in the account's incoming folders
//! (the share of their messages the user read, replied to and marked important, and how much
//! of the user's sent mail went to them) plus words of the subject (hashed into a few
//! buckets). A message's own flags are left out of its features (leave-one-out), so the model
//! cannot learn its label from itself.
//!
//! Until the user marked [`COLD_START`] messages important, Nextcloud's rules decide
//! (important share > 0.3, read > 0.7, replied > 0.1 or sent > 0.1). After that a small
//! L2-regularized logistic regression is trained on the newest [`MAX_TRAINING`] messages
//! (class-balanced, validated on a 20% hold-out first), retrained in the background when the
//! user marks messages important or not, and at least daily. New mail arriving in the inbox
//! that the model calls important gets the `$label1` keyword on the server.

use super::Account;
use super::parse::Addr;
use super::tags::{IMPORTANT, has_keyword};
use crate::util::now;
use crate::{Core, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Important messages needed before a model replaces the rules.
pub const COLD_START: usize = 20;
/// Messages (newest first) a model is trained on.
pub const MAX_TRAINING: i64 = 300;
/// Hash buckets for subject words.
pub const SUBJECT_BUCKETS: usize = 32;
/// Sender features before the subject buckets.
const SENDER_FEATURES: usize = 6;
pub const FEATURES: usize = SENDER_FEATURES + SUBJECT_BUCKETS;
/// A model is retrained at least this often.
const RETRAIN_EVERY: chrono::Duration = chrono::Duration::hours(24);

/// A sender's history in the account.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SenderStats {
    pub total: u32,
    pub read: u32,
    pub replied: u32,
    pub important: u32,
    /// Sent messages addressed to them.
    pub sent: u32,
}

impl SenderStats {
    /// The stats without one message of theirs (leave-one-out).
    pub fn without(mut self, seen: bool, answered: bool, important: bool) -> SenderStats {
        self.total = self.total.saturating_sub(1);
        self.read = self.read.saturating_sub(seen as u32);
        self.replied = self.replied.saturating_sub(answered as u32);
        self.important = self.important.saturating_sub(important as u32);
        self
    }

    fn share(n: u32, of: u32) -> f64 {
        if of == 0 { 0.0 } else { n as f64 / of as f64 }
    }

    pub fn read_share(&self) -> f64 {
        Self::share(self.read, self.total)
    }
    pub fn replied_share(&self) -> f64 {
        Self::share(self.replied, self.total)
    }
    pub fn important_share(&self) -> f64 {
        Self::share(self.important, self.total)
    }
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Subject words, lowercased, without reply/forward prefixes and very short words.
pub fn subject_words(subject: &str) -> Vec<String> {
    subject
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| w.chars().count() >= 3 && !matches!(w.as_str(), "re" | "fwd" | "aw" | "wg" | "the" | "and" | "for"))
        .collect()
}

/// The feature vector of a message.
pub fn features(st: &SenderStats, sent_total: u32, subject: &str) -> Vec<f64> {
    let mut v = vec![0.0; FEATURES];
    v[0] = st.read_share();
    v[1] = st.replied_share();
    v[2] = st.important_share();
    v[3] = SenderStats::share(st.sent, sent_total);
    v[4] = if st.total > 0 { 1.0 } else { 0.0 };
    v[5] = ((st.total as f64).ln_1p() / 5.0).min(1.0);
    let words = subject_words(subject);
    if !words.is_empty() {
        let w = 1.0 / (words.len() as f64).sqrt();
        for word in &words {
            v[SENDER_FEATURES + (fnv1a(word) % SUBJECT_BUCKETS as u64) as usize] += w;
        }
    }
    v
}

/// Nextcloud Mail's rules, used until there is enough to learn from.
pub fn rules(st: &SenderStats, sent_total: u32) -> bool {
    st.important_share() > 0.3 || st.read_share() > 0.7 || st.replied_share() > 0.1 || SenderStats::share(st.sent, sent_total) > 0.1
}

/// Logistic regression.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub weights: Vec<f64>,
    pub bias: f64,
}

fn sigmoid(z: f64) -> f64 {
    1.0 / (1.0 + (-z).exp())
}

impl Model {
    /// Full-batch gradient descent on the class-balanced, L2-regularized log loss.
    pub fn train(xs: &[Vec<f64>], ys: &[bool]) -> Model {
        let n = xs.len().max(1) as f64;
        let dims = xs.first().map_or(FEATURES, Vec::len);
        let pos = ys.iter().filter(|y| **y).count().max(1) as f64;
        let neg = ys.iter().filter(|y| !**y).count().max(1) as f64;
        let (w_pos, w_neg) = (n / (2.0 * pos), n / (2.0 * neg));
        let mut m = Model {
            weights: vec![0.0; dims],
            bias: 0.0,
        };
        const RATE: f64 = 0.5;
        const L2: f64 = 1e-3;
        for _ in 0..600 {
            let mut gw = vec![0.0; dims];
            let mut gb = 0.0;
            for (x, &y) in xs.iter().zip(ys) {
                let p = m.prob(x);
                let weight = if y { w_pos } else { w_neg };
                let err = weight * (p - if y { 1.0 } else { 0.0 });
                for (g, xi) in gw.iter_mut().zip(x) {
                    *g += err * xi;
                }
                gb += err;
            }
            for (w, g) in m.weights.iter_mut().zip(&gw) {
                *w -= RATE * (g / n + L2 * *w);
            }
            m.bias -= RATE * gb / n;
        }
        m
    }

    pub fn prob(&self, x: &[f64]) -> f64 {
        sigmoid(self.bias + self.weights.iter().zip(x).map(|(w, xi)| w * xi).sum::<f64>())
    }

    pub fn predict(&self, x: &[f64]) -> bool {
        self.prob(x) >= 0.5
    }
}

/// Share of `ys` that `predict` gets right.
pub fn accuracy(xs: &[Vec<f64>], ys: &[bool], predict: impl Fn(&[f64]) -> bool) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.iter().zip(ys).filter(|(x, y)| predict(x) == **y).count() as f64 / xs.len() as f64
}

/// What decides importance for an account.
#[derive(Debug, Clone)]
pub enum Classifier {
    Rules,
    Model(Model),
}

impl Classifier {
    pub fn important(&self, st: &SenderStats, sent_total: u32, subject: &str) -> bool {
        match self {
            Classifier::Rules => rules(st, sent_total),
            Classifier::Model(m) => m.predict(&features(st, sent_total, subject)),
        }
    }
}

/// Classifier state for the account settings.
#[derive(Debug, Clone, Serialize)]
pub struct ClassifierStatus {
    pub enabled: bool,
    /// "rules" until enough messages were marked important, then "model".
    pub mode: &'static str,
    pub samples: i64,
    pub important: i64,
    pub accuracy: Option<f64>,
    pub trained_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(sqlx::FromRow)]
struct SampleRow {
    id: String,
    from_addr: String,
    subject: String,
    seen: bool,
    answered: bool,
    keywords: String,
}

/// Folders whose mail counts as incoming (not sent, drafts, trash, junk, snoozed, shared).
const INCOMING: &str = "SELECT id FROM mail_folders WHERE account_id = ? AND selectable = 1 AND shared = 0 AND (role IS NULL OR role IN ('inbox', 'archive'))";

impl Core {
    /// Sender histories of an account, and how many sent messages were looked at.
    async fn sender_stats(&self, a: &Account) -> Result<(HashMap<String, SenderStats>, u32)> {
        let rows: Vec<(String, i64, i64, i64, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT lower(from_addr), COUNT(*), SUM(seen), SUM(answered), SUM(keywords LIKE '% $label1 %') FROM mail_messages
             WHERE folder_id IN ({INCOMING}) AND from_addr <> '' GROUP BY lower(from_addr)"
        )))
        .bind(&a.id)
        .fetch_all(&self.db)
        .await?;
        let mut stats: HashMap<String, SenderStats> = rows
            .into_iter()
            .map(|(addr, total, read, replied, important)| {
                (
                    addr,
                    SenderStats {
                        total: total as u32,
                        read: read as u32,
                        replied: replied as u32,
                        important: important as u32,
                        sent: 0,
                    },
                )
            })
            .collect();
        let sent: Vec<(String, String)> = sqlx::query_as(
            "SELECT to_addrs, cc_addrs FROM mail_messages WHERE folder_id IN (SELECT id FROM mail_folders WHERE account_id = ? AND role = 'sent')
             ORDER BY date DESC LIMIT 2000",
        )
        .bind(&a.id)
        .fetch_all(&self.db)
        .await?;
        let sent_total = sent.len() as u32;
        for (to, cc) in sent {
            let mut seen = std::collections::HashSet::new();
            for addr in serde_json::from_str::<Vec<Addr>>(&to)
                .unwrap_or_default()
                .into_iter()
                .chain(serde_json::from_str::<Vec<Addr>>(&cc).unwrap_or_default())
            {
                let key = addr.address.to_lowercase();
                if seen.insert(key.clone()) {
                    stats.entry(key).or_default().sent += 1;
                }
            }
        }
        Ok((stats, sent_total))
    }

    async fn load_classifier(&self, account_id: &str) -> Result<Classifier> {
        let model: Option<String> = sqlx::query_scalar("SELECT model FROM mail_classifiers WHERE account_id = ?")
            .bind(account_id)
            .fetch_optional(&self.db)
            .await?
            .flatten();
        Ok(match model.and_then(|m| serde_json::from_str::<Model>(&m).ok()) {
            Some(m) if m.weights.len() == FEATURES => Classifier::Model(m),
            _ => Classifier::Rules,
        })
    }

    /// Trains (or re-evaluates) an account's classifier from its cached mail.
    pub async fn train_classifier(&self, a: &Account) -> Result<ClassifierStatus> {
        let (stats, sent_total) = self.sender_stats(a).await?;
        let rows: Vec<SampleRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT id, from_addr, subject, seen, answered, keywords FROM mail_messages WHERE folder_id IN ({INCOMING}) AND from_addr <> ''
             AND lower(from_addr) <> lower(?) ORDER BY date DESC LIMIT {MAX_TRAINING}"
        )))
        .bind(&a.id)
        .bind(&a.email)
        .fetch_all(&self.db)
        .await?;
        let mut xs = Vec::with_capacity(rows.len());
        let mut ys = Vec::with_capacity(rows.len());
        for r in &rows {
            let important = has_keyword(&r.keywords, IMPORTANT);
            let st = stats
                .get(&r.from_addr.to_lowercase())
                .copied()
                .unwrap_or_default()
                .without(r.seen, r.answered, important);
            xs.push(features(&st, sent_total, &r.subject));
            ys.push(important);
        }
        let n_important = ys.iter().filter(|y| **y).count();
        let (model, acc) = if n_important >= COLD_START && n_important < ys.len() {
            // Validate on the newest 20% first, then train on everything.
            let hold = (xs.len() / 5).max(5);
            let check = Model::train(&xs[hold..], &ys[hold..]);
            let acc = accuracy(&xs[..hold], &ys[..hold], |x| check.predict(x));
            (Some(Model::train(&xs, &ys)), Some(acc))
        } else {
            let acc = (!rows.is_empty()).then(|| {
                let mut ok = 0;
                for (r, y) in rows.iter().zip(&ys) {
                    let st = stats
                        .get(&r.from_addr.to_lowercase())
                        .copied()
                        .unwrap_or_default()
                        .without(r.seen, r.answered, *y);
                    if rules(&st, sent_total) == *y {
                        ok += 1;
                    }
                }
                ok as f64 / rows.len() as f64
            });
            (None, acc)
        };
        sqlx::query(
            "INSERT INTO mail_classifiers (account_id, model, samples, important, accuracy, trained_at, dirty) VALUES (?, ?, ?, ?, ?, ?, 0)
             ON CONFLICT(account_id) DO UPDATE SET model = excluded.model, samples = excluded.samples, important = excluded.important,
             accuracy = excluded.accuracy, trained_at = excluded.trained_at, dirty = 0",
        )
        .bind(&a.id)
        .bind(model.as_ref().map(serde_json::to_string).transpose()?)
        .bind(rows.len() as i64)
        .bind(n_important as i64)
        .bind(acc)
        .bind(now())
        .execute(&self.db)
        .await?;
        self.classifier_status(&a.id).await
    }

    pub async fn classifier_status(&self, account_id: &str) -> Result<ClassifierStatus> {
        type Row = (Option<String>, i64, i64, Option<f64>, Option<chrono::DateTime<chrono::Utc>>);
        let row: Option<Row> = sqlx::query_as("SELECT model, samples, important, accuracy, trained_at FROM mail_classifiers WHERE account_id = ?")
            .bind(account_id)
            .fetch_optional(&self.db)
            .await?;
        let enabled = self.classify_enabled(account_id).await?;
        Ok(match row {
            Some((model, samples, important, accuracy, trained_at)) => ClassifierStatus {
                enabled,
                mode: if model.is_some() { "model" } else { "rules" },
                samples,
                important,
                accuracy,
                trained_at,
            },
            None => ClassifierStatus {
                enabled,
                mode: "rules",
                samples: 0,
                important: 0,
                accuracy: None,
                trained_at: None,
            },
        })
    }

    /// The user marked something important or not: retrain soon.
    pub(crate) async fn classifier_taught(&self, account_id: &str) {
        let r = sqlx::query("INSERT INTO mail_classifiers (account_id, dirty) VALUES (?, 1) ON CONFLICT(account_id) DO UPDATE SET dirty = 1")
            .bind(account_id)
            .execute(&self.db)
            .await;
        if let Err(e) = r {
            tracing::warn!(error = %e, "marking the classifier for retraining failed");
        }
    }

    /// Retrains the classifiers that were taught something or are a day old.
    pub(crate) async fn retrain_classifiers(&self) -> Result<()> {
        let accounts: Vec<String> = sqlx::query_scalar("SELECT id FROM mail_accounts").fetch_all(&self.db).await?;
        let stale = now() - RETRAIN_EVERY;
        for id in accounts {
            if !self.classify_enabled(&id).await? {
                continue;
            }
            let row: Option<(bool, Option<chrono::DateTime<chrono::Utc>>)> =
                sqlx::query_as("SELECT dirty, trained_at FROM mail_classifiers WHERE account_id = ?")
                    .bind(&id)
                    .fetch_optional(&self.db)
                    .await?;
            let due = match row {
                None => true,
                Some((dirty, trained)) => dirty || trained.is_none_or(|t| t < stale),
            };
            if !due {
                continue;
            }
            let a = self.mail_account_by_id(&id).await?;
            if let Err(e) = self.train_classifier(&a).await {
                tracing::info!(account = %id, error = %e, "training the importance classifier failed");
            }
        }
        Ok(())
    }

    /// Classifies mail that just arrived in an inbox, in the background.
    pub(crate) fn classify_new_mail_soon(&self, a: &Account, folder: &super::store::Folder, new: &[super::sync::NewMail]) {
        if folder.role.as_deref() != Some("inbox") || new.is_empty() {
            return;
        }
        let ids: Vec<String> = new.iter().map(|m| m.id.clone()).collect();
        let (core, a) = (self.clone(), a.clone());
        tokio::spawn(async move {
            if let Err(e) = core.classify_messages(&a, &ids).await {
                tracing::info!(account = %a.id, error = %e, "classifying new mail failed");
            }
        });
    }

    /// Tags the given messages `$label1` where the classifier calls them important. Returns
    /// the ids it tagged.
    pub async fn classify_messages(&self, a: &Account, ids: &[String]) -> Result<Vec<String>> {
        if !self.classify_enabled(&a.id).await? {
            return Ok(vec![]);
        }
        let classifier = self.load_classifier(&a.id).await?;
        let (stats, sent_total) = self.sender_stats(a).await?;
        let rows: Vec<SampleRow> = sqlx::query_as(
            "SELECT id, from_addr, subject, seen, answered, keywords FROM mail_messages WHERE account_id = ? AND id IN (SELECT value FROM json_each(?))",
        )
        .bind(&a.id)
        .bind(serde_json::to_string(ids)?)
        .fetch_all(&self.db)
        .await?;
        let important: Vec<String> = rows
            .iter()
            .filter(|r| !has_keyword(&r.keywords, IMPORTANT) && !r.from_addr.is_empty() && !r.from_addr.eq_ignore_ascii_case(&a.email))
            .filter(|r| {
                let st = stats
                    .get(&r.from_addr.to_lowercase())
                    .copied()
                    .unwrap_or_default()
                    .without(r.seen, r.answered, false);
                classifier.important(&st, sent_total, &r.subject)
            })
            .map(|r| r.id.clone())
            .collect();
        if !important.is_empty() {
            self.set_keyword(&a.user_id, &important, IMPORTANT, true, false).await?;
        }
        Ok(important)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(total: u32, read: u32, replied: u32, important: u32, sent: u32) -> SenderStats {
        SenderStats {
            total,
            read,
            replied,
            important,
            sent,
        }
    }

    #[test]
    fn leave_one_out() {
        let s = st(4, 3, 1, 2, 0).without(true, false, true);
        assert_eq!(s, st(3, 2, 1, 1, 0));
        assert_eq!(st(0, 0, 0, 0, 0).without(true, true, true), st(0, 0, 0, 0, 0));
        assert_eq!(st(0, 0, 0, 0, 0).read_share(), 0.0);
    }

    #[test]
    fn nextcloud_rules() {
        assert!(rules(&st(10, 8, 0, 0, 0), 10), "mostly read");
        assert!(rules(&st(10, 0, 2, 0, 0), 10), "replied to");
        assert!(rules(&st(10, 0, 0, 4, 0), 10), "marked important");
        assert!(rules(&st(10, 0, 0, 0, 2), 10), "the user writes to them");
        assert!(!rules(&st(10, 5, 1, 3, 1), 10));
        assert!(!rules(&st(0, 0, 0, 0, 0), 0), "unknown senders are not important");
    }

    #[test]
    fn feature_vector() {
        let f = features(&st(4, 2, 1, 1, 3), 6, "Re: Quarterly report numbers");
        assert_eq!(f.len(), FEATURES);
        assert_eq!(&f[..4], &[0.5, 0.25, 0.25, 0.5]);
        assert_eq!(f[4], 1.0);
        let words: f64 = f[SENDER_FEATURES..].iter().map(|x| x * x).sum();
        assert!((words - 1.0).abs() < 1e-9, "subject buckets are L2-normalized");
        assert_eq!(subject_words("Re: Fwd: the Q3 report"), vec!["report"]);
        assert_eq!(
            features(&st(4, 2, 1, 1, 3), 6, "Quarterly report"),
            features(&st(4, 2, 1, 1, 3), 6, "Quarterly report")
        );
    }

    /// Senders whose mail the user reads and answers are important; a newsletter whose
    /// subjects say "sale" is not, whatever its read share.
    #[test]
    fn learns_from_behaviour() {
        let mut xs = Vec::new();
        let mut ys = Vec::new();
        for i in 0..60 {
            xs.push(features(&st(20, 18, 8, 10, 4), 30, &format!("Project status {i}")));
            ys.push(true);
            xs.push(features(&st(40, 30, 0, 0, 0), 30, &format!("Big sale this week {i}")));
            ys.push(false);
            xs.push(features(&st(10, 1, 0, 0, 0), 30, &format!("Your receipt {i}")));
            ys.push(false);
        }
        let m = Model::train(&xs, &ys);
        assert!(accuracy(&xs, &ys, |x| m.predict(x)) > 0.95);
        assert!(m.predict(&features(&st(12, 11, 5, 6, 2), 30, "Project kickoff")));
        assert!(!m.predict(&features(&st(40, 30, 0, 0, 0), 30, "Flash sale")));
        // Marking a sender important moves its next message's probability up.
        let before = m.prob(&features(&st(10, 5, 0, 0, 0), 30, "Hello"));
        let after = m.prob(&features(&st(10, 5, 0, 5, 0), 30, "Hello"));
        assert!(after > before);
        // A trained model round-trips through its stored form.
        let back: Model = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert!(xs.iter().all(|x| (back.prob(x) - m.prob(x)).abs() < 1e-9));
    }
}
