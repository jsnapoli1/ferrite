//! The user's absence (out of office): a period of whole days in the user's time zone with a
//! status and a message. Other users see it (the directory and contact info show it while it
//! runs), and mail autoresponders that follow it are switched on for its days.

use crate::util::now;
use crate::{Core, Error, Result};
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct Absence {
    /// First day away (YYYY-MM-DD), inclusive.
    pub first_day: String,
    /// Last day away, inclusive.
    pub last_day: String,
    /// IANA time zone the days are in.
    pub timezone: String,
    /// Short status such as "On vacation".
    pub status: String,
    /// The out-of-office message.
    pub message: String,
    /// Who to contact instead.
    pub replacement: String,
    #[serde(default = "now")]
    pub updated_at: DateTime<Utc>,
}

/// What the user enters.
#[derive(Debug, Clone, Deserialize)]
pub struct AbsenceInput {
    pub first_day: String,
    pub last_day: String,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub replacement: String,
}

fn day(s: &str, what: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").map_err(|_| Error::bad(format!("{what} must be a date (YYYY-MM-DD)")))
}

impl Absence {
    fn zone(&self) -> crate::recur::Zone {
        crate::recur::Zone::from_tzid(&self.timezone)
    }

    /// The first moment of the first day, in UTC.
    pub fn starts_at(&self) -> DateTime<Utc> {
        let d = NaiveDate::parse_from_str(&self.first_day, "%Y-%m-%d").unwrap_or_default();
        self.zone().to_utc(d.and_time(NaiveTime::MIN))
    }

    /// The last second of the last day, in UTC.
    pub fn ends_at(&self) -> DateTime<Utc> {
        let d = NaiveDate::parse_from_str(&self.last_day, "%Y-%m-%d").unwrap_or_default();
        let t = NaiveTime::from_hms_opt(23, 59, 59).expect("valid time");
        self.zone().to_utc(d.and_time(t))
    }

    /// The absence is running at `t`.
    pub fn active_at(&self, t: DateTime<Utc>) -> bool {
        self.starts_at() <= t && t <= self.ends_at()
    }
}

impl Core {
    pub async fn absence(&self, user_id: &str) -> Result<Option<Absence>> {
        Ok(
            sqlx::query_as("SELECT first_day, last_day, timezone, status, message, replacement, updated_at FROM user_absences WHERE user_id = ?")
                .bind(user_id)
                .fetch_optional(&self.db)
                .await?,
        )
    }

    /// The absence when it is running now (what others see).
    pub async fn current_absence(&self, user_id: &str) -> Result<Option<Absence>> {
        Ok(self.absence(user_id).await?.filter(|a| a.active_at(now())))
    }

    /// Sets the absence and updates the autoresponders that follow it. Returns the absence
    /// and the accounts whose autoresponder could not be updated.
    pub async fn set_absence(&self, user_id: &str, input: &AbsenceInput) -> Result<(Absence, Vec<(String, String)>)> {
        let first = day(&input.first_day, "the first day")?;
        let last = day(&input.last_day, "the last day")?;
        if last < first {
            return Err(Error::bad("the last day must not be before the first day"));
        }
        let tz = input.timezone.as_deref().map(str::trim).filter(|t| !t.is_empty()).unwrap_or("UTC");
        if tz != "UTC" && tz.parse::<chrono_tz::Tz>().is_err() {
            return Err(Error::bad(format!("unknown time zone {tz:?}")));
        }
        if input.status.chars().count() > 100 {
            return Err(Error::bad("the status can be at most 100 characters"));
        }
        if input.message.len() > 10_000 {
            return Err(Error::bad("the message is too long"));
        }
        let a = Absence {
            first_day: first.format("%Y-%m-%d").to_string(),
            last_day: last.format("%Y-%m-%d").to_string(),
            timezone: tz.to_string(),
            status: input.status.trim().to_string(),
            message: input.message.trim_end().to_string(),
            replacement: input.replacement.trim().to_string(),
            updated_at: now(),
        };
        sqlx::query(
            "INSERT INTO user_absences (user_id, first_day, last_day, timezone, status, message, replacement, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(user_id) DO UPDATE SET first_day = excluded.first_day, last_day = excluded.last_day, timezone = excluded.timezone,
             status = excluded.status, message = excluded.message, replacement = excluded.replacement, updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(&a.first_day)
        .bind(&a.last_day)
        .bind(&a.timezone)
        .bind(&a.status)
        .bind(&a.message)
        .bind(&a.replacement)
        .bind(a.updated_at)
        .execute(&self.db)
        .await?;
        let failed = self.apply_absence(user_id).await?;
        Ok((a, failed))
    }

    /// Removes the absence and turns off the autoresponders that follow it.
    pub async fn clear_absence(&self, user_id: &str) -> Result<Vec<(String, String)>> {
        sqlx::query("DELETE FROM user_absences WHERE user_id = ?")
            .bind(user_id)
            .execute(&self.db)
            .await?;
        self.apply_absence(user_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_are_whole_days_in_the_users_zone() {
        let a = Absence {
            first_day: "2026-10-26".into(),
            last_day: "2026-10-30".into(),
            timezone: "Europe/Berlin".into(),
            status: String::new(),
            message: String::new(),
            replacement: String::new(),
            updated_at: now(),
        };
        // 26 Oct 2026 is after the change to CET (UTC+1).
        assert_eq!(a.starts_at().to_rfc3339(), "2026-10-25T23:00:00+00:00");
        assert_eq!(a.ends_at().to_rfc3339(), "2026-10-30T22:59:59+00:00");
        assert!(a.active_at("2026-10-28T12:00:00Z".parse().unwrap()));
        assert!(!a.active_at("2026-10-30T23:30:00Z".parse().unwrap()));
    }
}
