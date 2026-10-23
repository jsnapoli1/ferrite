//! Senders and recipients as people: who an address is in the user's contacts and among
//! Ferrite's users (with their absence while it runs), how much mail was exchanged, and adding
//! an address to a new or an existing contact.

use crate::groupware::{ContactInput, ContactView};
use crate::{Core, Error, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;

/// A Ferrite user with this address.
#[derive(Debug, Clone, Serialize)]
pub struct UserBrief {
    pub username: String,
    pub display_name: String,
    /// Their absence, while it runs.
    pub absence: Option<AbsenceBrief>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AbsenceBrief {
    pub status: String,
    pub message: String,
    pub last_day: String,
    pub replacement: String,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct RecentMessage {
    pub id: String,
    pub subject: String,
    pub date: DateTime<Utc>,
    pub from_name: String,
    pub from_addr: String,
}

/// What Ferrite knows about an address (the recipient info pane and the reader's sender).
#[derive(Debug, Clone, Serialize)]
pub struct ContactInfo {
    pub email: String,
    /// The best known name: the contact's, the user's, else from mail.
    pub name: String,
    pub contact: Option<ContactView>,
    pub user: Option<UserBrief>,
    /// Messages received from this address (cached mail of all accounts).
    pub received: i64,
    /// Messages sent to it.
    pub sent: i64,
    pub last_contact: Option<DateTime<Utc>>,
    /// The latest messages from or to it.
    pub recent: Vec<RecentMessage>,
}

fn same(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

impl Core {
    /// The contact holding `email` in the user's address books.
    pub async fn contact_by_email(&self, user_id: &str, email: &str) -> Result<Option<ContactView>> {
        let email = email.trim();
        Ok(self
            .contacts(user_id, Some(email))
            .await?
            .into_iter()
            .find(|c| c.emails.iter().any(|e| same(e.trim_start_matches("mailto:"), email))))
    }

    /// Everything known about an address, for one user.
    pub async fn mail_contact_info(&self, user_id: &str, email: &str) -> Result<ContactInfo> {
        let email = email.trim().to_string();
        if super::parse::Addr::parse(&email).is_none() {
            return Err(Error::bad(format!("{email:?} is not an email address")));
        }
        let contact = self.contact_by_email(user_id, &email).await?;
        let user = match self.user_by_email(&email).await? {
            Some(u) => {
                let absence = self.current_absence(&u.id).await?.map(|a| AbsenceBrief {
                    status: a.status,
                    message: a.message,
                    last_day: a.last_day,
                    replacement: a.replacement,
                });
                Some(UserBrief {
                    username: u.username,
                    display_name: u.display_name,
                    absence,
                })
            }
            None => None,
        };
        let (received, last_in): (i64, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT COUNT(*), MAX(m.date) FROM mail_messages m JOIN mail_accounts a ON a.id = m.account_id
             WHERE a.user_id = ? AND lower(m.from_addr) = lower(?)",
        )
        .bind(user_id)
        .bind(&email)
        .fetch_one(&self.db)
        .await?;
        let (sent, last_out): (i64, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT COUNT(*), MAX(m.date) FROM mail_messages m JOIN mail_accounts a ON a.id = m.account_id
             WHERE a.user_id = ? AND lower(m.from_addr) = lower(a.email)
             AND (EXISTS (SELECT 1 FROM json_each(m.to_addrs) WHERE lower(json_extract(value, '$.address')) = lower(?))
               OR EXISTS (SELECT 1 FROM json_each(m.cc_addrs) WHERE lower(json_extract(value, '$.address')) = lower(?)))",
        )
        .bind(user_id)
        .bind(&email)
        .bind(&email)
        .fetch_one(&self.db)
        .await?;
        let recent: Vec<RecentMessage> = sqlx::query_as(
            "SELECT m.id, m.subject, m.date, m.from_name, m.from_addr FROM mail_messages m JOIN mail_accounts a ON a.id = m.account_id
             WHERE a.user_id = ? AND (lower(m.from_addr) = lower(?)
               OR (lower(m.from_addr) = lower(a.email) AND EXISTS (SELECT 1 FROM json_each(m.to_addrs) WHERE lower(json_extract(value, '$.address')) = lower(?))))
             GROUP BY COALESCE(m.message_id, m.id) ORDER BY m.date DESC LIMIT 5",
        )
        .bind(user_id)
        .bind(&email)
        .bind(&email)
        .fetch_all(&self.db)
        .await?;
        let mail_name: Option<String> = sqlx::query_scalar(
            "SELECT m.from_name FROM mail_messages m JOIN mail_accounts a ON a.id = m.account_id
             WHERE a.user_id = ? AND lower(m.from_addr) = lower(?) AND m.from_name != '' ORDER BY m.date DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(&email)
        .fetch_optional(&self.db)
        .await?;
        let name = contact
            .as_ref()
            .map(|c| c.name.clone())
            .or_else(|| user.as_ref().map(|u| u.display_name.clone()))
            .or(mail_name)
            .unwrap_or_default();
        Ok(ContactInfo {
            email,
            name,
            contact,
            user,
            received,
            sent,
            last_contact: last_in.max(last_out),
            recent,
        })
    }

    /// Adds an address to a contact: to `contact_id` when given, else to a new contact named
    /// `name` (default: the part before the @).
    pub async fn mail_add_to_contacts(&self, user_id: &str, email: &str, name: Option<&str>, contact_id: Option<&str>) -> Result<ContactView> {
        let addr = super::parse::Addr::parse(email).ok_or_else(|| Error::bad(format!("{email:?} is not an email address")))?;
        match contact_id {
            Some(id) => {
                let c = self.contacts(user_id, None).await?.into_iter().find(|c| c.id == id).ok_or(Error::NotFound)?;
                if c.emails.iter().any(|e| same(e, &addr.address)) {
                    return Ok(c);
                }
                let mut emails = c.emails.clone();
                emails.push(addr.address.clone());
                self.update_contact(
                    user_id,
                    id,
                    &ContactInput {
                        name: c.name.clone(),
                        emails,
                        phones: c.phones.clone(),
                        org: Some(c.org.clone()),
                        note: Some(c.note.clone()),
                        addressbook: None,
                    },
                )
                .await
            }
            None => {
                if let Some(existing) = self.contact_by_email(user_id, &addr.address).await? {
                    return Ok(existing);
                }
                let name = name
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map(String::from)
                    .or_else(|| (!addr.name.is_empty()).then(|| addr.name.clone()))
                    .unwrap_or_else(|| addr.address.split('@').next().unwrap_or(&addr.address).to_string());
                self.create_contact(
                    user_id,
                    &ContactInput {
                        name,
                        emails: vec![addr.address.clone()],
                        ..Default::default()
                    },
                )
                .await
            }
        }
    }
}
