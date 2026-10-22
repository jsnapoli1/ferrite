//! Invitations and replies: server-side implicit scheduling (RFC 6638) for every way an
//! event can be written: the web app, MCP tools and CalDAV clients.
//!
//! **Why the server schedules.** Ferrite advertises `calendar-auto-schedule` on CalDAV, so
//! Apple Calendar, Thunderbird, DAVx⁵-based apps and others leave invitations to the server
//! and send no iMIP mail of their own; the web app and the MCP tools never send anything
//! either. Every write goes through [`Core::save_calendar_object`] /
//! [`Core::delete_calendar_object`], which compare the stored copy with the new one and
//! send each message exactly once, from one place, whoever made the change. That is what
//! keeps attendees from receiving the same invitation twice:
//!
//! - Messages go out only for a real change: a new attendee gets a REQUEST, a removed one a
//!   CANCEL, everyone else only when the time, recurrence, title or location changes (the
//!   SEQUENCE is raised if the client did not). Re-saving an unchanged event, adding an
//!   alarm, or a client writing back the server's own status updates sends nothing.
//! - Attendees marked `SCHEDULE-AGENT=CLIENT` (or `NONE`) are left to the client that says
//!   it handles them, as RFC 6638 §7.1 requires; a client that schedules on its own this
//!   way is never doubled by the server.
//! - Copies the server delivers into an attendee's calendar, and status updates written into
//!   the organizer's copy, are stored directly and never schedule again.
//! - Moving an event between calendars (a PUT into the new one before the DELETE of the old
//!   one, as clients do) is recognized by its UID and sends nothing.
//!
//! Delivery: attendees whose email address belongs to a Ferrite account get the event in
//! their default calendar (status NEEDS-ACTION) with a notification; everyone else gets an
//! iMIP email (RFC 6047) through `[smtp]` with Reply-To set to the organizer and links to a
//! public page where they answer without an account (`/invitation/<token>`, a token signed
//! with the server secret per attendee and event). Replies (from the web app, a CalDAV
//! client changing the attendee's PARTSTAT, deleting an invitation, or that page) update
//! the organizer's copy and notify them. Recurring events are scheduled as a series, and a
//! reply applies to the instances it names (the whole series from Ferrite's own UI).

use crate::auth::User;
use crate::calendar_sharing::CalendarRef;
use crate::groupware::{Collection, DavObject, KIND_CALENDAR};
use crate::itip::{self, Component, Party};
use crate::mailer::{CalendarPart, OutgoingMail};
use crate::util::new_id;
use crate::{Core, Error, Result};
use base64::Engine;
use chrono::{DateTime, Utc};
use hmac::{KeyInit, Mac};
use serde::Serialize;
use std::collections::BTreeMap;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Answers an attendee can give.
pub const RESPONSES: [&str; 3] = ["ACCEPTED", "TENTATIVE", "DECLINED"];

/// "accept", "accepted", "Tentative", "decline" … → the PARTSTAT value.
pub fn parse_response(s: &str) -> Result<&'static str> {
    match s.trim().to_ascii_lowercase().as_str() {
        "accept" | "accepted" | "yes" => Ok("ACCEPTED"),
        "tentative" | "maybe" => Ok("TENTATIVE"),
        "decline" | "declined" | "no" => Ok("DECLINED"),
        _ => Err(Error::bad("answer accepted, tentative or declined")),
    }
}

/// The result of storing a calendar object.
pub struct SavedObject {
    pub object: DavObject,
    pub created: bool,
    /// The stored data differs from what was sent (scheduling status, sequence, reset
    /// statuses): CalDAV must then not return an ETag for the PUT (RFC 4791 §5.3.4).
    pub rewritten: bool,
}

/// A recipient of a scheduling message, with their Ferrite account when they have one.
#[derive(Clone)]
struct Recipient {
    party: Party,
    user: Option<User>,
}

enum Message {
    Request { to: Recipient, view: Component, update: bool },
    Refresh { to: User, view: Component },
    Cancel { to: Recipient, view: Component, whole: bool },
    Reply { statuses: BTreeMap<String, String> },
}

/// What a write means for scheduling, worked out before the object is stored.
#[derive(Default)]
struct Plan {
    /// Data to store instead of the request body.
    data: Option<String>,
    messages: Vec<Message>,
    uid: String,
    summary: String,
    organizer: Option<Party>,
    /// The organizer's account when the organizer is a Ferrite user.
    organizer_user: Option<User>,
    /// The attendee replying (for REPLY plans).
    attendee: Option<Party>,
    /// The calendar data the messages describe (new copy, or the old one for deletions).
    cal: Option<Component>,
}

/// An invitation as the public response page shows it.
#[derive(Debug, Clone, Serialize)]
pub struct InvitationView {
    pub summary: String,
    pub description: String,
    pub location: String,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub all_day: bool,
    pub tzid: Option<String>,
    pub recurring: bool,
    /// Human-readable time, in the event's own time zone.
    pub when: String,
    pub organizer: Party,
    pub attendee: Party,
}

/// A calendar-safe object name for a UID.
fn uri_for_uid(uid: &str) -> String {
    let clean: String = uid
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "@._-".contains(c) { c } else { '-' })
        .take(200)
        .collect();
    format!("{}.ics", clean.trim_matches('.'))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// "Thu, 5 Nov 2026, 09:00 – 10:00 (Europe/Berlin)" in the event's own zone.
