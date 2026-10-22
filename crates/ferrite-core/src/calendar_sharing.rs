//! Calendar sharing: owners share a calendar with users or groups, read-only or read-write,
//! and can publish a read-only ICS subscription feed at an unguessable URL.
//!
//! A sharee sees a shared calendar next to their own (in the web app and in their CalDAV
//! calendar home) under a Nextcloud-style uri, `<uri>_shared_by_<owner>`, with their own
//! name and colour for it. Access resolves to [`CalAccess`]: read-only sharees can read and
//! subscribe but every write is refused; read-write sharees create, edit and delete events
//! (recorded as theirs in `calendar_object_authors`). Changes reach everyone with access
//! through `groupware.changed`.

use crate::auth::User;
use crate::groupware::{Collection, DavObject, KIND_CALENDAR};
use crate::itip::{Component, Prop};
use crate::util::{new_id, now, random_token};
use crate::{Core, Error, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{BTreeSet, HashMap};

/// Separates a shared calendar's uri from its owner's username in a sharee's view.
pub const SHARED_SEP: &str = "_shared_by_";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CalAccess {
    Owner,
    ReadWrite,
    Read,
}

impl CalAccess {
    pub fn can_write(self) -> bool {
        self != CalAccess::Read
    }
}

/// The owner of a calendar shared with the viewer.
#[derive(Debug, Clone, Serialize)]
pub struct SharedBy {
    pub id: String,
    pub username: String,
    pub display_name: String,
    #[serde(skip)]
    pub email: Option<String>,
}

/// A calendar as one user sees it. For a shared calendar, `collection.uri`, `name` and
/// `color` are the sharee's (the stored collection keeps the owner's).
#[derive(Debug, Clone, Serialize)]
pub struct CalendarRef {
    #[serde(flatten)]
    pub collection: Collection,
    pub access: CalAccess,
    pub writable: bool,
    pub shared_by: Option<SharedBy>,
}

impl CalendarRef {
    /// A collection seen by its owner.
    pub fn owned(c: Collection) -> CalendarRef {
        CalendarRef {
            collection: c,
            access: CalAccess::Owner,
            writable: true,
            shared_by: None,
        }
    }

    pub fn is_owner(&self) -> bool {
        self.access == CalAccess::Owner
    }

    pub fn require_write(&self) -> Result<()> {
        if self.access.can_write() { Ok(()) } else { Err(Error::Forbidden) }
    }
}

#[derive(sqlx::FromRow)]
struct SharedRow {
    id: String,
    owner_id: String,
    kind: String,
    uri: String,
    name: String,
    color: String,
    components: String,
    ctag: String,
    created_at: DateTime<Utc>,
    writable: bool,
    pref_name: Option<String>,
    pref_color: Option<String>,
    owner_username: String,
    owner_display_name: String,
    owner_email: Option<String>,
}

impl SharedRow {
    fn into_ref(self) -> CalendarRef {
        let access = if self.writable { CalAccess::ReadWrite } else { CalAccess::Read };
        CalendarRef {
            collection: Collection {
                uri: format!("{}{SHARED_SEP}{}", self.uri, self.owner_username),
                name: self.pref_name.unwrap_or(self.name),
                color: self.pref_color.unwrap_or(self.color),
                id: self.id,
                owner_id: self.owner_id.clone(),
                kind: self.kind,
                components: self.components,
                ctag: self.ctag,
                created_at: self.created_at,
            },
            access,
            writable: access.can_write(),
            shared_by: Some(SharedBy {
                id: self.owner_id,
                username: self.owner_username,
                display_name: self.owner_display_name,
                email: self.owner_email,
            }),
        }
    }
}

/// One share of a calendar, as its owner manages it.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct CalendarShare {
    pub id: String,
    /// "user" or "group"
    pub kind: String,
    pub target_id: String,
    /// Username or group name.
    pub target: String,
    pub target_name: String,
    pub writable: bool,
    pub created_at: DateTime<Utc>,
}

const SHARED_SQL: &str = "SELECT c.id, c.owner_id, c.kind, c.uri, c.name, c.color, c.components, c.ctag, c.created_at,
        MAX(s.writable) AS writable, p.name AS pref_name, p.color AS pref_color,
        u.username AS owner_username, u.display_name AS owner_display_name, u.email AS owner_email
     FROM calendar_shares s
     JOIN dav_collections c ON c.id = s.collection_id
     JOIN users u ON u.id = c.owner_id
     LEFT JOIN calendar_share_prefs p ON p.collection_id = c.id AND p.user_id = ?1
     WHERE c.owner_id != ?1 AND u.disabled = 0
       AND ((s.target_kind = 'user' AND s.target_id = ?1)
         OR (s.target_kind = 'group' AND s.target_id IN (SELECT group_id FROM group_members WHERE user_id = ?1)))
       AND COALESCE(p.removed, 0) = 0
     GROUP BY c.id
     ORDER BY u.display_name, c.created_at";

