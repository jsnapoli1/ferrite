//! Meeting invitations in mail (iMIP, RFC 6047, carrying iTIP, RFC 5546).
//!
//! A message with a `text/calendar` part (or an `.ics` attachment) is shown as an invitation
//! card: what, when (in the viewer's zone), who, and what else is in the user's calendars
//! then. What the user can do depends on the method:
//!
//! - **REQUEST** (the user is invited): accept, tentative, decline or propose a new time. The
//!   user's calendar copy is updated and the iTIP REPLY (or COUNTER) goes to the organizer by
//!   mail through the user's own account. When the organizer is a Ferrite user whose
//!   invitation is already in the calendar, the answer goes through server-side scheduling
//!   instead (no mail needed).
//! - **REPLY** (the user organizes the event in Ferrite and a guest answered from their own
//!   mail client): the guest's status is applied to the organizer's copy through `scheduling`,
//!   as if they had answered on Ferrite's invitation page.
//! - **CANCEL**: the user's copy is marked cancelled (or the cancelled instances removed).
//! - **COUNTER** (a guest proposes another time): the organizer accepts it (the event moves and
//!   attendees are updated) or declines it (DECLINECOUNTER to the guest).
//!
//! New mail in the inbox is processed automatically: replies always; updates and
//! cancellations of events already in the calendar when the account allows it (default on);
//! new invitations are put in the calendar as tentative only when the account asks for that.
//! A message counts only when it comes from the right party (the organizer for REQUEST and
//! CANCEL, the attendee for REPLY and COUNTER), checked against From, Reply-To, Sender and
//! SENT-BY; others are shown with a warning and applied only on request. Stale messages
//! (lower SEQUENCE than the calendar copy, or an older DTSTAMP than the last applied reply)
//! are ignored. Each message is processed once (by Message-ID).

use super::Account;
use super::delegation::MailRight;
use crate::groupware::{Collection, DavObject, EventInput, KIND_CALENDAR};
use crate::itip::{self, Component, Party, Prop};
use crate::mailer::{self, CalendarPart, OutgoingMail};
use crate::scheduling::{describe_when, event_link};
use crate::util::{new_id, now};
use crate::{Core, Error, Result};
use chrono::{DateTime, Utc};
use mail_parser::{MessageParser, MimeHeaders, PartType};
use serde::Serialize;
use std::collections::BTreeMap;

/// A calendar object found in a message.
#[derive(Debug, Clone)]
pub struct ItipPart {
    /// REQUEST, REPLY, CANCEL, COUNTER, DECLINECOUNTER, ADD, REFRESH or PUBLISH.
    pub method: String,
    pub cal: Component,
}

/// Who sent the message, as its headers say.
#[derive(Debug, Clone, Default)]
pub struct Senders {
    pub from: Option<String>,
    pub reply_to: Vec<String>,
    pub sender: Option<String>,
}

impl Senders {
    fn all(&self) -> impl Iterator<Item = String> + '_ {
        self.from
            .iter()
            .chain(self.reply_to.iter())
            .chain(self.sender.iter())
            .map(|a| itip::normalize_address(a))
    }

    /// The message comes from `party` (or someone they authorized with SENT-BY).
    fn is(&self, email: &str, sent_by: Option<&str>) -> bool {
        self.all().any(|a| a == email || sent_by.is_some_and(|s| itip::normalize_address(s) == a))
    }
}

fn is_calendar(mime: &str, name: Option<&str>) -> bool {
    let m = mime.to_ascii_lowercase();
    m == "text/calendar" || m == "application/ics" || name.is_some_and(|n| n.to_ascii_lowercase().ends_with(".ics"))
}

fn part_text(p: &mail_parser::MessagePart) -> Option<String> {
    match &p.body {
        PartType::Text(t) | PartType::Html(t) => Some(t.to_string()),
        PartType::Binary(b) | PartType::InlineBinary(b) => Some(String::from_utf8_lossy(b).into_owned()),
        _ => None,
    }
}

/// The iTIP objects of a message (identical copies, as in the alternative part and the
/// attachment of one invitation, once).
pub fn itip_parts(raw: &[u8]) -> Vec<ItipPart> {
    let Some(m) = MessageParser::default().parse(raw) else {
        return Vec::new();
    };
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for p in m.parts.iter() {
        let Some(ct) = p.content_type() else { continue };
        let mime = match ct.subtype() {
            Some(s) => format!("{}/{s}", ct.ctype()),
            None => ct.ctype().to_string(),
        };
        if !is_calendar(&mime, p.attachment_name()) {
            continue;
        }
        let Some(text) = part_text(p) else { continue };
        let Ok(cal) = Component::parse(&text) else { continue };
        if cal.items().next().is_none() {
            continue;
        }
        let key = cal.to_ics();
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let method = cal
            .value("METHOD")
            .map(|m| m.trim().to_ascii_uppercase())
            .or_else(|| ct.attribute("method").map(|m| m.trim().to_ascii_uppercase()))
            .unwrap_or_else(|| "PUBLISH".into());
        out.push(ItipPart { method, cal });
    }
    out
}

/// The indexes (among the message's attachments) of calendar files that can be imported.
pub fn ics_attachments(raw: &[u8]) -> Vec<usize> {
    let Some(m) = MessageParser::default().parse(raw) else {
        return Vec::new();
    };
    m.attachments()
        .enumerate()
        .filter(|(_, p)| {
            let mime = p
                .content_type()
                .map(|ct| match ct.subtype() {
                    Some(s) => format!("{}/{s}", ct.ctype()),
                    None => ct.ctype().to_string(),
                })
                .unwrap_or_default();
            is_calendar(&mime, p.attachment_name())
        })
        .map(|(i, _)| i)
        .collect()
}

/// The senders of a message.
pub fn senders(raw: &[u8]) -> Senders {
    let Some(m) = MessageParser::default().parse(raw) else {
        return Senders::default();
    };
    let first = |a: Option<&mail_parser::Address>| a.and_then(|a| a.first()).and_then(|x| x.address()).map(str::to_string);
    Senders {
        from: first(m.from()),
        reply_to: m
            .reply_to()
            .map(|a| a.iter().filter_map(|x| x.address().map(str::to_string)).collect())
            .unwrap_or_default(),
        sender: first(m.sender()),
    }
}

/// Key of a component within its series ("" for the master), like `itip` uses.
fn instance_key(c: &Component) -> String {
    c.get("RECURRENCE-ID").map(|p| p.value.trim().to_string()).unwrap_or_default()
}

fn rid_instant(p: &Prop) -> Option<DateTime<Utc>> {
    crate::recur::parse_time(p.value.trim(), &p.params).map(|t| t.utc())
}