pub fn describe_when(cal: &Component) -> String {
    let Some(p) = crate::groupware::parse_object(&cal.to_ics()).ok() else {
        return String::new();
    };
    let Some(start) = p.starts_at else {
        return String::new();
    };
    let end = p.ends_at.unwrap_or(start);
    let mut s = if p.all_day {
        let last = if end > start { end - chrono::Duration::days(1) } else { start };
        if last.date_naive() == start.date_naive() {
            format!("{} (all day)", start.format("%a, %-d %b %Y"))
        } else {
            format!("{} – {} (all day)", start.format("%a, %-d %b %Y"), last.format("%a, %-d %b %Y"))
        }
    } else {
        let zone = p.tzid.as_deref().map(crate::recur::Zone::from_tzid).unwrap_or(crate::recur::Zone::Utc);
        let (a, b) = (zone.to_local(start), zone.to_local(end));
        let label = zone.name().unwrap_or("UTC");
        if a.date() == b.date() {
            format!("{}, {} – {} ({label})", a.format("%a, %-d %b %Y"), a.format("%H:%M"), b.format("%H:%M"))
        } else {
            format!("{} – {} ({label})", a.format("%a, %-d %b %Y, %H:%M"), b.format("%a, %-d %b %Y, %H:%M"))
        }
    };
    if let Some(freq) = cal
        .master()
        .and_then(|m| m.value("RRULE"))
        .and_then(|r| r.split(';').find_map(|part| part.strip_prefix("FREQ=")).map(|f| f.to_ascii_lowercase()))
    {
        let every = match freq.as_str() {
            "daily" => "every day",
            "weekly" => "every week",
            "monthly" => "every month",
            "yearly" => "every year",
            _ => "repeatedly",
        };
        s.push_str(&format!(", repeats {every}"));
    }
    s
}

fn response_word(partstat: &str) -> &'static str {
    match partstat {
        "ACCEPTED" => "accepted",
        "TENTATIVE" => "tentatively accepted",
        "DECLINED" => "declined",
        _ => "has not answered",
    }
}

impl Core {
    /// An active account with this email address.
    pub async fn user_by_email(&self, email: &str) -> Result<Option<User>> {
        let id: Option<String> = sqlx::query_scalar("SELECT id FROM users WHERE email = ? COLLATE NOCASE AND disabled = 0 ORDER BY created_at LIMIT 1")
            .bind(email.trim())
            .fetch_optional(&self.db)
            .await?;
        match id {
            Some(id) => Ok(Some(self.user(&id).await?)),
            None => Ok(None),
        }
    }

    async fn recipient(&self, party: Party) -> Result<Recipient> {
        let user = self.user_by_email(&party.email).await?;
        Ok(Recipient { party, user })
    }

    /// The object with `uid` in `user_id`'s own calendars (and, with `shared`, in calendars
    /// shared with them that they may write to).
    async fn find_by_uid(&self, user_id: &str, uid: &str, shared: bool) -> Result<Option<(Collection, DavObject)>> {
        for cal in self.user_calendars(user_id).await? {
            if !(cal.is_owner() || shared && cal.writable) {
                continue;
            }
            let found: Option<DavObject> = sqlx::query_as(
                "SELECT id, collection_id, uri, uid, etag, component, summary, starts_at, ends_at, all_day, recurring, data, modified_at
                 FROM dav_objects WHERE collection_id = ? AND uid = ?",
            )
            .bind(&cal.collection.id)
            .bind(uid)
            .fetch_optional(&self.db)
            .await?;
            if let Some(o) = found {
                return Ok(Some((cal.collection, o)));
            }
        }
        Ok(None)
    }