fn valid_color(c: &str) -> bool {
    c.starts_with('#') && c.len() <= 9 && c[1..].chars().all(|x| x.is_ascii_hexdigit())
}

impl Core {
    /// Calendars shared with `user_id` by others.
    pub async fn shared_calendars(&self, user_id: &str) -> Result<Vec<CalendarRef>> {
        let rows: Vec<SharedRow> = sqlx::query_as(SHARED_SQL).bind(user_id).fetch_all(&self.db).await?;
        Ok(rows.into_iter().map(SharedRow::into_ref).collect())
    }

    /// The user's own calendars followed by those shared with them.
    pub async fn user_calendars(&self, user_id: &str) -> Result<Vec<CalendarRef>> {
        let mut out: Vec<CalendarRef> = self
            .dav_collections(user_id, KIND_CALENDAR)
            .await?
            .into_iter()
            .map(CalendarRef::owned)
            .collect();
        out.extend(self.shared_calendars(user_id).await?);
        Ok(out)
    }

    /// A calendar by the uri `user_id` sees it under (own, or `<uri>_shared_by_<owner>`).
    pub async fn calendar_ref(&self, user_id: &str, uri: &str) -> Result<CalendarRef> {
        if let Ok(c) = self.dav_collection(user_id, KIND_CALENDAR, uri).await {
            return Ok(CalendarRef::owned(c));
        }
        if !uri.contains(SHARED_SEP) {
            return Err(Error::NotFound);
        }
        self.shared_calendars(user_id)
            .await?
            .into_iter()
            .find(|c| c.collection.uri == uri)
            .ok_or(Error::NotFound)
    }

    /// A calendar `user_id` may write events to, by uri or name; by default their first own
    /// calendar that holds events.
    pub async fn writable_calendar(&self, user_id: &str, uri: Option<&str>) -> Result<CalendarRef> {
        let cals = self.user_calendars(user_id).await?;
        let found = match uri {
            Some(u) => cals
                .into_iter()
                .find(|c| c.collection.uri == u || c.collection.name.eq_ignore_ascii_case(u))
                .ok_or(Error::NotFound)?,
            None => cals
                .into_iter()
                .find(|c| c.is_owner() && c.collection.components.split(',').any(|x| x == "VEVENT"))
                .ok_or(Error::NotFound)?,
        };
        found.require_write()?;
        Ok(found)
    }

    /// The calendar holding object `id` as `user_id` sees it, and the object.
    pub async fn calendar_object_access(&self, user_id: &str, id: &str) -> Result<(CalendarRef, DavObject)> {
        let o: DavObject = sqlx::query_as(
            "SELECT id, collection_id, uri, uid, etag, component, summary, starts_at, ends_at, all_day, recurring, data, modified_at
             FROM dav_objects WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&self.db)
        .await?;
        let cal = self
            .user_calendars(user_id)
            .await?
            .into_iter()
            .find(|c| c.collection.id == o.collection_id)
            .ok_or(Error::NotFound)?;
        Ok((cal, o))
    }

    /// Everyone who sees `c`: its owner and every user it is shared with (directly or
    /// through a group).
    pub async fn collection_audience(&self, c: &Collection) -> Result<Vec<String>> {
        let mut ids: Vec<String> = if c.kind == KIND_CALENDAR {
            sqlx::query_scalar(
                "SELECT target_id FROM calendar_shares WHERE collection_id = ?1 AND target_kind = 'user'
                 UNION SELECT m.user_id FROM calendar_shares s JOIN group_members m ON m.group_id = s.target_id
                   WHERE s.collection_id = ?1 AND s.target_kind = 'group'",
            )
            .bind(&c.id)
            .fetch_all(&self.db)
            .await?
        } else {
            Vec::new()
        };
        ids.retain(|u| *u != c.owner_id);
        ids.insert(0, c.owner_id.clone());
        Ok(ids)
    }

