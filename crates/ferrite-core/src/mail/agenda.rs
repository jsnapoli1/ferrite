//! Events and tasks from messages: a prefilled draft (title from the subject, the text as
//! description, the other people of the message as attendees) the reader edits before saving,
//! and creating tasks (VTODO) in calendars that hold them.

use super::parse::{self, Addr};
use crate::calendar_sharing::CalendarRef;
use crate::groupware::{DavObject, KIND_CALENDAR, escape};
use crate::util::{new_id, now};
use crate::{Core, Error, Result};
use chrono::{DateTime, Duration, DurationRound, Utc};
use serde::{Deserialize, Serialize};

/// Text of the message kept in a draft's description.
const DESCRIPTION_CHARS: usize = 2000;

/// A prefilled event or task.
#[derive(Debug, Clone, Serialize)]
pub struct AgendaDraft {
    pub summary: String,
    pub description: String,
    /// The next full hour, one hour long.
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// People of the message other than the user (for invitations).
    pub attendees: Vec<Addr>,
    /// When the message was sent.
    pub message_date: DateTime<Utc>,
}

/// "Re: Fwd: Plans" → "Plans".
pub fn clean_subject(s: &str) -> String {
    let mut t = s.trim();
    loop {
        let lower = t.to_ascii_lowercase();
        let cut = ["re:", "fwd:", "fw:", "aw:", "wg:", "sv:", "vs:", "tr:"]
            .iter()
            .find(|p| lower.starts_with(**p))
            .map(|p| p.len());
        match cut {
            Some(n) => t = t[n..].trim_start(),
            None => break,
        }
    }
    t.to_string()
}

/// A task to create.
#[derive(Debug, Clone, Deserialize)]
pub struct TaskInput {
    pub summary: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub due: Option<DateTime<Utc>>,
    /// Calendar uri (default: the first own calendar that holds tasks).
    #[serde(default)]
    pub calendar: Option<String>,
}

/// The iCalendar text of a new task.
pub fn build_task(uid: &str, t: &TaskInput) -> String {
    let stamp = |d: &DateTime<Utc>| d.format("%Y%m%dT%H%M%SZ").to_string();
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".into(),
        "PRODID:-//Ferrite//Ferrite//EN".into(),
        "CALSCALE:GREGORIAN".into(),
        "BEGIN:VTODO".into(),
        format!("UID:{uid}"),
        format!("DTSTAMP:{}", stamp(&now())),
        format!("CREATED:{}", stamp(&now())),
        format!("SUMMARY:{}", escape(t.summary.trim())),
        "STATUS:NEEDS-ACTION".into(),
    ];
    if let Some(d) = t.description.as_deref().filter(|d| !d.trim().is_empty()) {
        lines.push(format!("DESCRIPTION:{}", escape(d)));
    }
    if let Some(due) = &t.due {
        lines.push(format!("DUE:{}", stamp(due)));
    }
    lines.push("END:VTODO".into());
    lines.push("END:VCALENDAR".into());
    lines.iter().map(|l| crate::groupware::fold(l)).collect()
}

impl Core {
    /// A draft event or task from a message.
    pub async fn mail_agenda_draft(&self, user_id: &str, message_id: &str) -> Result<AgendaDraft> {
        let view = self.mail_message(user_id, message_id).await?;
        let (a, _) = self.message_row(user_id, message_id).await?;
        let m = &view.summary;
        let mut text: String = view.text.trim().chars().take(DESCRIPTION_CHARS).collect();
        if view.text.trim().chars().count() > DESCRIPTION_CHARS {
            text.push('…');
        }
        let mut attendees: Vec<Addr> = Vec::new();
        for x in std::iter::once(m.from()).chain(m.to_addrs.iter().cloned()).chain(m.cc_addrs.iter().cloned()) {
            if x.address.is_empty() || x.address.eq_ignore_ascii_case(&a.email) || attendees.iter().any(|y| y.address.eq_ignore_ascii_case(&x.address)) {
                continue;
            }
            attendees.push(x);
        }
        let start = (now() + Duration::hours(1)).duration_trunc(Duration::hours(1)).unwrap_or_else(|_| now());
        let summary = clean_subject(&m.subject);
        Ok(AgendaDraft {
            summary: if summary.is_empty() { "(no subject)".into() } else { summary },
            description: format!("{text}\n\n— {}, {}", parse::Addr::display(&m.from()), m.date.format("%a, %-d %b %Y %H:%M UTC")),
            start,
            end: start + Duration::hours(1),
            attendees,
            message_date: m.date,
        })
    }

    /// Creates a task (VTODO) in one of the user's calendars that hold tasks.
    pub async fn create_task(&self, user_id: &str, t: &TaskInput) -> Result<DavObject> {
        if t.summary.trim().is_empty() {
            return Err(Error::bad("the task needs a title"));
        }
        let actor = self.user(user_id).await?;
        let holds_tasks = |c: &CalendarRef| c.collection.components.split(',').any(|x| x == "VTODO");
        let cal = match t.calendar.as_deref() {
            Some(uri) => {
                let c = self.writable_calendar(user_id, Some(uri)).await?;
                if !holds_tasks(&c) {
                    return Err(Error::bad("that calendar does not hold tasks"));
                }
                c
            }
            None => self
                .user_calendars(user_id)
                .await?
                .into_iter()
                .find(|c| c.is_owner() && holds_tasks(c))
                .ok_or_else(|| Error::bad("none of your calendars holds tasks"))?,
        };
        if cal.collection.kind != KIND_CALENDAR {
            return Err(Error::NotFound);
        }
        let uid = format!("{}@ferrite", new_id());
        let saved = self
            .save_calendar_object(&actor, &cal, &format!("{uid}.ics"), &build_task(&uid, t), None, true)
            .await?;
        Ok(saved.object)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subjects_and_tasks() {
        assert_eq!(clean_subject("Re: Fwd: AW: Plans for Q4"), "Plans for Q4");
        assert_eq!(clean_subject("Review"), "Review");
        let ics = build_task(
            "t1@ferrite",
            &TaskInput {
                summary: "Send the report; today".into(),
                description: Some("Line 1\nLine 2".into()),
                due: Some("2026-11-02T17:00:00Z".parse().unwrap()),
                calendar: None,
            },
        );
        let p = crate::groupware::parse_object(&ics).unwrap();
        assert_eq!((p.component.as_str(), p.summary.as_str()), ("VTODO", "Send the report; today"));
        assert_eq!(p.description, "Line 1\nLine 2");
        assert_eq!(p.ends_at.unwrap().to_rfc3339(), "2026-11-02T17:00:00+00:00");
    }
}