    /// Another copy of `uid` in the owner's calendars than (collection, uri): the event is
    /// being moved between calendars.
    async fn other_copy(&self, c: &Collection, uri: &str, uid: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "SELECT o.data FROM dav_objects o JOIN dav_collections c ON c.id = o.collection_id
             WHERE c.owner_id = ? AND c.kind = ? AND o.uid = ? AND NOT (o.collection_id = ? AND o.uri = ?) LIMIT 1",
        )
        .bind(&c.owner_id)
        .bind(KIND_CALENDAR)
        .bind(uid)
        .bind(&c.id)
        .bind(uri)
        .fetch_optional(&self.db)
        .await?)
    }

    /// Stores a calendar object written by `actor` (web app, MCP or CalDAV) and performs
    /// implicit scheduling for it.
    pub async fn save_calendar_object(
        &self,
        actor: &User,
        cal: &CalendarRef,
        uri: &str,
        data: &str,
        if_match: Option<&str>,
        if_none_match: bool,
    ) -> Result<SavedObject> {
        cal.require_write()?;
        let c = &cal.collection;
        let old = self.dav_object(c, uri).await.ok();
        let mut plan = self.plan(actor, cal, uri, old.as_ref().map(|o| o.data.as_str()), Some(data), true).await?;
        let stored = plan.data.take();
        let (object, created) = self.put_dav_object(c, uri, stored.as_deref().unwrap_or(data), if_match, if_none_match).await?;
        self.record_calendar_author(&object.id, &actor.id).await?;
        if !cal.is_owner() {
            self.record_sharee_activity(actor, cal, &object, created).await?;
        }
        self.execute(actor, (&c.id, uri), plan).await;
        Ok(SavedObject {
            object,
            created,
            rewritten: stored.is_some(),
        })
    }

    /// Deletes a calendar object for `actor` and performs implicit scheduling: the organizer
    /// deleting cancels the event for its attendees; an attendee deleting declines it
    /// (unless `reply` is false, the CalDAV `Schedule-Reply: F` header).
    pub async fn delete_calendar_object(&self, actor: &User, cal: &CalendarRef, uri: &str, if_match: Option<&str>, reply: bool) -> Result<()> {
        cal.require_write()?;
        let c = &cal.collection;
        let old = self.dav_object(c, uri).await?;
        self.delete_dav_object(c, uri, if_match).await?;
        let plan = self.plan(actor, cal, uri, Some(&old.data), None, reply).await?;
        self.execute(actor, (&c.id, uri), plan).await;
        Ok(())
    }

    /// Scheduling for an event that was moved from (collection, uri) with `old` data to a
    /// calendar where it now has `new` data (the web app's calendar picker).
    pub(crate) async fn schedule_moved(&self, actor: &User, to: &CalendarRef, uri: &str, old: &str, new: &str) -> Result<Option<String>> {
        let mut plan = self.plan(actor, to, uri, Some(old), Some(new), true).await?;
        let rewritten = plan.data.take();
        if let Some(data) = &rewritten {
            let o = self.dav_object(&to.collection, uri).await?;
            self.put_dav_object(&to.collection, uri, data, Some(&o.etag), false).await?;
        }
        self.execute(actor, (&to.collection.id, uri), plan).await;
        Ok(rewritten)
    }

    async fn record_sharee_activity(&self, actor: &User, cal: &CalendarRef, o: &DavObject, created: bool) -> Result<()> {
        let owner = cal.shared_by.as_ref().map(|s| s.display_name.as_str()).unwrap_or("");
        let audience = self.collection_audience(&cal.collection).await?;
        let summary = format!(
            "{} {} “{}” in {owner}’s calendar “{}”",
            actor.display_name,
            if created { "added" } else { "changed" },
            o.summary,
            cal.collection.name
        );
        self.record_activity(
            &audience,
            Some(&actor.id),
            if created { "created" } else { "updated" },
            "event",
            &o.id,
            &summary,
        )
        .await
    }

    /// Works out the scheduling messages for a change from `old` to `new` (None = deleted).
    async fn plan(&self, actor: &User, cal: &CalendarRef, uri: &str, old: Option<&str>, new: Option<&str>, reply_on_delete: bool) -> Result<Plan> {
        let c = &cal.collection;
        if c.kind != KIND_CALENDAR {
            return Ok(Plan::default());
        }
        let parse = |d: Option<&str>| d.and_then(|d| Component::parse(d).ok());
        let (old_cal, new_cal) = (parse(old), parse(new));
        let Some(current) = new_cal.as_ref().or(old_cal.as_ref()) else {
            return Ok(Plan::default());
        };
        let Some(org) = itip::organizer(current) else {
            return Ok(Plan::default());
        };
        let uid = itip::uid(current).unwrap_or_default();
        let owner = if cal.is_owner() { None } else { Some(self.user(&c.owner_id).await?) };
        let mine = |u: &User| u.email.as_deref().map(itip::normalize_address);
        let actor_email = mine(actor);
        let owner_email = owner.as_ref().map(mine).unwrap_or_else(|| actor_email.clone());
        let organizer_user = if actor_email.as_deref() == Some(org.email.as_str()) {
            Some(actor.clone())
        } else if owner_email.as_deref() == Some(org.email.as_str()) {
            Some(owner.clone().unwrap_or_else(|| actor.clone()))
        } else {
            None
        };
        match organizer_user {
            Some(org_user) => self.plan_organizer(c, uri, &uid, org, org_user, old_cal, new_cal, old.is_some()).await,
            None => {
                let candidates = [actor_email, owner_email];
                let me = candidates.iter().flatten().find(|e| itip::partstat_of(current, e).is_some()).cloned();
                match me {
                    Some(me) => self.plan_attendee(c, uri, &uid, org, &me, old_cal, new_cal, reply_on_delete).await,
                    None => Ok(Plan::default()),
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn plan_organizer(
        &self,
        c: &Collection,
        uri: &str,
        uid: &str,
        org: Party,
        org_user: User,
        old_cal: Option<Component>,
        new_cal: Option<Component>,
        had_old: bool,
    ) -> Result<Plan> {
        let mut plan = Plan {
            uid: uid.to_string(),
            organizer: Some(org.clone()),
            organizer_user: Some(org_user),
            ..Default::default()
        };
        if !org.server_scheduled() {
            return Ok(plan);
        }
        let Some(mut new_cal) = new_cal else {
            // Deleted: cancel for everyone, unless the event lives on in another calendar.
            let Some(old_cal) = old_cal else { return Ok(plan) };
            if self.other_copy(c, uri, uid).await?.is_some() || itip::is_cancelled(&old_cal) {
                return Ok(plan);
            }
            let seq = itip::sequence(&old_cal) + 1;
            for a in itip::attendees(&old_cal).into_iter().filter(Party::server_scheduled) {
                if let Some(view) = itip::for_attendee(&old_cal, &a.email) {
                    let mut view = view;
                    itip::set_sequence(&mut view, seq);
                    plan.messages.push(Message::Cancel {
                        to: self.recipient(a).await?,
                        view,
                        whole: true,
                    });
                }
            }
            plan.summary = itip::summary(&old_cal);
            plan.cal = Some(old_cal);
            return Ok(plan);
        };
        // A new object whose UID already exists in another of the owner's calendars is the
        // first half of a move: compare against that copy.
        let prev = match (old_cal, had_old) {
            (Some(o), _) => Some(o),
            (None, false) => self.other_copy(c, uri, uid).await?.and_then(|d| Component::parse(&d).ok()),
            (None, true) => None,
        };
        let raw_before = new_cal.to_ics();
        let (forced, _) = itip::take_force_send(&mut new_cal);
        let now_att = itip::attendees(&new_cal);
        let before_att = prev.as_ref().map(itip::attendees).unwrap_or_default();
        let was = |e: &str| before_att.iter().find(|a| a.email == e && a.server_scheduled());
        let cancelled_now = itip::is_cancelled(&new_cal) && !prev.as_ref().is_some_and(itip::is_cancelled);
        let significant = prev.as_ref().is_none_or(|p| itip::is_significant(p, &new_cal));
        if prev.as_ref().is_some_and(|p| itip::is_reschedule(p, &new_cal)) {
            itip::reset_partstats(&mut new_cal, &org.email);
        }
        let removed: Vec<Party> = before_att
            .iter()
            .filter(|a| a.server_scheduled() && !now_att.iter().any(|n| n.email == a.email && n.server_scheduled()))
            .cloned()
            .collect();
        if let Some(p) = &prev {
            let old_seq = itip::sequence(p);
            if (significant || !removed.is_empty() || cancelled_now) && itip::sequence(&new_cal) <= old_seq {
                itip::set_sequence(&mut new_cal, old_seq + 1);
            }
        }
        let mail = self.mail_configured();
        let mut statuses: Vec<(String, &'static str)> = Vec::new();
        for a in now_att.iter().filter(|a| a.server_scheduled()) {
            let Some(view) = itip::for_attendee(&new_cal, &a.email) else { continue };
            let to = self.recipient(a.clone()).await?;
            let existing = was(&a.email).is_some();
            let status = if to.user.is_some() {
                "1.2"
            } else if mail {
                "1.1"
            } else {
                "5.1"
            };
            if cancelled_now {
                statuses.push((a.email.clone(), status));
                plan.messages.push(Message::Cancel { to, view, whole: true });
            } else if !existing || significant || forced.contains(&a.email) {
                statuses.push((a.email.clone(), status));
                plan.messages.push(Message::Request { to, view, update: existing });
            } else if let Some(user) = to.user {
                plan.messages.push(Message::Refresh { to: user, view });
            }
        }
        for a in removed {
            let Some(view) = prev.as_ref().and_then(|p| itip::for_attendee(p, &a.email)) else {
                continue;
            };
            let mut view = view;
            itip::set_sequence(&mut view, itip::sequence(&new_cal));
            plan.messages.push(Message::Cancel {
                to: self.recipient(a).await?,
                view,
                whole: false,
            });
        }
        for (email, code) in &statuses {
            itip::set_schedule_status(&mut new_cal, "ATTENDEE", email, code);
        }
        let after = new_cal.to_ics();
        if after != raw_before || !forced.is_empty() || !statuses.is_empty() {
            plan.data = Some(after);
        }
        plan.summary = itip::summary(&new_cal);
        plan.cal = Some(new_cal);
        Ok(plan)
    }

    #[allow(clippy::too_many_arguments)]
    async fn plan_attendee(
        &self,
        c: &Collection,
        uri: &str,
        uid: &str,
        org: Party,
        me: &str,
        old_cal: Option<Component>,
        new_cal: Option<Component>,
        reply_on_delete: bool,
    ) -> Result<Plan> {
        let mut plan = Plan {
            uid: uid.to_string(),
            organizer_user: self.user_by_email(&org.email).await?,
            organizer: Some(org.clone()),
            ..Default::default()
        };
        if !org.server_scheduled() {
            return Ok(plan);
        }
        let attendee = new_cal
            .as_ref()
            .or(old_cal.as_ref())
            .and_then(|cal| itip::attendees(cal).into_iter().find(|a| a.email == me));
        let Some(attendee) = attendee else { return Ok(plan) };
        let statuses = match (&old_cal, new_cal) {
            (Some(old), None) => {
                // The attendee removed the invitation: decline it, unless it was cancelled,
                // already declined, or still exists in another of their calendars.
                if !reply_on_delete
                    || itip::is_cancelled(old)
                    || itip::partstat_of(old, me).as_deref() == Some("DECLINED")
                    || self.other_copy(c, uri, uid).await?.is_some()
                {
                    return Ok(plan);
                }
                let statuses: BTreeMap<String, String> = itip::partstats(old, me).into_keys().map(|k| (k, "DECLINED".to_string())).collect();
                let mut declined = old.clone();
                itip::set_partstat(&mut declined, me, "DECLINED");
                plan.cal = Some(declined);
                statuses
            }
            (old, Some(mut new)) => {
                let (_, force_reply) = itip::take_force_send(&mut new);
                let before = old.as_ref().map(|o| itip::partstats(o, me)).unwrap_or_default();
                let after = itip::partstats(&new, me);
                let changed: BTreeMap<String, String> = after
                    .iter()
                    .filter(|(k, v)| {
                        let prev = before.get(*k).map(String::as_str).unwrap_or("NEEDS-ACTION");
                        prev != v.as_str() || force_reply
                    })
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                if itip::is_cancelled(&new) || changed.is_empty() {
                    if force_reply {
                        plan.data = Some(new.to_ics());
                    }
                    return Ok(plan);
                }
                let code = if plan.organizer_user.is_some() {
                    "1.2"
                } else if self.mail_configured() {
                    "1.1"
                } else {
                    "5.1"
                };
                itip::set_schedule_status(&mut new, "ORGANIZER", &org.email, code);
                plan.data = Some(new.to_ics());
                plan.cal = Some(new);
                changed
            }
            (None, None) => return Ok(plan),
        };
        plan.summary = plan.cal.as_ref().map(itip::summary).unwrap_or_default();
        plan.attendee = Some(attendee);
        plan.messages.push(Message::Reply { statuses });
        Ok(plan)
    }

    /// Sends a plan's messages. Delivery problems are logged and reported to the organizer;
    /// they never undo the write that caused them.
    async fn execute(&self, actor: &User, source: (&str, &str), plan: Plan) {
        if let Err(e) = self.execute_inner(actor, source, plan).await {
            tracing::warn!(error = %e, "calendar scheduling failed");
        }
    }

    async fn execute_inner(&self, actor: &User, source: (&str, &str), plan: Plan) -> Result<()> {
        if plan.messages.is_empty() {
            return Ok(());
        }
        let Some(org) = plan.organizer.clone() else { return Ok(()) };
        let org_name = plan
            .organizer_user
            .as_ref()
            .map(|u| u.display_name.clone())
            .unwrap_or_else(|| org.display().to_string());
        for m in plan.messages {
            match m {
                Message::Request { to, view, update } => {
                    let summary = itip::summary(&view);
                    match &to.user {
                        Some(u) => {
                            let title = if update {
                                format!("{org_name} updated “{summary}”")
                            } else {
                                format!("{org_name} invited you to “{summary}”")
                            };
                            self.deliver(u, &view, source, Some((&title, "calendar.invitation"))).await?;
                        }
                        None => {
                            let org_user = plan.organizer_user.as_ref().map(|u| u.id.as_str()).unwrap_or(&actor.id);
                            self.mail_request(&org, &org_name, org_user, &to.party, &view, update);
                        }
                    }
                }
                Message::Refresh { to, view } => {
                    self.deliver(&to, &view, source, None).await?;
                }
                Message::Cancel { to, view, whole } => match &to.user {
                    Some(u) => self.cancel_delivered(u, &view, source, &org_name, whole).await?,
                    None => self.mail_cancel(&org, &org_name, &to.party, &view, whole),
                },
                Message::Reply { statuses } => {
                    let (Some(attendee), Some(cal)) = (&plan.attendee, &plan.cal) else { continue };
                    let name = if attendee.email == actor.email.as_deref().map(itip::normalize_address).unwrap_or_default() {
                        actor.display_name.clone()
                    } else {
                        attendee.display().to_string()
                    };
                    match &plan.organizer_user {
                        Some(org_user) => {
                            self.apply_reply(org_user, &plan.uid, &attendee.email, &name, &statuses, source).await?;
                        }
                        None => self.mail_reply(&org, attendee, &name, cal, &statuses),
                    }
                }
            }
        }
        Ok(())
    }

    /// Puts the organizer's view of an event into an internal attendee's calendar (updating
    /// their copy if they have one) and notifies them. Without a notification (a silent
    /// refresh) only an existing copy is updated: an attendee who removed the invitation
    /// does not get it back.
    async fn deliver(&self, user: &User, view: &Component, source: (&str, &str), notify: Option<(&str, &str)>) -> Result<()> {
        let Some(uid) = itip::uid(view) else { return Ok(()) };
        let object = match self.find_by_uid(&user.id, &uid, false).await? {
            // The attendee already sees the organizer's own copy (a calendar they own).
            Some((c, o)) if c.id == source.0 && o.uri == source.1 => o,
            Some((c, o)) => {
                let prev = Component::parse(&o.data).ok();
                let data = itip::attendee_copy(view, prev.as_ref());
                if data == o.data {
                    o
                } else {
                    self.put_dav_object(&c, &o.uri, &data, None, false).await?.0
                }
            }
            None if notify.is_none() => return Ok(()),
            None => {
                let c = self
                    .dav_collections(&user.id, KIND_CALENDAR)
                    .await?
                    .into_iter()
                    .find(|c| c.components.split(',').any(|x| x == "VEVENT"))
                    .ok_or(Error::NotFound)?;
                let data = itip::attendee_copy(view, None);
                let uri = uri_for_uid(&uid);
                match self.put_dav_object(&c, &uri, &data, None, true).await {
                    Ok((o, _)) => o,
                    Err(Error::PreconditionFailed) => self.put_dav_object(&c, &format!("{}.ics", new_id()), &data, None, true).await?.0,
                    Err(e) => return Err(e),
                }
            }
        };
        if let Some((title, kind)) = notify {
            self.notify(&user.id, kind, title, &describe_when(view), &event_link(&object)).await?;
        }
        Ok(())
    }

    /// Marks an internal attendee's copy cancelled (the event, or their invitation to it).
    async fn cancel_delivered(&self, user: &User, view: &Component, source: (&str, &str), org_name: &str, whole: bool) -> Result<()> {
        let Some(uid) = itip::uid(view) else { return Ok(()) };
        let Some((c, o)) = self.find_by_uid(&user.id, &uid, false).await? else {
            return Ok(());
        };
        let summary = itip::summary(view);
        if !(c.id == source.0 && o.uri == source.1) {
            let Ok(mut cal) = Component::parse(&o.data) else { return Ok(()) };
            if itip::is_cancelled(&cal) {
                return Ok(());
            }
            itip::mark_cancelled(&mut cal, itip::sequence(view));
            self.put_dav_object(&c, &o.uri, &cal.to_ics(), None, false).await?;
        }
        let title = if whole {
            format!("{org_name} cancelled “{summary}”")
        } else {
            format!("{org_name} removed you from “{summary}”")
        };
        self.notify(&user.id, "calendar.cancel", &title, &describe_when(view), &event_link(&o)).await
    }

    /// Records an attendee's answer in the organizer's copy, notifies the organizer and
    /// refreshes the other internal attendees' copies.
    async fn apply_reply(
        &self,
        org_user: &User,
        uid: &str,
        attendee: &str,
        name: &str,
        statuses: &BTreeMap<String, String>,
        source: (&str, &str),
    ) -> Result<()> {
        let Some((c, o)) = self.find_by_uid(&org_user.id, uid, true).await? else {
            return Ok(());
        };
        let mut cal = Component::parse(&o.data)?;
        let same_object = c.id == source.0 && o.uri == source.1;
        let changed = itip::apply_partstats(&mut cal, attendee, statuses);
        if changed && !same_object {
            self.put_dav_object(&c, &o.uri, &cal.to_ics(), Some(&o.etag), false).await?;
        }
        if !changed && !same_object {
            return Ok(());
        }
        let ps = statuses
            .get("")
            .or_else(|| statuses.values().next())
            .map(String::as_str)
            .unwrap_or("NEEDS-ACTION");
        let summary = itip::summary(&cal);
        if org_user.email.as_deref().map(itip::normalize_address).as_deref() != Some(attendee) {
            self.notify(
                &org_user.id,
                "calendar.reply",
                &format!("{name} {} “{summary}”", response_word(ps)),
                &describe_when(&cal),
                &event_link(&o),
            )
            .await?;
        }
        self.refresh_attendees(&cal, Some(attendee), (&c.id, &o.uri)).await
    }

    /// Updates internal attendees' copies to match the organizer's (no notifications).
    async fn refresh_attendees(&self, cal: &Component, except: Option<&str>, source: (&str, &str)) -> Result<()> {
        for a in itip::attendees(cal).into_iter().filter(Party::server_scheduled) {
            if Some(a.email.as_str()) == except {
                continue;
            }
            let (Some(user), Some(view)) = (self.user_by_email(&a.email).await?, itip::for_attendee(cal, &a.email)) else {
                continue;
            };
            self.deliver(&user, &view, source, None).await?;
        }
        Ok(())
    }

    /// The viewer's answer to an invitation in their calendar ("ACCEPTED", "TENTATIVE",
    /// "DECLINED"); applies to the whole series.
    pub async fn respond_to_event(&self, user_id: &str, event_id: &str, partstat: &str) -> Result<DavObject> {
        if !RESPONSES.contains(&partstat) {
            return Err(Error::bad("answer accepted, tentative or declined"));
        }
        let me = self.user(user_id).await?;
        let email = me
            .email
            .as_deref()
            .map(itip::normalize_address)
            .ok_or_else(|| Error::bad("add your email address in Settings to answer invitations"))?;
        let (mut cal, mut o) = self.calendar_object_access(user_id, event_id).await?;
        if !cal.is_owner() {
            // Seen in someone else's (shared) calendar: answer in the user's own copy, so the
            // reply goes to the organizer instead of editing their copy.
            let own = self.find_by_uid(user_id, &o.uid, false).await?;
            let (c, own_o) = own.ok_or_else(|| Error::bad("answer the invitation in your own calendar"))?;
            cal = CalendarRef::owned(c);
            o = own_o;
        }
        let mut c = Component::parse(&o.data)?;
        if itip::organizer(&c).is_some_and(|org| org.email == email) {
            return Err(Error::bad("you organize this event"));
        }
        if itip::partstat_of(&c, &email).is_none() {
            return Err(Error::bad("you are not invited to this event"));
        }
        itip::set_partstat(&mut c, &email, partstat);
        Ok(self.save_calendar_object(&me, &cal, &o.uri, &c.to_ics(), Some(&o.etag), false).await?.object)
    }

    // ---- external attendees: signed response links ----------------------------------------

    fn invitation_mac(&self, payload: &[u8]) -> hmac::Hmac<sha2::Sha256> {
        let mut mac = <hmac::Hmac<sha2::Sha256> as KeyInit>::new_from_slice(self.secret.as_bytes()).expect("HMAC takes keys of any length");
        mac.update(b"ferrite-invitation\n");
        mac.update(payload);
        mac
    }

    /// A per-attendee token for the public response page: the organizer's account, the
    /// event's UID and the attendee's address, signed with the server secret (`~` joins
    /// the parts: it is URL-safe and, unlike a dot, never taken for a file extension).
    pub fn invitation_token(&self, organizer_id: &str, uid: &str, email: &str) -> String {
        let payload = format!("{organizer_id}\n{uid}\n{email}");
        let tag = self.invitation_mac(payload.as_bytes()).finalize().into_bytes();
        format!("{}~{}", B64.encode(payload), B64.encode(tag))
    }

    fn open_invitation_token(&self, token: &str) -> Result<(String, String, String)> {
        let (p, t) = token.split_once('~').ok_or(Error::NotFound)?;
        let payload = B64.decode(p).map_err(|_| Error::NotFound)?;
        let tag = B64.decode(t).map_err(|_| Error::NotFound)?;
        self.invitation_mac(&payload).verify_slice(&tag).map_err(|_| Error::NotFound)?;
        let text = String::from_utf8(payload).map_err(|_| Error::NotFound)?;
        let mut parts = text.splitn(3, '\n');
        match (parts.next(), parts.next(), parts.next()) {
            (Some(o), Some(u), Some(e)) => Ok((o.to_string(), u.to_string(), e.to_string())),
            _ => Err(Error::NotFound),
        }
    }

    async fn invitation_parts(&self, token: &str) -> Result<(User, Collection, DavObject, Component, String)> {
        let (org_id, uid, email) = self.open_invitation_token(token)?;
        let org_user = self.user(&org_id).await?;
        let (c, o) = self.find_by_uid(&org_id, &uid, true).await?.ok_or(Error::NotFound)?;
        let cal = Component::parse(&o.data)?;
        // The attendee must still be invited and the event not cancelled.
        if itip::partstat_of(&cal, &email).is_none() || itip::is_cancelled(&cal) {
            return Err(Error::NotFound);
        }
        Ok((org_user, c, o, cal, email))
    }

    /// The invitation behind a public response link.
    pub async fn invitation(&self, token: &str) -> Result<InvitationView> {
        let (org_user, _, o, cal, email) = self.invitation_parts(token).await?;
        let p = crate::groupware::parse_object(&o.data)?;
        let organizer = itip::organizer(&cal).unwrap_or(Party {
            email: org_user.email.clone().unwrap_or_default(),
            name: Some(org_user.display_name.clone()),
            partstat: "ACCEPTED".into(),
            role: "CHAIR".into(),
            agent: "SERVER".into(),
        });
        let attendee = itip::attendees(&cal).into_iter().find(|a| a.email == email).ok_or(Error::NotFound)?;
        Ok(InvitationView {
            summary: p.summary,
            description: p.description,
            location: p.location,
            starts_at: p.starts_at,
            ends_at: p.ends_at,
            all_day: p.all_day,
            tzid: p.tzid,
            recurring: p.recurring,
            when: describe_when(&cal),
            organizer: Party {
                name: organizer.name.or(Some(org_user.display_name)),
                ..organizer
            },
            attendee,
        })
    }

    /// Records an external attendee's answer from the public response page.
    pub async fn respond_to_invitation(&self, token: &str, partstat: &str) -> Result<InvitationView> {
        if !RESPONSES.contains(&partstat) {
            return Err(Error::bad("answer accepted, tentative or declined"));
        }
        let (org_user, c, o, cal, email) = self.invitation_parts(token).await?;
        let statuses: BTreeMap<String, String> = itip::partstats(&cal, &email).into_keys().map(|k| (k, partstat.to_string())).collect();
        let name = itip::attendees(&cal)
            .into_iter()
            .find(|a| a.email == email)
            .map(|a| a.display().to_string())
            .unwrap_or_else(|| email.clone());
        let mut updated = cal.clone();
        if itip::apply_partstats(&mut updated, &email, &statuses) {
            self.put_dav_object(&c, &o.uri, &updated.to_ics(), Some(&o.etag), false).await?;
            // Notify as if the reply arrived for the copy just written.
            self.apply_reply(&org_user, &o.uid, &email, &name, &statuses, (&c.id, &o.uri)).await?;
        }
        self.invitation(token).await
    }

    // ---- iMIP email -------------------------------------------------------------------------

    /// "Alice Archer via Ferrite <noreply@…>" from the configured sender.
    fn mail_from(&self, name: &str) -> Option<String> {
        let from = &self.config.smtp.as_ref()?.from;
        let mailbox: lettre::message::Mailbox = from.parse().ok()?;
        Some(lettre::message::Mailbox::new(Some(format!("{name} via Ferrite")), mailbox.email).to_string())
    }

    fn spawn_mail(&self, mail: OutgoingMail, what: String) {
        if !self.mail_configured() {
            return;
        }
        let core = self.clone();
        tokio::spawn(async move {
            if let Err(e) = core.send_system_mail(&mail).await {
                tracing::warn!(error = %e, what, "calendar mail failed");
            }
        });
    }

    fn mail_request(&self, org: &Party, org_name: &str, org_user_id: &str, to: &Party, view: &Component, update: bool) {
        let summary = itip::summary(view);
        let when = describe_when(view);
        let master = view.master();
        let location = master.and_then(|m| m.value("LOCATION")).map(crate::groupware::unescape).unwrap_or_default();
        let description = master.and_then(|m| m.value("DESCRIPTION")).map(crate::groupware::unescape).unwrap_or_default();
        let uid = itip::uid(view).unwrap_or_default();
        let base = format!("{}/invitation/{}", self.config.public_url, self.invitation_token(org_user_id, &uid, &to.email));
        let links = [("Accept", "accepted"), ("Maybe", "tentative"), ("Decline", "declined")];
        let mut text = format!(
            "{org_name} {} “{summary}”.\n\nWhen: {when}\n",
            if update { "updated the event" } else { "invited you to" }
        );
        if !location.is_empty() {
            text.push_str(&format!("Where: {location}\n"));
        }
        text.push_str(&format!("Organizer: {} <{}>\n", org_name, org.email));
        if !description.is_empty() {
            text.push_str(&format!("\n{description}\n"));
        }
        text.push_str("\nWill you attend?\n");
        for (label, r) in links {
            text.push_str(&format!("  {label}: {base}?response={r}\n"));
        }
        text.push_str(&format!("\nOr open the invitation: {base}\n"));
        let buttons: String = links
            .iter()
            .map(|(label, r)| {
                format!(
                    "<a href=\"{}\" style=\"display:inline-block;margin:0 8px 8px 0;padding:9px 18px;border-radius:8px;background:#c2410c;color:#ffffff;text-decoration:none;font-weight:600\">{label}</a>",
                    html_escape(&format!("{base}?response={r}"))
                )
            })
            .collect();
        let html = format!(
            "<!doctype html><html><body style=\"font-family:system-ui,-apple-system,Segoe UI,sans-serif;color:#16191f;line-height:1.5\">\
             <p>{} {} <strong>{}</strong>.</p>\
             <table style=\"border-collapse:collapse;margin:12px 0\"><tr><td style=\"padding:2px 14px 2px 0;color:#5b6270\">When</td><td>{}</td></tr>{}\
             <tr><td style=\"padding:2px 14px 2px 0;color:#5b6270\">Organizer</td><td>{} &lt;{}&gt;</td></tr></table>{}\
             <p style=\"margin-top:18px\">Will you attend?</p><p>{buttons}</p></body></html>",
            html_escape(org_name),
            if update { "updated the event" } else { "invited you to" },
            html_escape(&summary),
            html_escape(&when),
            if location.is_empty() {
                String::new()
            } else {
                format!(
                    "<tr><td style=\"padding:2px 14px 2px 0;color:#5b6270\">Where</td><td>{}</td></tr>",
                    html_escape(&location)
                )
            },
            html_escape(org_name),
            html_escape(&org.email),
            if description.is_empty() {
                String::new()
            } else {
                format!("<p style=\"white-space:pre-wrap\">{}</p>", html_escape(&description))
            },
        );
        let subject = format!("{}: {summary} @ {when}", if update { "Updated invitation" } else { "Invitation" });
        self.spawn_mail(
            OutgoingMail {
                from: self.mail_from(org_name),
                to: vec![to.mailbox()],
                reply_to: Some(org.mailbox()),
                subject,
                text,
                html: Some(html),
                calendar: Some(CalendarPart {
                    method: "REQUEST".into(),
                    data: itip::request(view),
                }),
                ..Default::default()
            },
            format!("invitation to {}", to.email),
        );
    }

    fn mail_cancel(&self, org: &Party, org_name: &str, to: &Party, view: &Component, whole: bool) {
        let summary = itip::summary(view);
        let when = describe_when(view);
        let text = if whole {
            format!("{org_name} cancelled “{summary}”.\n\nWhen: {when}\n")
        } else {
            format!("{org_name} removed you from “{summary}”.\n\nWhen: {when}\n")
        };
        self.spawn_mail(
            OutgoingMail {
                from: self.mail_from(org_name),
                to: vec![to.mailbox()],
                reply_to: Some(org.mailbox()),
                subject: format!("Cancelled: {summary} @ {when}"),
                text,
                calendar: Some(CalendarPart {
                    method: "CANCEL".into(),
                    data: itip::cancel(view, std::slice::from_ref(&to.email), itip::sequence(view), whole),
                }),
                ..Default::default()
            },
            format!("cancellation to {}", to.email),
        );
    }

    fn mail_reply(&self, org: &Party, attendee: &Party, name: &str, cal: &Component, statuses: &BTreeMap<String, String>) {
        let summary = itip::summary(cal);
        let ps = statuses
            .get("")
            .or_else(|| statuses.values().next())
            .map(String::as_str)
            .unwrap_or("NEEDS-ACTION");
        let word = response_word(ps);
        self.spawn_mail(
            OutgoingMail {
                from: self.mail_from(name),
                to: vec![org.mailbox()],
                reply_to: Some(attendee.mailbox()),
                subject: format!("{name} {word}: {summary}"),
                text: format!("{name} {word} “{summary}”.\n\nWhen: {}\n", describe_when(cal)),
                calendar: Some(CalendarPart {
                    method: "REPLY".into(),
                    data: itip::reply(cal, &attendee.email, statuses),
                }),
                ..Default::default()
            },
            format!("reply to {}", org.email),
        );
    }

    // ---- free/busy (CalDAV scheduling outbox) ------------------------------------------------

    /// Busy periods of a user's own calendars within [from, to): opaque, not cancelled, and
    /// not declined by them.
    pub async fn busy_periods(&self, user: &User, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<(DateTime<Utc>, DateTime<Utc>)>> {
        let email = user.email.as_deref().map(itip::normalize_address);
        let mut out = Vec::new();
        for c in self.dav_collections(&user.id, KIND_CALENDAR).await? {
            for o in self.dav_objects_in_range(&c, Some(from), Some(to)).await? {
                if o.component != "VEVENT" {
                    continue;
                }
                let Ok(cal) = Component::parse(&o.data) else { continue };
                let master = cal.master();
                let transparent = master
                    .and_then(|m| m.value("TRANSP"))
                    .is_some_and(|t| t.trim().eq_ignore_ascii_case("TRANSPARENT"));
                let declined = email.as_deref().and_then(|e| itip::partstat_of(&cal, e)).as_deref() == Some("DECLINED");
                if transparent || declined || itip::is_cancelled(&cal) || o.all_day {
                    continue;
                }
                let spans: Vec<(DateTime<Utc>, DateTime<Utc>)> = if o.recurring {
                    crate::recur::expand(&o.data, from, to, 1000)
                        .map(|occ| occ.into_iter().map(|x| (x.start, x.end)).collect())
                        .unwrap_or_default()
                } else {
                    o.starts_at.zip(o.ends_at).into_iter().collect()
                };
                out.extend(spans.into_iter().filter(|(s, e)| *s < to && *e > from).map(|(s, e)| (s.max(from), e.min(to))));
            }
        }
        out.sort();
        Ok(out)
    }
}

/// Link to an event in the web calendar.
pub fn event_link(o: &DavObject) -> String {
    match o.starts_at {
        Some(t) => format!("/calendar?event={}&date={}", o.id, t.format("%Y-%m-%d")),
        None => format!("/calendar?event={}", o.id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(uri_for_uid("a/b c@x"), "a-b-c@x.ics");
        assert_eq!(parse_response("Accept").unwrap(), "ACCEPTED");
        assert_eq!(parse_response("maybe").unwrap(), "TENTATIVE");
        assert!(parse_response("perhaps").is_err());
        let cal = Component::parse("BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x\r\nDTSTART;TZID=Europe/Berlin:20261105T090000\r\nDTEND;TZID=Europe/Berlin:20261105T100000\r\nRRULE:FREQ=WEEKLY\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n").unwrap();
        assert_eq!(describe_when(&cal), "Thu, 5 Nov 2026, 09:00 – 10:00 (Europe/Berlin), repeats every week");
    }
}