/// The statuses a REPLY gives, keyed like the components of `target` (RECURRENCE-IDs written
/// in another zone or form are matched by the instant they name).
fn reply_statuses(reply: &Component, attendee: &str, target: &Component) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for c in reply.items() {
        let Some(ps) = c
            .all("ATTENDEE")
            .find(|p| p.address() == attendee)
            .and_then(|p| p.param("PARTSTAT").map(str::to_ascii_uppercase))
        else {
            continue;
        };
        if !matches!(ps.as_str(), "ACCEPTED" | "TENTATIVE" | "DECLINED" | "DELEGATED" | "NEEDS-ACTION") {
            continue;
        }
        let key = match c.get("RECURRENCE-ID") {
            None => String::new(),
            Some(rid) => {
                let want = rid_instant(rid);
                target
                    .items()
                    .filter_map(|t| t.get("RECURRENCE-ID").map(|r| (r, instance_key(t))))
                    .find(|(r, k)| *k == rid.value.trim() || (want.is_some() && rid_instant(r) == want))
                    .map(|(_, k)| k)
                    .unwrap_or_else(|| rid.value.trim().to_string())
            }
        };
        out.insert(key, ps);
    }
    out
}

fn dtstamp(cal: &Component) -> String {
    cal.items()
        .filter_map(|c| c.value("DTSTAMP"))
        .map(|v| v.trim().to_string())
        .max()
        .unwrap_or_default()
}

fn object_uri(uid: &str) -> String {
    let clean: String = uid
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "@._-".contains(c) { c } else { '-' })
        .take(200)
        .collect();
    format!("{}.ics", clean.trim_matches('.'))
}

/// An event in the user's calendar with the same UID.
#[derive(Debug, Clone, Serialize)]
pub struct ExistingEvent {
    pub id: String,
    pub calendar: String,
    pub calendar_name: String,
    pub sequence: i64,
    pub cancelled: bool,
    pub my_partstat: Option<String>,
    pub link: String,
}

/// Something else in the user's calendars at the invitation's time.
#[derive(Debug, Clone, Serialize)]
pub struct Conflict {
    pub id: String,
    pub summary: String,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub calendar_name: String,
}

/// What a processed (or answered) message led to.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ImipLog {
    pub status: String,
    pub detail: String,
    pub processed_at: DateTime<Utc>,
}

/// The invitation card.
#[derive(Debug, Clone, Serialize)]
pub struct ImipView {
    pub method: String,
    pub uid: String,
    pub sequence: i64,
    pub summary: String,
    pub description: String,
    pub location: String,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub all_day: bool,
    pub tzid: Option<String>,
    pub recurring: bool,
    /// In the event's own time zone (the client shows the viewer's zone itself).
    pub when: String,
    pub organizer: Option<Party>,
    pub attendees: Vec<Party>,
    /// The user's address on the attendee list.
    pub me: Option<String>,
    /// The user organizes the event (a REPLY or COUNTER to their own event).
    pub organizer_is_me: bool,
    /// The user's answer: from their calendar copy, else from the message.
    pub my_partstat: Option<String>,
    pub existing: Option<ExistingEvent>,
    /// The calendar holds a newer version than this message.
    pub outdated: bool,
    pub conflicts: Vec<Conflict>,
    /// The message comes from the party the method requires.
    pub sender_ok: bool,
    /// For REPLY/COUNTER: the attendee who sent it and what they answered or proposed.
    pub from_attendee: Option<Party>,
    pub proposed_start: Option<DateTime<Utc>>,
    pub proposed_end: Option<DateTime<Utc>>,
    pub comment: String,
    pub processed: Option<ImipLog>,
    /// The user can answer (a REQUEST they are invited to that is not cancelled).
    pub can_respond: bool,
}

/// Result of applying a message.
#[derive(Debug, Clone, Serialize)]
pub struct ImipOutcome {
    pub status: &'static str,
    pub detail: String,
    pub event_id: Option<String>,
}

impl ImipOutcome {
    fn new(status: &'static str, detail: impl Into<String>, event_id: Option<String>) -> Self {
        ImipOutcome {
            status,
            detail: detail.into(),
            event_id,
        }
    }
}

fn response_word(ps: &str) -> &'static str {
    match ps {
        "ACCEPTED" => "Accepted",
        "TENTATIVE" => "Tentatively accepted",
        "DECLINED" => "Declined",
        _ => "Answered",
    }
}

impl Core {
    /// The user's own calendar object with `uid`.
    async fn own_object_by_uid(&self, user_id: &str, uid: &str) -> Result<Option<(Collection, DavObject)>> {
        for c in self.dav_collections(user_id, KIND_CALENDAR).await? {
            let found: Option<DavObject> = sqlx::query_as(
                "SELECT id, collection_id, uri, uid, etag, component, summary, starts_at, ends_at, all_day, recurring, data, modified_at
                 FROM dav_objects WHERE collection_id = ? AND uid = ?",
            )
            .bind(&c.id)
            .bind(uid)
            .fetch_optional(&self.db)
            .await?;
            if let Some(o) = found {
                return Ok(Some((c, o)));
            }
        }
        Ok(None)
    }

    /// The addresses that are "the user" for an account: the account's, then their own.
    async fn my_addresses(&self, a: &Account) -> Result<Vec<String>> {
        let user = self.user(&a.user_id).await?;
        let mut out = vec![itip::normalize_address(&a.email)];
        if let Some(e) = user.email.as_deref().map(itip::normalize_address) {
            if !out.contains(&e) {
                out.push(e);
            }
        }
        Ok(out)
    }

    async fn imip_log(&self, account_id: &str, message_id: &str) -> Result<Option<ImipLog>> {
        Ok(
            sqlx::query_as("SELECT status, detail, processed_at FROM mail_imip_log WHERE account_id = ? AND message_id = ?")
                .bind(account_id)
                .bind(message_id)
                .fetch_optional(&self.db)
                .await?,
        )
    }

    async fn record_imip(&self, a: &Account, message_key: &str, part: &ItipPart, o: &ImipOutcome) -> Result<()> {
        self.log_imip(a, message_key, part, o, false).await
    }