    /// Publishes `groupware.changed` to everyone who sees `c`.
    pub async fn publish_groupware_change(&self, c: &Collection, collections: bool) -> Result<()> {
        let audience = self.collection_audience(c).await?;
        let mut ev = serde_json::json!({"type": "groupware.changed", "kind": c.kind});
        if collections {
            ev["collections"] = serde_json::json!(true);
        }
        self.hub.publish(audience.iter().map(String::as_str), &ev);
        Ok(())
    }

    fn publish_collections_changed(&self, users: &[String]) {
        self.hub.publish(
            users.iter().map(String::as_str),
            &serde_json::json!({"type": "groupware.changed", "kind": KIND_CALENDAR, "collections": true}),
        );
    }

    async fn own_calendar(&self, owner_id: &str, uri: &str) -> Result<Collection> {
        self.dav_collection(owner_id, KIND_CALENDAR, uri).await
    }

    pub async fn calendar_shares(&self, owner_id: &str, uri: &str) -> Result<Vec<CalendarShare>> {
        let c = self.own_calendar(owner_id, uri).await?;
        Ok(sqlx::query_as(
            "SELECT s.id, s.target_kind AS kind, s.target_id,
                    COALESCE(u.username, g.name, '') AS target,
                    COALESCE(u.display_name, g.name, '') AS target_name,
                    s.writable, s.created_at
             FROM calendar_shares s
             LEFT JOIN users u ON s.target_kind = 'user' AND u.id = s.target_id
             LEFT JOIN groups g ON s.target_kind = 'group' AND g.id = s.target_id
             WHERE s.collection_id = ? ORDER BY s.created_at",
        )
        .bind(&c.id)
        .fetch_all(&self.db)
        .await?)
    }

    /// Shares the owner's calendar `uri` with a user (by username) or a group (by name).
    /// Sharing again with the same target changes the permission.
    pub async fn share_calendar(&self, owner: &User, uri: &str, kind: &str, target: &str, writable: bool) -> Result<CalendarShare> {
        let c = self.own_calendar(&owner.id, uri).await?;
        let target = target.trim();
        let (target_id, recipients) = match kind {
            "user" => {
                let u = self.user_by_name(target).await.map_err(|_| Error::bad(format!("no user named {target}")))?;
                if u.id == owner.id {
                    return Err(Error::bad("you own this calendar"));
                }
                if u.disabled {
                    return Err(Error::bad(format!("{target} is disabled")));
                }
                (u.id.clone(), vec![u.id])
            }
            "group" => {
                let gid: String = sqlx::query_scalar("SELECT id FROM groups WHERE name = ?")
                    .bind(target)
                    .fetch_optional(&self.db)
                    .await?
                    .ok_or_else(|| Error::bad(format!("no group named {target}")))?;
                let members = self.group_members(&gid).await?.into_iter().map(|u| u.id).filter(|u| *u != owner.id).collect();
                (gid, members)
            }
            _ => return Err(Error::bad("share with a user or a group")),
        };
        let existing: Option<String> = sqlx::query_scalar("SELECT id FROM calendar_shares WHERE collection_id = ? AND target_kind = ? AND target_id = ?")
            .bind(&c.id)
            .bind(kind)
            .bind(&target_id)
            .fetch_optional(&self.db)
            .await?;
        let id = match existing {
            Some(id) => {
                sqlx::query("UPDATE calendar_shares SET writable = ? WHERE id = ?")
                    .bind(writable)
                    .bind(&id)
                    .execute(&self.db)
                    .await?;
                id
            }
            None => {
                let id = new_id();
                sqlx::query("INSERT INTO calendar_shares (id, collection_id, target_kind, target_id, writable, created_at) VALUES (?, ?, ?, ?, ?, ?)")
                    .bind(&id)
                    .bind(&c.id)
                    .bind(kind)
                    .bind(&target_id)
                    .bind(writable)
                    .bind(now())
                    .execute(&self.db)
                    .await?;
                for r in &recipients {
                    sqlx::query("UPDATE calendar_share_prefs SET removed = 0 WHERE collection_id = ? AND user_id = ?")
                        .bind(&c.id)
                        .bind(r)
                        .execute(&self.db)
                        .await?;
                    self.notify(
                        r,
                        "calendar.share",
                        &format!("{} shared the calendar “{}” with you", owner.display_name, c.name),
                        if writable { "You can add and edit events." } else { "Read only." },
                        "/calendar",
                    )
                    .await?;
                }
                id
            }
        };
        self.publish_collections_changed(&recipients);
        self.calendar_shares(&owner.id, uri)
            .await?
            .into_iter()
            .find(|s| s.id == id)
            .ok_or(Error::NotFound)
    }

    pub async fn update_calendar_share(&self, owner_id: &str, uri: &str, share_id: &str, writable: bool) -> Result<CalendarShare> {
        let c = self.own_calendar(owner_id, uri).await?;
        let n = sqlx::query("UPDATE calendar_shares SET writable = ? WHERE id = ? AND collection_id = ?")
            .bind(writable)
            .bind(share_id)
            .bind(&c.id)
            .execute(&self.db)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::NotFound);
        }
        let audience = self.collection_audience(&c).await?;
        self.publish_collections_changed(&audience);
        self.calendar_shares(owner_id, uri)
            .await?
            .into_iter()
            .find(|s| s.id == share_id)
            .ok_or(Error::NotFound)
    }

    pub async fn unshare_calendar(&self, owner_id: &str, uri: &str, share_id: &str) -> Result<()> {
        let c = self.own_calendar(owner_id, uri).await?;
        let before = self.collection_audience(&c).await?;
        let n = sqlx::query("DELETE FROM calendar_shares WHERE id = ? AND collection_id = ?")
            .bind(share_id)
            .bind(&c.id)
            .execute(&self.db)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::NotFound);
        }
        self.publish_collections_changed(&before);
        Ok(())
    }

    /// Removes the share of `uri` with a user (by username) or group (by name).
    pub async fn unshare_calendar_target(&self, owner_id: &str, uri: &str, kind: &str, target: &str) -> Result<()> {
        let share = self
            .calendar_shares(owner_id, uri)
            .await?
            .into_iter()
            .find(|s| s.kind == kind && s.target.eq_ignore_ascii_case(target.trim()))
            .ok_or(Error::NotFound)?;
        self.unshare_calendar(owner_id, uri, &share.id).await
    }

    /// A sharee's own name and colour for a calendar shared with them.
    pub async fn set_shared_calendar_prefs(&self, user_id: &str, cal: &CalendarRef, name: Option<&str>, color: Option<&str>) -> Result<()> {
        let name = name.map(str::trim).filter(|n| !n.is_empty());
        let color = color.filter(|c| valid_color(c));
        sqlx::query(
            "INSERT INTO calendar_share_prefs (collection_id, user_id, name, color) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(collection_id, user_id) DO UPDATE SET name = COALESCE(?3, name), color = COALESCE(?4, color)",
        )
        .bind(&cal.collection.id)
        .bind(user_id)
        .bind(name)
        .bind(color)
        .execute(&self.db)
        .await?;
        self.publish_collections_changed(&[user_id.to_string()]);
        Ok(())
    }

    /// Removes a calendar shared with `user_id` from their list: their own share ends; a
    /// group share stays but is hidden for them until shared with them again.
    pub async fn leave_shared_calendar(&self, user_id: &str, cal: &CalendarRef) -> Result<()> {
        if cal.is_owner() {
            return Err(Error::bad("that is your own calendar"));
        }
        sqlx::query("DELETE FROM calendar_shares WHERE collection_id = ? AND target_kind = 'user' AND target_id = ?")
            .bind(&cal.collection.id)
            .bind(user_id)
            .execute(&self.db)
            .await?;
        sqlx::query(
            "INSERT INTO calendar_share_prefs (collection_id, user_id, removed) VALUES (?, ?, 1)
             ON CONFLICT(collection_id, user_id) DO UPDATE SET removed = 1",
        )
        .bind(&cal.collection.id)
        .bind(user_id)
        .execute(&self.db)
        .await?;
        self.publish_collections_changed(&[user_id.to_string()]);
        Ok(())
    }

    // ---- public subscription links -----------------------------------------------------

    fn feed_url(&self, token: &str) -> String {
        format!("{}/remote.php/dav/public-calendars/{token}.ics", self.config.public_url)
    }

    /// The public feed URL of the owner's calendar, if published.
    pub async fn calendar_publication(&self, owner_id: &str, uri: &str) -> Result<Option<String>> {
        let c = self.own_calendar(owner_id, uri).await?;
        let token: Option<String> = sqlx::query_scalar("SELECT token FROM calendar_publications WHERE collection_id = ?")
            .bind(&c.id)
            .fetch_optional(&self.db)
            .await?;
        Ok(token.map(|t| self.feed_url(&t)))
    }

    /// Publishes a read-only ICS feed of the calendar; returns its URL (the existing one if
    /// already published).
    pub async fn publish_calendar(&self, owner_id: &str, uri: &str) -> Result<String> {
        let c = self.own_calendar(owner_id, uri).await?;
        sqlx::query("INSERT OR IGNORE INTO calendar_publications (collection_id, token, created_at) VALUES (?, ?, ?)")
            .bind(&c.id)
            .bind(random_token(24))
            .bind(now())
            .execute(&self.db)
            .await?;
        self.calendar_publication(owner_id, uri).await?.ok_or(Error::NotFound)
    }

    /// Revokes the public feed: its URL stops working for good.
    pub async fn unpublish_calendar(&self, owner_id: &str, uri: &str) -> Result<()> {
        let c = self.own_calendar(owner_id, uri).await?;
        sqlx::query("DELETE FROM calendar_publications WHERE collection_id = ?")
            .bind(&c.id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// The public feed of a published calendar: every stored object in one VCALENDAR.
    /// Attendee and organizer addresses and alarms are left out; private events are hidden
    /// and confidential ones show only as busy time.
    pub async fn public_calendar_feed(&self, token: &str) -> Result<(String, String)> {
        let c: Collection = sqlx::query_as(
            "SELECT c.id, c.owner_id, c.kind, c.uri, c.name, c.color, c.components, c.ctag, c.created_at
             FROM calendar_publications p JOIN dav_collections c ON c.id = p.collection_id WHERE p.token = ?",
        )
        .bind(token)
        .fetch_one(&self.db)
        .await?;
        let mut feed = Component::new("VCALENDAR");
        feed.props.push(Prop::new("VERSION", "2.0"));
        feed.props.push(Prop::new("PRODID", "-//Ferrite//Ferrite//EN"));
        feed.props.push(Prop::new("CALSCALE", "GREGORIAN"));
        feed.props.push(Prop::new("X-WR-CALNAME", crate::groupware::escape(&c.name)));
        feed.props.push(Prop::new("X-APPLE-CALENDAR-COLOR", c.color.clone()));
        let mut zones = BTreeSet::new();
        for o in self.dav_objects(&c).await? {
            let Ok(cal) = Component::parse(&o.data) else { continue };
            for sub in cal.subs {
                if sub.name == "VTIMEZONE" {
                    let tzid = sub.value("TZID").unwrap_or("").to_string();
                    if zones.insert(tzid) {
                        feed.subs.push(sub);
                    }
                    continue;
                }
                let class = sub.value("CLASS").unwrap_or("PUBLIC").trim().to_ascii_uppercase();
                if class == "PRIVATE" {
                    continue;
                }
                let mut item = sub;
                item.subs.retain(|s| s.name != "VALARM");
                item.props.retain(|p| !matches!(p.name.as_str(), "ATTENDEE" | "ORGANIZER"));
                if class == "CONFIDENTIAL" {
                    item.props.retain(|p| {
                        matches!(
                            p.name.as_str(),
                            "UID"
                                | "DTSTAMP"
                                | "DTSTART"
                                | "DTEND"
                                | "DURATION"
                                | "DUE"
                                | "RRULE"
                                | "RDATE"
                                | "EXDATE"
                                | "RECURRENCE-ID"
                                | "SEQUENCE"
                                | "CLASS"
                                | "TRANSP"
                                | "STATUS"
                        )
                    });
                    item.props.push(Prop::new("SUMMARY", "Busy"));
                }
                feed.subs.push(item);
            }
        }
        Ok((c.name, feed.to_ics()))
    }

    // ---- authorship ---------------------------------------------------------------------

    /// Records who created (first write) and last changed a calendar object.
    pub async fn record_calendar_author(&self, object_id: &str, user_id: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO calendar_object_authors (object_id, created_by, modified_by) VALUES (?1, ?2, ?2)
             ON CONFLICT(object_id) DO UPDATE SET modified_by = ?2",
        )
        .bind(object_id)
        .bind(user_id)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// (created by, last modified by) user ids of the objects in a collection.
    pub async fn calendar_authors(&self, collection_id: &str) -> Result<HashMap<String, (Option<String>, Option<String>)>> {
        let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT a.object_id, a.created_by, a.modified_by FROM calendar_object_authors a
             JOIN dav_objects o ON o.id = a.object_id WHERE o.collection_id = ?",
        )
        .bind(collection_id)
        .fetch_all(&self.db)
        .await?;
        Ok(rows.into_iter().map(|(o, c, m)| (o, (c, m))).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors() {
        assert!(valid_color("#0f766e") && valid_color("#abc") && !valid_color("red") && !valid_color("#12345g"));
    }
}