    /// Logs what a message led to. The user's own actions replace the entry; automatic
    /// processing (`automatic`) never overwrites one (the user may have acted meanwhile).
    async fn log_imip(&self, a: &Account, message_key: &str, part: &ItipPart, o: &ImipOutcome, automatic: bool) -> Result<()> {
        let sql = if automatic {
            "INSERT INTO mail_imip_log (account_id, message_id, method, uid, status, detail, processed_at) VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(account_id, message_id) DO NOTHING"
        } else {
            "INSERT INTO mail_imip_log (account_id, message_id, method, uid, status, detail, processed_at) VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(account_id, message_id) DO UPDATE SET method = excluded.method, uid = excluded.uid, status = excluded.status,
             detail = excluded.detail, processed_at = excluded.processed_at"
        };
        sqlx::query(sql)
            .bind(&a.id)
            .bind(message_key)
            .bind(&part.method)
            .bind(itip::uid(&part.cal).unwrap_or_default())
            .bind(o.status)
            .bind(&o.detail)
            .bind(now())
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// The message's iTIP part, its raw bytes' senders, and the key it is logged under.
    /// A message's account (checked for `right`), its key, iTIP part and senders.
    async fn imip_of(&self, user_id: &str, message_id: &str, right: MailRight) -> Result<(Account, String, ItipPart, Senders)> {
        let (a, m) = self.mail_message_access(user_id, message_id, right).await?;
        let (_, raw) = self.mail_raw(user_id, message_id).await?;
        let part = itip_parts(&raw)
            .into_iter()
            .next()
            .ok_or_else(|| Error::bad("this message has no calendar data"))?;
        let key = m.message_id.clone().unwrap_or_else(|| m.id.clone());
        Ok((a, key, part, senders(&raw)))
    }

    /// The invitation card of a message (None when it carries no calendar data).
    pub async fn mail_imip(&self, user_id: &str, message_id: &str) -> Result<Option<ImipView>> {
        let (a, m) = self.message_row(user_id, message_id).await?;
        let (_, raw) = self.mail_raw(user_id, message_id).await?;
        let Some(part) = itip_parts(&raw).into_iter().next() else {
            return Ok(None);
        };
        let key = m.message_id.clone().unwrap_or_else(|| m.id.clone());
        let from = senders(&raw);
        let cal = &part.cal;
        let uid = itip::uid(cal).unwrap_or_default();
        let parsed = crate::groupware::parse_object(&cal.to_ics()).unwrap_or_default();
        let organizer = itip::organizer(cal);
        let attendees = itip::attendees(cal);
        let mine = self.my_addresses(&a).await?;
        let me = mine.iter().find(|e| attendees.iter().any(|x| &x.email == *e)).cloned();
        let organizer_is_me = organizer.as_ref().is_some_and(|o| mine.contains(&o.email));
        let existing = match self.own_object_by_uid(user_id, &uid).await? {
            Some((c, o)) => {
                let copy = Component::parse(&o.data).ok();
                Some(ExistingEvent {
                    link: event_link(&o),
                    id: o.id.clone(),
                    calendar: c.uri.clone(),
                    calendar_name: c.name.clone(),
                    sequence: copy.as_ref().map(itip::sequence).unwrap_or(0),
                    cancelled: copy.as_ref().is_some_and(itip::is_cancelled),
                    my_partstat: me.as_deref().and_then(|e| copy.as_ref().and_then(|c| itip::partstat_of(c, e))),
                })
            }
            None => None,
        };
        let sequence = itip::sequence(cal);
        let outdated = existing.as_ref().is_some_and(|e| e.sequence > sequence);
        let organizer_sent_by = cal
            .items()
            .find_map(|c| c.get("ORGANIZER"))
            .and_then(|p| p.param("SENT-BY").map(str::to_string));
        let reply_party = cal
            .items()
            .find_map(|c| c.get("ATTENDEE"))
            .map(|p| (p.address(), p.param("SENT-BY").map(str::to_string)));
        let sender_ok = match part.method.as_str() {
            "REPLY" | "COUNTER" | "REFRESH" => reply_party.as_ref().is_some_and(|(e, sb)| from.is(e, sb.as_deref())),
            _ => organizer.as_ref().is_some_and(|o| from.is(&o.email, organizer_sent_by.as_deref())),
        };
        let from_attendee = if matches!(part.method.as_str(), "REPLY" | "COUNTER" | "REFRESH") {
            reply_party.and_then(|(e, _)| attendees.iter().find(|x| x.email == e).cloned())
        } else {
            None
        };
        let master = cal.master();
        let comment = master.and_then(|c| c.value("COMMENT")).map(crate::groupware::unescape).unwrap_or_default();
        let (proposed_start, proposed_end) = if part.method == "COUNTER" {
            (parsed.starts_at, parsed.ends_at)
        } else {
            (None, None)
        };
        let mut conflicts = Vec::new();
        if let (Some(s), Some(e)) = (parsed.starts_at, parsed.ends_at) {
            if matches!(part.method.as_str(), "REQUEST" | "ADD" | "PUBLISH" | "COUNTER") && e > s {
                let my_email = self.user(user_id).await?.email.map(|e| itip::normalize_address(&e));
                for ev in self.events_between(user_id, s, e).await? {
                    let declined = ev.my_partstat.as_deref() == Some("DECLINED")
                        || my_email
                            .as_deref()
                            .is_some_and(|me| ev.attendees.iter().any(|x| x.email == me && x.partstat == "DECLINED"));
                    if ev.uid == uid || ev.cancelled || declined || (ev.all_day && !parsed.all_day) || ev.component != "VEVENT" {
                        continue;
                    }
                    conflicts.push(Conflict {
                        id: ev.id,
                        summary: ev.summary,
                        starts_at: ev.starts_at,
                        ends_at: ev.ends_at,
                        calendar_name: ev.calendar_name,
                    });
                }
            }
        }
        let my_partstat = existing
            .as_ref()
            .and_then(|e| e.my_partstat.clone())
            .or_else(|| me.as_deref().and_then(|e| itip::partstat_of(cal, e)));
        let can_respond = part.method == "REQUEST" && me.is_some() && !organizer_is_me && !existing.as_ref().is_some_and(|e| e.cancelled);
        Ok(Some(ImipView {
            method: part.method.clone(),
            uid,
            sequence,
            summary: parsed.summary,
            description: parsed.description,
            location: parsed.location,
            starts_at: parsed.starts_at,
            ends_at: parsed.ends_at,
            all_day: parsed.all_day,
            tzid: parsed.tzid,
            recurring: parsed.recurring,
            when: describe_when(cal),
            organizer,
            attendees,
            me,
            organizer_is_me,
            my_partstat,
            existing,
            outdated,
            conflicts,
            sender_ok,
            from_attendee,
            proposed_start,
            proposed_end,
            comment,
            processed: self.imip_log(&a.id, &key).await?,
            can_respond,
        }))
    }

    /// Sends a message from the account (its SMTP server) and files it in Sent when the
    /// account keeps sent mail. Returns the Message-ID.
    pub(crate) async fn send_from_account(&self, a: &Account, mut m: OutgoingMail) -> Result<String> {
        m.from = Some(a.sender());
        let msg = mailer::build_message(&m, &a.sender())?;
        let raw = msg.formatted();
        let transport = mailer::transport(&self.smtp_settings(a)?)?;
        let id = mailer::send(&transport, msg).await?;
        if a.save_sent {
            let r = async {
                let folder = self.ensure_role_folder(a, "sent", "Sent").await?;
                let mut conn = self.imap_conn(a).await?;
                conn.append(&folder.name, "(\\Seen)", &raw).await?;
                drop(conn);
                self.refresh_folder_soon(a, &folder.id);
                Ok::<_, Error>(())
            }
            .await;
            if let Err(e) = r {
                tracing::warn!(account = %a.id, error = %e, "saving the calendar reply to Sent failed");
            }
        }
        Ok(id)
    }

    /// Stores the user's copy of an invitation without scheduling (the reply is sent by mail
    /// separately). Returns the stored object.
    async fn store_copy(&self, user_id: &str, at: Option<(Collection, DavObject)>, calendar: Option<&str>, data: &str) -> Result<DavObject> {
        let o = match at {
            Some((c, o)) => self.put_dav_object(&c, &o.uri, data, None, false).await?.0,
            None => {
                let c = match calendar {
                    Some(uri) => self.dav_collection(user_id, KIND_CALENDAR, uri).await?,
                    None => self
                        .dav_collections(user_id, KIND_CALENDAR)
                        .await?
                        .into_iter()
                        .find(|c| c.components.split(',').any(|x| x == "VEVENT"))
                        .ok_or(Error::NotFound)?,
                };
                let uid = crate::groupware::parse_object(data)?.uid;
                match self.put_dav_object(&c, &object_uri(&uid), data, None, true).await {
                    Ok((o, _)) => o,
                    // Stored meanwhile (automatic processing of the same invitation): update it.
                    Err(Error::PreconditionFailed) | Err(Error::Conflict(_)) => match self.own_object_by_uid(user_id, &uid).await? {
                        Some((c, o)) => self.put_dav_object(&c, &o.uri, data, None, false).await?.0,
                        None => self.put_dav_object(&c, &format!("{}.ics", new_id()), data, None, true).await?.0,
                    },
                    Err(e) => return Err(e),
                }
            }
        };
        self.record_calendar_author(&o.id, user_id).await?;
        Ok(o)
    }

    /// The organizer's current data merged into the user's copy (their alarms and, unless the
    /// event was rescheduled, their answer are kept).
    fn merged_copy(request: &Component, prev: Option<&Component>, me: &str) -> Component {
        let data = itip::attendee_copy(request, prev);
        let mut next = Component::parse(&data).expect("attendee copies parse");
        if let Some(p) = prev {
            if !itip::is_reschedule(p, &next) {
                if let Some(ps) = itip::partstat_of(p, me) {
                    itip::set_partstat(&mut next, me, &ps);
                }
            }
        }
        next
    }

    /// Answers an invitation received by mail: updates the user's calendar and sends the
    /// REPLY to the organizer. `calendar` picks the calendar for a new copy.
    pub async fn mail_imip_respond(
        &self,
        user_id: &str,
        message_id: &str,
        partstat: &str,
        calendar: Option<&str>,
        comment: Option<&str>,
    ) -> Result<ImipOutcome> {
        if !crate::scheduling::RESPONSES.contains(&partstat) {
            return Err(Error::bad("answer accepted, tentative or declined"));
        }
        let (a, key, part, _) = self.imip_of(user_id, message_id, MailRight::Send).await?;
        if part.method != "REQUEST" {
            return Err(Error::bad("only invitations can be answered"));
        }
        let req = &part.cal;
        let uid = itip::uid(req).ok_or_else(|| Error::bad("the invitation has no UID"))?;
        let org = itip::organizer(req).ok_or_else(|| Error::bad("the invitation has no organizer"))?;
        let mine = self.my_addresses(&a).await?;
        let me = mine
            .iter()
            .find(|e| itip::partstat_of(req, e).is_some())
            .cloned()
            .ok_or_else(|| Error::bad("you are not on this invitation's attendee list"))?;
        let user = self.user(user_id).await?;
        let existing = self.own_object_by_uid(user_id, &uid).await?;
        // A Ferrite organizer whose invitation is in the calendar: answer through scheduling.
        let internal = self.user_by_email(&org.email).await?.is_some_and(|u| u.id != user_id);
        if internal && user.email.as_deref().map(itip::normalize_address).as_deref() == Some(me.as_str()) {
            if let Some((_, o)) = &existing {
                let saved = self.respond_to_event(user_id, &o.id, partstat).await?;
                let outcome = ImipOutcome::new("answered", format!("{} (through Ferrite)", response_word(partstat)), Some(saved.id));
                self.record_imip(&a, &key, &part, &outcome).await?;
                return Ok(outcome);
            }
        }
        let prev = existing.as_ref().and_then(|(_, o)| Component::parse(&o.data).ok());
        if prev.as_ref().is_some_and(|p| itip::sequence(p) > itip::sequence(req)) {
            return Err(Error::conflict("this invitation was updated since; answer the newer one"));
        }
        let mut copy = Self::merged_copy(req, prev.as_ref(), &me);
        itip::set_partstat(&mut copy, &me, partstat);
        let o = self.store_copy(user_id, existing, calendar, &copy.to_ics()).await?;
        let statuses = itip::partstats(&copy, &me);
        let mut reply = Component::parse(&itip::reply(&copy, &me, &statuses))?;
        if let Some(c) = comment.map(str::trim).filter(|c| !c.is_empty()) {
            for item in reply.items_mut() {
                item.set(Prop::new("COMMENT", crate::groupware::escape(c)));
            }
        }
        let summary = itip::summary(&copy);
        let name = if a.name.is_empty() { user.display_name.clone() } else { a.name.clone() };
        let mut text = format!(
            "{name} {} the invitation “{summary}”.\n\nWhen: {}\n",
            response_word(partstat).to_lowercase(),
            describe_when(&copy)
        );
        if let Some(c) = comment.map(str::trim).filter(|c| !c.is_empty()) {
            text.push_str(&format!("\n{c}\n"));
        }
        self.send_from_account(
            &a,
            OutgoingMail {
                to: vec![org.mailbox()],
                subject: format!("{}: {summary}", response_word(partstat)),
                text,
                calendar: Some(CalendarPart {
                    method: "REPLY".into(),
                    data: reply.to_ics(),
                }),
                ..Default::default()
            },
        )
        .await?;
        let outcome = ImipOutcome::new("answered", response_word(partstat), Some(o.id));
        self.record_imip(&a, &key, &part, &outcome).await?;
        Ok(outcome)
    }

    /// Proposes another time for an invitation (COUNTER to the organizer, RFC 5546 §3.2.7).
    pub async fn mail_imip_counter(
        &self,
        user_id: &str,
        message_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        comment: Option<&str>,
    ) -> Result<ImipOutcome> {
        if end <= start {
            return Err(Error::bad("the proposed end must be after its start"));
        }
        let (a, key, part, _) = self.imip_of(user_id, message_id, MailRight::Send).await?;
        if part.method != "REQUEST" {
            return Err(Error::bad("a new time can only be proposed for an invitation"));
        }
        let req = &part.cal;
        let org = itip::organizer(req).ok_or_else(|| Error::bad("the invitation has no organizer"))?;
        let mine = self.my_addresses(&a).await?;
        let me = mine
            .iter()
            .find(|e| itip::partstat_of(req, e).is_some())
            .cloned()
            .ok_or_else(|| Error::bad("you are not on this invitation's attendee list"))?;
        let master = req.master().ok_or_else(|| Error::bad("the invitation has no event"))?;
        let mut out = Component::new("VCALENDAR");
        out.props.push(Prop::new("PRODID", "-//Ferrite//Ferrite//EN"));
        out.props.push(Prop::new("VERSION", "2.0"));
        out.props.push(Prop::new("METHOD", "COUNTER"));
        let mut ev = Component::new(&master.name);
        let stamp = |t: &DateTime<Utc>| t.format("%Y%m%dT%H%M%SZ").to_string();
        ev.props.push(Prop::new("DTSTAMP", stamp(&now())));
        for p in master
            .props
            .iter()
            .filter(|p| matches!(p.name.as_str(), "UID" | "SEQUENCE" | "ORGANIZER" | "SUMMARY" | "LOCATION"))
        {
            ev.props.push(p.clone());
        }
        ev.props.push(Prop::new("DTSTART", stamp(&start)));
        ev.props.push(Prop::new("DTEND", stamp(&end)));
        if let Some(att) = master.all("ATTENDEE").find(|p| p.address() == me) {
            let mut att = att.clone();
            att.set_param("PARTSTAT", "TENTATIVE");
            att.remove_param("RSVP");
            ev.props.push(att);
        }
        if let Some(c) = comment.map(str::trim).filter(|c| !c.is_empty()) {
            ev.props.push(Prop::new("COMMENT", crate::groupware::escape(c)));
        }
        out.subs.push(ev);
        let summary = itip::summary(req);
        let mut text = format!(
            "{} proposes a new time for “{summary}”:\n\n{} – {} (UTC)\n",
            if a.name.is_empty() { a.email.as_str() } else { a.name.as_str() },
            start.format("%a, %-d %b %Y %H:%M"),
            end.format("%a, %-d %b %Y %H:%M")
        );
        if let Some(c) = comment.map(str::trim).filter(|c| !c.is_empty()) {
            text.push_str(&format!("\n{c}\n"));
        }
        self.send_from_account(
            &a,
            OutgoingMail {
                to: vec![org.mailbox()],
                subject: format!("New time proposed: {summary}"),
                text,
                calendar: Some(CalendarPart {
                    method: "COUNTER".into(),
                    data: out.to_ics(),
                }),
                ..Default::default()
            },
        )
        .await?;
        let outcome = ImipOutcome::new("answered", "Proposed a new time", None);
        self.record_imip(&a, &key, &part, &outcome).await?;
        Ok(outcome)
    }

    /// The organizer's decision on a proposed time: accepting moves the event (attendees are
    /// updated through scheduling); declining sends DECLINECOUNTER to the attendee.
    pub async fn mail_imip_counter_decision(&self, user_id: &str, message_id: &str, accept: bool) -> Result<ImipOutcome> {
        let (a, key, part, _) = self.imip_of(user_id, message_id, MailRight::Send).await?;
        if part.method != "COUNTER" {
            return Err(Error::bad("this message does not propose a new time"));
        }
        let counter = &part.cal;
        let uid = itip::uid(counter).ok_or_else(|| Error::bad("the proposal has no UID"))?;
        let attendee = counter
            .items()
            .find_map(|c| c.get("ATTENDEE"))
            .map(|p| Party {
                email: p.address(),
                name: p.param("CN").map(str::to_string),
                partstat: String::new(),
                role: String::new(),
                agent: String::new(),
            })
            .ok_or_else(|| Error::bad("the proposal names no attendee"))?;
        let (_, o) = self
            .own_object_by_uid(user_id, &uid)
            .await?
            .ok_or_else(|| Error::bad("the event is not in your calendars"))?;
        let current = Component::parse(&o.data)?;
        let mine = self.my_addresses(&a).await?;
        if !itip::organizer(&current).is_some_and(|org| mine.contains(&org.email)) {
            return Err(Error::bad("you do not organize this event"));
        }
        let outcome = if accept {
            let p = crate::groupware::parse_object(&counter.to_ics())?;
            let ev = self.event_view(user_id, &o.id).await?;
            let start = p.starts_at.ok_or_else(|| Error::bad("the proposal has no start"))?;
            let input = EventInput {
                summary: ev.summary.clone(),
                start,
                end: p.ends_at,
                all_day: false,
                description: Some(ev.description.clone()),
                location: Some(ev.location.clone()),
                calendar: None,
                attendees: None,
            };
            let updated = self.update_event(user_id, &o.id, &input).await?;
            ImipOutcome::new("applied", "Moved to the proposed time", Some(updated.id))
        } else {
            let mut out = Component::new("VCALENDAR");
            out.props.push(Prop::new("PRODID", "-//Ferrite//Ferrite//EN"));
            out.props.push(Prop::new("VERSION", "2.0"));
            out.props.push(Prop::new("METHOD", "DECLINECOUNTER"));
            let master = current.master().ok_or_else(|| Error::bad("the event has no component"))?;
            let mut ev = Component::new(&master.name);
            ev.props.push(Prop::new("DTSTAMP", now().format("%Y%m%dT%H%M%SZ").to_string()));
            for p in master.props.iter().filter(|p| matches!(p.name.as_str(), "UID" | "SEQUENCE" | "ORGANIZER")) {
                ev.props.push(p.clone());
            }
            if let Some(att) = master.all("ATTENDEE").find(|p| p.address() == attendee.email) {
                ev.props.push(att.clone());
            }
            out.subs.push(ev);
            let summary = itip::summary(&current);
            self.send_from_account(
                &a,
                OutgoingMail {
                    to: vec![attendee.mailbox()],
                    subject: format!("New time declined: {summary}"),
                    text: format!(
                        "The proposed new time for “{summary}” was declined. The event stays at {}.\n",
                        describe_when(&current)
                    ),
                    calendar: Some(CalendarPart {
                        method: "DECLINECOUNTER".into(),
                        data: out.to_ics(),
                    }),
                    ..Default::default()
                },
            )
            .await?;
            ImipOutcome::new("answered", "Declined the proposed time", Some(o.id))
        };
        self.record_imip(&a, &key, &part, &outcome).await?;
        Ok(outcome)
    }

    /// Applies a REPLY, CANCEL or REQUEST update from a message now. `force` applies it even
    /// when the sender is not the party the method requires (the user checked it).
    pub async fn mail_imip_apply(&self, user_id: &str, message_id: &str, force: bool) -> Result<ImipOutcome> {
        let (a, key, part, from) = self.imip_of(user_id, message_id, MailRight::Modify).await?;
        let settings = super::integrations::AccountIntegrations::manual();
        let outcome = self.apply_itip(&a, &part, &from, force, &settings).await?;
        self.record_imip(&a, &key, &part, &outcome).await?;
        Ok(outcome)
    }

    async fn apply_itip(&self, a: &Account, part: &ItipPart, from: &Senders, force: bool, s: &super::integrations::AccountIntegrations) -> Result<ImipOutcome> {
        match part.method.as_str() {
            "REPLY" => self.apply_reply_mail(a, &part.cal, from, force).await,
            "CANCEL" if s.imip_auto_update => self.apply_cancel_mail(a, &part.cal, from, force).await,
            "REQUEST" | "ADD" => self.apply_request_mail(a, &part.cal, from, force, s).await,
            "CANCEL" => Ok(ImipOutcome::new("ignored", "automatic updates are off for this account", None)),
            other => Ok(ImipOutcome::new("ignored", format!("{other} messages need no processing"), None)),
        }
    }

    async fn apply_reply_mail(&self, a: &Account, reply: &Component, from: &Senders, force: bool) -> Result<ImipOutcome> {
        let uid = itip::uid(reply).ok_or_else(|| Error::bad("the reply has no UID"))?;
        let Some(att) = reply.items().find_map(|c| c.get("ATTENDEE")) else {
            return Ok(ImipOutcome::new("ignored", "the reply names no attendee", None));
        };
        let attendee = att.address();
        if !force && !from.is(&attendee, att.param("SENT-BY")) {
            return Ok(ImipOutcome::new(
                "rejected",
                format!("the reply was not sent by {attendee}, so it was not applied"),
                None,
            ));
        }
        let user = self.user(&a.user_id).await?;
        let Some((_, o)) = self.own_object_by_uid(&a.user_id, &uid).await? else {
            return Ok(ImipOutcome::new("ignored", "the event is not in your calendars", None));
        };
        let event = Component::parse(&o.data)?;
        let mine = self.my_addresses(a).await?;
        if !itip::organizer(&event).is_some_and(|org| mine.contains(&org.email)) {
            return Ok(ImipOutcome::new("ignored", "you do not organize this event", Some(o.id)));
        }
        if itip::partstat_of(&event, &attendee).is_none() {
            return Ok(ImipOutcome::new("ignored", format!("{attendee} is not invited to this event"), Some(o.id)));
        }
        if itip::sequence(reply) < itip::sequence(&event) {
            return Ok(ImipOutcome::new("outdated", "the reply answers an older version of the event", Some(o.id)));
        }
        let stamp = dtstamp(reply);
        let last: Option<String> = sqlx::query_scalar("SELECT dtstamp FROM mail_imip_replies WHERE user_id = ? AND uid = ? AND attendee = ?")
            .bind(&user.id)
            .bind(&uid)
            .bind(&attendee)
            .fetch_optional(&self.db)
            .await?;
        if last.as_deref().is_some_and(|l| !stamp.is_empty() && stamp.as_str() < l) {
            return Ok(ImipOutcome::new("outdated", "a newer reply from this attendee was applied already", Some(o.id)));
        }
        let statuses = reply_statuses(reply, &attendee, &event);
        if statuses.is_empty() {
            return Ok(ImipOutcome::new("ignored", "the reply has no answer", Some(o.id)));
        }
        let name = itip::attendees(&event)
            .into_iter()
            .find(|x| x.email == attendee)
            .map(|x| x.display().to_string())
            .unwrap_or_else(|| attendee.clone());
        self.apply_reply(&user, &uid, &attendee, &name, &statuses, ("", "")).await?;
        sqlx::query(
            "INSERT INTO mail_imip_replies (user_id, uid, attendee, dtstamp) VALUES (?, ?, ?, ?)
             ON CONFLICT(user_id, uid, attendee) DO UPDATE SET dtstamp = excluded.dtstamp",
        )
        .bind(&user.id)
        .bind(&uid)
        .bind(&attendee)
        .bind(&stamp)
        .execute(&self.db)
        .await?;
        let ps = statuses.get("").or_else(|| statuses.values().next()).cloned().unwrap_or_default();
        Ok(ImipOutcome::new("applied", format!("{name}: {}", ps.to_lowercase()), Some(o.id)))
    }

    async fn apply_cancel_mail(&self, a: &Account, cancel: &Component, from: &Senders, force: bool) -> Result<ImipOutcome> {
        let uid = itip::uid(cancel).ok_or_else(|| Error::bad("the cancellation has no UID"))?;
        let Some((c, o)) = self.own_object_by_uid(&a.user_id, &uid).await? else {
            return Ok(ImipOutcome::new("ignored", "the event is not in your calendars", None));
        };
        let mut copy = Component::parse(&o.data)?;
        let org = itip::organizer(&copy).or_else(|| itip::organizer(cancel));
        let sent_by = cancel
            .items()
            .find_map(|c| c.get("ORGANIZER"))
            .and_then(|p| p.param("SENT-BY").map(str::to_string));
        if !force && !org.as_ref().is_some_and(|org| from.is(&org.email, sent_by.as_deref())) {
            return Ok(ImipOutcome::new(
                "rejected",
                "the cancellation was not sent by the organizer, so it was not applied",
                Some(o.id),
            ));
        }
        if itip::sequence(cancel) < itip::sequence(&copy) {
            return Ok(ImipOutcome::new("outdated", "the cancellation is older than your copy", Some(o.id)));
        }
        if itip::is_cancelled(&copy) {
            return Ok(ImipOutcome::new("ignored", "the event is already cancelled", Some(o.id)));
        }
        let instances: Vec<&Component> = cancel.items().filter(|x| x.get("RECURRENCE-ID").is_some()).collect();
        if instances.is_empty() {
            itip::mark_cancelled(&mut copy, itip::sequence(cancel));
        } else {
            for inst in instances {
                let rid = inst.get("RECURRENCE-ID").expect("filtered on RECURRENCE-ID");
                let want = rid_instant(rid);
                let target = copy.subs.iter_mut().find(|x| {
                    x.get("RECURRENCE-ID")
                        .is_some_and(|r| r.value.trim() == rid.value.trim() || (want.is_some() && rid_instant(r) == want))
                });
                match target {
                    Some(t) => t.set(Prop::new("STATUS", "CANCELLED")),
                    None => {
                        if let Some(m) = copy.master_mut() {
                            let mut ex = Prop::new("EXDATE", rid.value.clone());
                            ex.params = rid.params.iter().filter(|(k, _)| k != "RANGE").cloned().collect();
                            m.props.push(ex);
                        }
                    }
                }
            }
        }
        let summary = itip::summary(&copy);
        let saved = self.put_dav_object(&c, &o.uri, &copy.to_ics(), Some(&o.etag), false).await?.0;
        let who = org.as_ref().map(|o| o.display().to_string()).unwrap_or_default();
        self.notify(
            &a.user_id,
            "calendar.cancel",
            &format!("{who} cancelled “{summary}”"),
            &describe_when(&copy),
            &event_link(&saved),
        )
        .await?;
        Ok(ImipOutcome::new("applied", "Cancelled in your calendar", Some(saved.id)))
    }

    async fn apply_request_mail(
        &self,
        a: &Account,
        req: &Component,
        from: &Senders,
        force: bool,
        s: &super::integrations::AccountIntegrations,
    ) -> Result<ImipOutcome> {
        let uid = itip::uid(req).ok_or_else(|| Error::bad("the invitation has no UID"))?;
        let Some(org) = itip::organizer(req) else {
            return Ok(ImipOutcome::new("ignored", "the invitation has no organizer", None));
        };
        let sent_by = req
            .items()
            .find_map(|c| c.get("ORGANIZER"))
            .and_then(|p| p.param("SENT-BY").map(str::to_string));
        if !force && !from.is(&org.email, sent_by.as_deref()) {
            return Ok(ImipOutcome::new(
                "rejected",
                format!("the invitation was not sent by its organizer {}", org.email),
                None,
            ));
        }
        let mine = self.my_addresses(a).await?;
        let Some(me) = mine.iter().find(|e| itip::partstat_of(req, e).is_some()).cloned() else {
            return Ok(ImipOutcome::new("ignored", "you are not on the attendee list", None));
        };
        let existing = self.own_object_by_uid(&a.user_id, &uid).await?;
        let org_name = org.display().to_string();
        let summary = itip::summary(req);
        match existing {
            Some((c, o)) => {
                if !s.imip_auto_update {
                    return Ok(ImipOutcome::new("ignored", "automatic updates are off for this account", Some(o.id)));
                }
                let prev = Component::parse(&o.data)?;
                let (rs, ps) = (itip::sequence(req), itip::sequence(&prev));
                if rs < ps || (rs == ps && dtstamp(req) <= dtstamp(&prev)) {
                    return Ok(ImipOutcome::new(
                        if rs < ps { "outdated" } else { "ignored" },
                        "your calendar is up to date",
                        Some(o.id),
                    ));
                }
                let next = Self::merged_copy(req, Some(&prev), &me);
                let significant = itip::is_significant(&prev, &next);
                let saved = self.put_dav_object(&c, &o.uri, &next.to_ics(), Some(&o.etag), false).await?.0;
                if significant {
                    self.notify(
                        &a.user_id,
                        "calendar.invitation",
                        &format!("{org_name} updated “{summary}”"),
                        &describe_when(&next),
                        &event_link(&saved),
                    )
                    .await?;
                }
                Ok(ImipOutcome::new("applied", "Updated in your calendar", Some(saved.id)))
            }
            None if s.imip_auto_create => {
                let mut copy = Self::merged_copy(req, None, &me);
                for item in copy.items_mut() {
                    item.set(Prop::new("STATUS", "TENTATIVE"));
                }
                let saved = self.store_copy(&a.user_id, None, None, &copy.to_ics()).await?;
                self.notify(
                    &a.user_id,
                    "calendar.invitation",
                    &format!("{org_name} invited you to “{summary}”"),
                    &describe_when(&copy),
                    &event_link(&saved),
                )
                .await?;
                Ok(ImipOutcome::new("applied", "Added to your calendar as tentative", Some(saved.id)))
            }
            None => Ok(ImipOutcome::new("ignored", "not in your calendar yet; answer to add it", None)),
        }
    }

    /// Processes iMIP messages that just arrived in an account's inbox (in the background).
    pub(crate) fn after_new_mail(&self, a: &Account, folder_id: &str, new_ids: Vec<String>) {
        if new_ids.is_empty() {
            return;
        }
        let (core, a, folder_id) = (self.clone(), a.clone(), folder_id.to_string());
        tokio::spawn(async move {
            if let Err(e) = core.process_new_imip(&a, &folder_id, &new_ids).await {
                tracing::info!(account = %a.id, error = %e, "processing meeting invitations failed");
            }
        });
    }

    async fn process_new_imip(&self, a: &Account, folder_id: &str, ids: &[String]) -> Result<()> {
        let rows: Vec<(String, i64)> = sqlx::query_as("SELECT id, uid FROM mail_messages WHERE folder_id = ? AND id IN (SELECT value FROM json_each(?))")
            .bind(folder_id)
            .bind(serde_json::to_string(ids)?)
            .fetch_all(&self.db)
            .await?;
        if rows.is_empty() {
            return Ok(());
        }
        let folder = self.folder_row(folder_id).await?;
        let uids: Vec<u32> = rows.iter().map(|(_, u)| *u as u32).collect();
        let with_cal = {
            let mut conn = self.imap_conn(a).await?;
            conn.ensure_selected(&folder.name).await?;
            conn.uids_with_calendar(&super::imap::uid_set(&uids)).await?
        };
        let settings = self.integrations_of(a).await?;
        for (id, uid) in rows.iter().filter(|(_, u)| with_cal.contains(&(*u as u32))) {
            let (key, part, from) = match self.imip_of(&a.user_id, id, MailRight::Read).await {
                Ok((_, key, part, from)) => (key, part, from),
                Err(e) => {
                    tracing::debug!(message = %id, uid, error = %e, "not an iMIP message");
                    continue;
                }
            };
            if self.imip_log(&a.id, &key).await?.is_some() {
                continue;
            }
            let outcome = match self.apply_itip(a, &part, &from, false, &settings).await {
                Ok(o) => o,
                Err(e) => ImipOutcome::new("error", e.to_string(), None),
            };
            tracing::info!(account = %a.id, method = %part.method, status = outcome.status, detail = %outcome.detail, "iMIP message processed");
            self.log_imip(a, &key, &part, &outcome, true).await?;
        }
        Ok(())
    }

    /// Imports a calendar attachment into one of the user's calendars (events and tasks; an
    /// object with the same UID there is replaced). Nothing is sent to anyone.
    pub async fn mail_import_ics(&self, user_id: &str, message_id: &str, index: usize, calendar: Option<&str>) -> Result<Vec<String>> {
        let (_, _, data) = self.mail_attachment(user_id, message_id, index).await?;
        let text = String::from_utf8_lossy(&data);
        let cal = Component::parse(&text)?;
        let target = match calendar {
            Some(uri) => self.dav_collection(user_id, KIND_CALENDAR, uri).await?,
            None => self.dav_collections(user_id, KIND_CALENDAR).await?.into_iter().next().ok_or(Error::NotFound)?,
        };
        // One object per UID, with the calendar's time zones.
        let mut by_uid: Vec<(String, Vec<Component>)> = Vec::new();
        for item in cal.items() {
            let uid = item
                .value("UID")
                .map(|u| u.trim().to_string())
                .unwrap_or_else(|| format!("{}@ferrite", new_id()));
            match by_uid.iter_mut().find(|(u, _)| *u == uid) {
                Some((_, v)) => v.push(item.clone()),
                None => by_uid.push((uid, vec![item.clone()])),
            }
        }
        if by_uid.is_empty() {
            return Err(Error::bad("the file has no events or tasks"));
        }
        let mut ids = Vec::new();
        for (uid, items) in by_uid {
            let mut obj = Component::new("VCALENDAR");
            obj.props = cal.props.iter().filter(|p| p.name != "METHOD").cloned().collect();
            obj.subs.extend(cal.subs.iter().filter(|c| c.name == "VTIMEZONE").cloned());
            for mut it in items {
                if it.get("UID").is_none() {
                    it.props.insert(0, Prop::new("UID", uid.clone()));
                }
                obj.subs.push(it);
            }
            let data = obj.to_ics();
            let existing: Option<String> = sqlx::query_scalar("SELECT uri FROM dav_objects WHERE collection_id = ? AND uid = ?")
                .bind(&target.id)
                .bind(&uid)
                .fetch_optional(&self.db)
                .await?;
            let uri = existing.unwrap_or_else(|| object_uri(&uid));
            let (o, _) = self.put_dav_object(&target, &uri, &data, None, false).await?;
            self.record_calendar_author(&o.id, user_id).await?;
            ids.push(o.id);
        }
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A REQUEST as Thunderbird sends it (RFC 5546 §4.2.1 shape): text/calendar with a
    /// method parameter inside multipart/alternative, plus the .ics attachment.
    pub const THUNDERBIRD_REQUEST: &str = "From: Organizer One <org@example.org>\r\nTo: alice@example.test\r\nSubject: Invitation: Design review\r\nMessage-ID: <req-1@example.org>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"mix\"\r\n\r\n--mix\r\nContent-Type: multipart/alternative; boundary=\"alt\"\r\n\r\n--alt\r\nContent-Type: text/plain; charset=UTF-8\r\n\r\nYou have been invited to a meeting.\r\n--alt\r\nContent-Type: text/calendar; charset=UTF-8; method=REQUEST\r\nContent-Transfer-Encoding: 8bit\r\n\r\nBEGIN:VCALENDAR\r\nPRODID:-//Mozilla.org/NONSGML Mozilla Calendar V1.1//EN\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VTIMEZONE\r\nTZID:Europe/Berlin\r\nBEGIN:STANDARD\r\nTZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100\r\nTZNAME:CET\r\nDTSTART:19701025T030000\r\nRRULE:FREQ=YEARLY;BYDAY=-1SU;BYMONTH=10\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nCREATED:20261020T100000Z\r\nLAST-MODIFIED:20261020T100000Z\r\nDTSTAMP:20261020T100000Z\r\nUID:design-review-42@example.org\r\nSUMMARY:Design review\r\nORGANIZER;RSVP=TRUE;CN=Organizer One;PARTSTAT=ACCEPTED;ROLE=CHAIR:mailto:org@example.org\r\nATTENDEE;RSVP=TRUE;CN=Alice;PARTSTAT=NEEDS-ACTION;ROLE=REQ-PARTICIPANT:mailto:alice@example.test\r\nATTENDEE;RSVP=TRUE;PARTSTAT=NEEDS-ACTION;ROLE=OPT-PARTICIPANT:mailto:bob@example.org\r\nDTSTART;TZID=Europe/Berlin:20261105T100000\r\nDTEND;TZID=Europe/Berlin:20261105T110000\r\nLOCATION:Room 4\r\nSEQUENCE:0\r\nTRANSP:OPAQUE\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n\r\n--alt--\r\n--mix\r\nContent-Type: application/ics; name=\"invite.ics\"\r\nContent-Disposition: attachment; filename=\"invite.ics\"\r\nContent-Transfer-Encoding: base64\r\n\r\nQkVHSU46VkNBTEVOREFSDQpQUk9ESUQ6LS8vTW96aWxsYS5vcmcvTk9OU0dNTCBNb3ppbGxhIENh\r\nbGVuZGFyIFYxLjEvL0VODQpWRVJTSU9OOjIuMA0KTUVUSE9EOlJFUVVFU1QNCkJFR0lOOlZUSU1F\r\nWk9ORQ0KVFpJRDpFdXJvcGUvQmVybGluDQpCRUdJTjpTVEFOREFSRA0KVFpPRkZTRVRGUk9NOisw\r\nMjAwDQpUWk9GRlNFVFRPOiswMTAwDQpUWk5BTUU6Q0VUDQpEVFNUQVJUOjE5NzAxMDI1VDAzMDAw\r\nMA0KUlJVTEU6RlJFUT1ZRUFSTFk7QllEQVk9LTFTVTtCWU1PTlRIPTEwDQpFTkQ6U1RBTkRBUkQN\r\nCkVORDpWVElNRVpPTkUNCkJFR0lOOlZFVkVOVA0KQ1JFQVRFRDoyMDI2MTAyMFQxMDAwMDBaDQpM\r\nQVNULU1PRElGSUVEOjIwMjYxMDIwVDEwMDAwMFoNCkRUU1RBTVA6MjAyNjEwMjBUMTAwMDAwWg0K\r\nVUlEOmRlc2lnbi1yZXZpZXctNDJAZXhhbXBsZS5vcmcNClNVTU1BUlk6RGVzaWduIHJldmlldw0K\r\nT1JHQU5JWkVSO1JTVlA9VFJVRTtDTj1Pcmdhbml6ZXIgT25lO1BBUlRTVEFUPUFDQ0VQVEVEO1JP\r\nTEU9Q0hBSVI6bWFpbHRvOm9yZ0BleGFtcGxlLm9yZw0KQVRURU5ERUU7UlNWUD1UUlVFO0NOPUFs\r\naWNlO1BBUlRTVEFUPU5FRURTLUFDVElPTjtST0xFPVJFUS1QQVJUSUNJUEFOVDptYWlsdG86YWxp\r\nY2VAZXhhbXBsZS50ZXN0DQpBVFRFTkRFRTtSU1ZQPVRSVUU7UEFSVFNUQVQ9TkVFRFMtQUNUSU9O\r\nO1JPTEU9T1BULVBBUlRJQ0lQQU5UOm1haWx0bzpib2JAZXhhbXBsZS5vcmcNCkRUU1RBUlQ7VFpJ\r\nRD1FdXJvcGUvQmVybGluOjIwMjYxMTA1VDEwMDAwMA0KRFRFTkQ7VFpJRD1FdXJvcGUvQmVybGlu\r\nOjIwMjYxMTA1VDExMDAwMA0KTE9DQVRJT046Um9vbSA0DQpTRVFVRU5DRTowDQpUUkFOU1A6T1BB\r\nUVVFDQpFTkQ6VkVWRU5UDQpFTkQ6VkNBTEVOREFSDQo=\r\n--mix--\r\n";

    #[test]
    fn finds_calendar_parts_and_attachments() {
        let raw = THUNDERBIRD_REQUEST.as_bytes();
        let parts = itip_parts(raw);
        assert_eq!(parts.len(), 1, "the alternative part and the identical attachment count once");
        assert_eq!(parts[0].method, "REQUEST");
        assert_eq!(itip::uid(&parts[0].cal).as_deref(), Some("design-review-42@example.org"));
        assert_eq!(itip::summary(&parts[0].cal), "Design review");
        // mail-parser lists the text/calendar alternative as an attachment too.
        assert_eq!(ics_attachments(raw), vec![0, 1]);
        let s = senders(raw);
        assert!(s.is("org@example.org", None) && !s.is("mallory@example.org", None));
        // A plain message has none.
        assert!(itip_parts(b"From: a@b.c\r\nSubject: x\r\n\r\nhello\r\n").is_empty());
    }

    #[test]
    fn reply_statuses_match_instances_by_time() {
        let organizer = Component::parse("BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:s\r\nDTSTART;TZID=Europe/Berlin:20261105T090000\r\nRRULE:FREQ=WEEKLY\r\nATTENDEE:mailto:g@example.org\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:s\r\nRECURRENCE-ID;TZID=Europe/Berlin:20261112T090000\r\nDTSTART;TZID=Europe/Berlin:20261112T110000\r\nATTENDEE:mailto:g@example.org\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n").unwrap();
        // Google writes the RECURRENCE-ID in UTC.
        let reply = Component::parse("BEGIN:VCALENDAR\r\nMETHOD:REPLY\r\nBEGIN:VEVENT\r\nUID:s\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:g@example.org\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:s\r\nRECURRENCE-ID:20261112T080000Z\r\nATTENDEE;PARTSTAT=DECLINED:mailto:G@Example.org\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n").unwrap();
        let st = reply_statuses(&reply, "g@example.org", &organizer);
        assert_eq!(st.get("").map(String::as_str), Some("ACCEPTED"));
        assert_eq!(st.get("20261112T090000").map(String::as_str), Some("DECLINED"));
        assert_eq!(object_uri("a/b c@x"), "a-b-c@x.ics");
    }
}
