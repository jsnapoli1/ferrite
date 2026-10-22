//! Calendars and contacts (served over CalDAV/CardDAV, the web UI and MCP).
//!
//! Objects are stored as the exact iCalendar/vCard text clients send, so nothing a client
//! wrote is ever lost; a handful of fields (UID, summary, start/end, recurrence) are
//! extracted for time-range queries, the calendar view, search and AI tools.
//!
//! Limitation: times with a `TZID` are indexed as if they were UTC (the stored data is
//! exact); recurring events always match time-range queries instead of being expanded.

use crate::util::{new_id, now, short_token};
use crate::{Core, Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const KIND_CALENDAR: &str = "calendar";
pub const KIND_ADDRESSBOOK: &str = "addressbook";
const MAX_OBJECT: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Collection {
    pub id: String,
    pub owner_id: String,
    pub kind: String,
    pub uri: String,
    pub name: String,
    pub color: String,
    pub components: String,
    pub ctag: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct DavObject {
    pub id: String,
    pub collection_id: String,
    pub uri: String,
    pub uid: String,
    pub etag: String,
    pub component: String,
    pub summary: String,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub all_day: bool,
    pub recurring: bool,
    pub data: String,
    pub modified_at: DateTime<Utc>,
}

const OBJ_COLS: &str = "id, collection_id, uri, uid, etag, component, summary, starts_at, ends_at, all_day, recurring, data, modified_at";
const COL_COLS: &str = "id, owner_id, kind, uri, name, color, components, ctag, created_at";

/// Fields extracted from an iCalendar or vCard object.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Parsed {
    pub component: String,
    pub uid: String,
    pub summary: String,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub all_day: bool,
    pub recurring: bool,
    /// IANA zone of DTSTART, when it names one.
    pub tzid: Option<String>,
    pub description: String,
    pub location: String,
    pub emails: Vec<String>,
    pub phones: Vec<String>,
    pub org: String,
}

/// Unfolds RFC 5545/6350 content lines (CRLF or LF; continuation lines start with space/tab).
pub fn unfold(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if (line.starts_with(' ') || line.starts_with('\t')) && !out.is_empty() {
            out.last_mut().expect("checked non-empty").push_str(&line[1..]);
        } else if !line.is_empty() {
            out.push(line.to_string());
        }
    }
    out
}

/// Splits a content line into (NAME, params, value).
pub fn split_prop(line: &str) -> (String, Vec<(String, String)>, String) {
    let mut in_quotes = false;
    let mut colon = None;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ':' if !in_quotes => {
                colon = Some(i);
                break;
            }
            _ => {}
        }
    }
    let (head, value) = match colon {
        Some(i) => (&line[..i], &line[i + 1..]),
        None => (line, ""),
    };
    let mut parts = head.split(';');
    let name = parts.next().unwrap_or("").trim().to_ascii_uppercase();
    // Strip vCard group prefixes such as "item1.EMAIL".
    let name = name.rsplit('.').next().unwrap_or(&name).to_string();
    let params = parts
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k.trim().to_ascii_uppercase(), v.trim_matches('"').to_string()))
        .collect();
    (name, params, value.to_string())
}

pub fn unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn escape(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace(';', "\\;")
        .replace(',', "\\,")
        .replace("\r\n", "\\n")
        .replace('\n', "\\n")
}

/// Parses an ISO 8601 duration such as `PT1H30M` or `P1D` into seconds.
pub(crate) fn parse_duration(v: &str) -> Option<i64> {
    let (neg, v) = match v.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, v.strip_prefix('+').unwrap_or(v)),
    };
    let v = v.strip_prefix('P')?;
    let mut secs = 0i64;
    let mut num = String::new();
    let mut in_time = false;
    for c in v.chars() {
        match c {
            'T' => in_time = true,
            '0'..='9' => num.push(c),
            unit => {
                let n: i64 = num.parse().ok()?;
                num.clear();
                secs += match (unit, in_time) {
                    ('W', _) => n * 7 * 86400,
                    ('D', _) => n * 86400,
                    ('H', true) => n * 3600,
                    ('M', true) => n * 60,
                    ('S', true) => n,
                    _ => return None,
                };
            }
        }
    }
    Some(if neg { -secs } else { secs })
}

/// Extracts the indexed fields of the first VEVENT/VTODO/VJOURNAL or of a VCARD.
pub fn parse_object(text: &str) -> Result<Parsed> {
    let lines = unfold(text);
    let mut p = Parsed::default();
    let mut depth_target: Option<String> = None;
    let mut seen_root = false;
    let mut duration: Option<i64> = None;
    for line in &lines {
        let (name, params, value) = split_prop(line);
        match name.as_str() {
            "BEGIN" => {
                let comp = value.trim().to_ascii_uppercase();
                if matches!(comp.as_str(), "VCALENDAR" | "VCARD") {
                    seen_root = true;
                }
                if p.component.is_empty() && matches!(comp.as_str(), "VEVENT" | "VTODO" | "VJOURNAL" | "VCARD") {
                    p.component = comp.clone();
                    depth_target = Some(comp);
                }
                continue;
            }
            "END" => {
                if depth_target.as_deref() == Some(value.trim().to_ascii_uppercase().as_str()) {
                    depth_target = None;
                }
                continue;
            }
            _ => {}
        }
        if depth_target.is_none() {
            continue;
        }
        match name.as_str() {
            "UID" if p.uid.is_empty() => p.uid = value.trim().to_string(),
            "SUMMARY" | "FN" if p.summary.is_empty() => p.summary = unescape(&value),
            "DESCRIPTION" | "NOTE" if p.description.is_empty() => p.description = unescape(&value),
            "LOCATION" if p.location.is_empty() => p.location = unescape(&value),
            "ORG" if p.org.is_empty() => p.org = unescape(&value).replace(';', " ").trim().to_string(),
            "EMAIL" => p.emails.push(value.trim().to_string()),
            "TEL" => p.phones.push(value.trim().to_string()),
            "RRULE" | "RDATE" => p.recurring = true,
            "DTSTART" => {
                if let Some(t) = crate::recur::parse_time(&value, &params) {
                    p.starts_at = Some(t.utc());
                    p.all_day = t.date_only;
                    p.tzid = t.zone.name().map(str::to_string);
                }
            }
            "DTEND" | "DUE" => {
                if let Some(t) = crate::recur::parse_time(&value, &params) {
                    p.ends_at = Some(t.utc());
                }
            }
            "DURATION" => duration = parse_duration(value.trim()),
            _ => {}
        }
    }
    if !seen_root || p.component.is_empty() {
        return Err(Error::bad("not an iCalendar (VEVENT/VTODO/VJOURNAL) or vCard object"));
    }
    if p.uid.is_empty() {
        return Err(Error::bad("object has no UID"));
    }
    if p.ends_at.is_none() {
        p.ends_at = match (p.starts_at, duration) {
            (Some(s), Some(d)) => Some(s + chrono::Duration::seconds(d)),
            (Some(s), None) if p.all_day => Some(s + chrono::Duration::days(1)),
            (s, None) => s,
            (None, Some(_)) => None,
        };
    }
    Ok(p)
}

/// Folds a content line at 75 octets (RFC 5545 §3.1).
fn fold(line: &str) -> String {
    let mut out = String::new();
    let mut count = 0;
    for c in line.chars() {
        let n = c.len_utf8();
        if count + n > 75 {
            out.push_str("\r\n ");
            count = 1;
        }
        out.push(c);
        count += n;
    }
    out.push_str("\r\n");
    out
}

fn ical_time(t: &DateTime<Utc>, all_day: bool) -> String {
    if all_day {
        t.format("%Y%m%d").to_string()
    } else {
        t.format("%Y%m%dT%H%M%SZ").to_string()
    }
}

/// What the web UI and AI tools send to create or edit an event.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct EventInput {
    pub summary: String,
    pub start: DateTime<Utc>,
    pub end: Option<DateTime<Utc>>,
    #[serde(default)]
    pub all_day: bool,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
    /// Calendar uri (default: the first calendar). On update, a different calendar moves the event.
    #[serde(default)]
    pub calendar: Option<String>,
}

pub fn build_event(uid: &str, e: &EventInput) -> String {
    let end = e.end.unwrap_or(if e.all_day {
        e.start + chrono::Duration::days(1)
    } else {
        e.start + chrono::Duration::hours(1)
    });
    let date_param = if e.all_day { ";VALUE=DATE" } else { "" };
    let mut s = String::new();
    for l in [
        "BEGIN:VCALENDAR",
        "VERSION:2.0",
        "PRODID:-//Ferrite//Ferrite//EN",
        "CALSCALE:GREGORIAN",
        "BEGIN:VEVENT",
    ] {
        s.push_str(&fold(l));
    }
    s.push_str(&fold(&format!("UID:{uid}")));
    s.push_str(&fold(&format!("DTSTAMP:{}", ical_time(&now(), false))));
    s.push_str(&fold(&format!("DTSTART{date_param}:{}", ical_time(&e.start, e.all_day))));
    s.push_str(&fold(&format!("DTEND{date_param}:{}", ical_time(&end, e.all_day))));
    s.push_str(&fold(&format!("SUMMARY:{}", escape(&e.summary))));
    if let Some(d) = e.description.as_deref().filter(|d| !d.is_empty()) {
        s.push_str(&fold(&format!("DESCRIPTION:{}", escape(d))));
    }
    if let Some(l) = e.location.as_deref().filter(|l| !l.is_empty()) {
        s.push_str(&fold(&format!("LOCATION:{}", escape(l))));
    }
    for l in ["END:VEVENT", "END:VCALENDAR"] {
        s.push_str(&fold(l));
    }
    s
}

/// Replaces the simple properties of the first VEVENT, keeping everything else (alarms,
/// attendees, recurrence, custom properties) exactly as the client wrote them.
///
/// Timed events whose DTSTART named a known time zone keep it, so recurring series continue
/// to follow local time across daylight-saving changes.
pub fn patch_event(data: &str, e: &EventInput) -> String {
    let lines = unfold(data);
    let end = e.end.unwrap_or(if e.all_day {
        e.start + chrono::Duration::days(1)
    } else {
        e.start + chrono::Duration::hours(1)
    });
    let zone = parse_object(data)
        .ok()
        .and_then(|p| p.tzid)
        .map(|z| crate::recur::Zone::from_tzid(&z))
        .filter(|_| !e.all_day);
    let stamp = |name: &str, t: &DateTime<Utc>| match zone {
        Some(z @ crate::recur::Zone::Tz(_)) => format!("{name};TZID={}:{}", z.name().unwrap_or("UTC"), z.to_local(*t).format("%Y%m%dT%H%M%S")),
        _ if e.all_day => format!("{name};VALUE=DATE:{}", ical_time(t, true)),
        _ => format!("{name}:{}", ical_time(t, false)),
    };
    let mut out = String::new();
    let mut in_event = false;
    let mut done_event = false;
    for line in &lines {
        let (name, _, value) = split_prop(line);
        if name == "BEGIN" && value.eq_ignore_ascii_case("VEVENT") && !done_event {
            in_event = true;
            out.push_str(&fold(line));
            out.push_str(&fold(&stamp("DTSTART", &e.start)));
            out.push_str(&fold(&stamp("DTEND", &end)));
            out.push_str(&fold(&format!("SUMMARY:{}", escape(&e.summary))));
            if let Some(d) = e.description.as_deref().filter(|d| !d.is_empty()) {
                out.push_str(&fold(&format!("DESCRIPTION:{}", escape(d))));
            }
            if let Some(l) = e.location.as_deref().filter(|l| !l.is_empty()) {
                out.push_str(&fold(&format!("LOCATION:{}", escape(l))));
            }
            continue;
        }
        if in_event && name == "END" && value.eq_ignore_ascii_case("VEVENT") {
            in_event = false;
            done_event = true;
        }
        if in_event
            && matches!(
                name.as_str(),
                "DTSTART" | "DTEND" | "DURATION" | "SUMMARY" | "DESCRIPTION" | "LOCATION" | "DTSTAMP"
            )
        {
            continue;
        }
        if in_event && name == "SEQUENCE" {
            let n: i64 = value.trim().parse().unwrap_or(0);
            out.push_str(&fold(&format!("SEQUENCE:{}", n + 1)));
            continue;
        }
        out.push_str(&fold(line));
        if in_event && name == "UID" {
            out.push_str(&fold(&format!("DTSTAMP:{}", ical_time(&now(), false))));
        }
    }
    out
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ContactInput {
    pub name: String,
    #[serde(default)]
    pub emails: Vec<String>,
    #[serde(default)]
    pub phones: Vec<String>,
    #[serde(default)]
    pub org: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    /// Address book uri (default: the first address book).
    #[serde(default)]
    pub addressbook: Option<String>,
}

/// Replaces the fields of a vCard that the web UI edits, keeping all other properties
/// (photos, addresses, birthdays, custom fields) as the client wrote them.
pub fn patch_contact(data: &str, c: &ContactInput) -> String {
    let fresh = build_contact("", c);
    let replaced = ["FN", "N", "EMAIL", "TEL", "ORG", "NOTE", "REV"];
    let new_props: Vec<String> = unfold(&fresh)
        .into_iter()
        .filter(|l| {
            let (name, _, _) = split_prop(l);
            replaced.contains(&name.as_str())
        })
        .collect();
    let mut out = String::new();
    for line in unfold(data) {
        let (name, _, value) = split_prop(&line);
        if replaced.contains(&name.as_str()) {
            continue;
        }
        if name == "END" && value.trim().eq_ignore_ascii_case("VCARD") {
            for l in &new_props {
                out.push_str(&fold(l));
            }
        }
        out.push_str(&fold(&line));
    }
    out
}

pub fn build_contact(uid: &str, c: &ContactInput) -> String {
    let mut s = String::new();
    for l in ["BEGIN:VCARD", "VERSION:3.0"] {
        s.push_str(&fold(l));
    }
    s.push_str(&fold(&format!("UID:{uid}")));
    s.push_str(&fold(&format!("FN:{}", escape(&c.name))));
    let mut parts = c.name.rsplitn(2, ' ');
    let last = parts.next().unwrap_or("");
    let first = parts.next().unwrap_or("");
    s.push_str(&fold(&format!("N:{};{};;;", escape(last), escape(first))));
    for e in &c.emails {
        s.push_str(&fold(&format!("EMAIL;TYPE=INTERNET:{}", e.trim())));
    }
    for p in &c.phones {
        s.push_str(&fold(&format!("TEL:{}", p.trim())));
    }
    if let Some(o) = c.org.as_deref().filter(|o| !o.is_empty()) {
        s.push_str(&fold(&format!("ORG:{}", escape(o))));
    }
    if let Some(n) = c.note.as_deref().filter(|n| !n.is_empty()) {
        s.push_str(&fold(&format!("NOTE:{}", escape(n))));
    }
    s.push_str(&fold(&format!("REV:{}", ical_time(&now(), false))));
    s.push_str(&fold("END:VCARD"));
    s
}

/// An event or task as shown to the web UI and AI.
#[derive(Debug, Clone, Serialize)]
pub struct EventView {
    pub id: String,
    pub calendar: String,
    pub calendar_name: String,
    pub color: String,
    pub uid: String,
    pub component: String,
    pub summary: String,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub all_day: bool,
    pub recurring: bool,
    /// For an occurrence of a recurring event: the start it was generated for.
    pub recurrence_id: Option<DateTime<Utc>>,
    /// For an occurrence: the series' first start and end (what editing the series changes).
    pub series_start: Option<DateTime<Utc>>,
    pub series_end: Option<DateTime<Utc>>,
    /// IANA time zone the event was written in, if any.
    pub tzid: Option<String>,
    pub description: String,
    pub location: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContactView {
    pub id: String,
    pub uid: String,
    pub name: String,
    pub emails: Vec<String>,
    pub phones: Vec<String>,
    pub org: String,
    pub note: String,
}

fn view_of(o: &DavObject, c: &Collection) -> EventView {
    let p = parse_object(&o.data).unwrap_or_default();
    EventView {
        id: o.id.clone(),
        calendar: c.uri.clone(),
        calendar_name: c.name.clone(),
        color: c.color.clone(),
        uid: o.uid.clone(),
        component: o.component.clone(),
        summary: o.summary.clone(),
        starts_at: o.starts_at,
        ends_at: o.ends_at,
        all_day: o.all_day,
        recurring: o.recurring,
        recurrence_id: None,
        series_start: None,
        series_end: None,
        tzid: p.tzid,
        description: p.description,
        location: p.location,
    }
}

/// Most occurrences of one series returned for a single range query.
const MAX_OCCURRENCES: usize = 1000;

/// The views of `o` within `[from, to)`: its occurrences when it recurs, else itself.
fn views_in_range(o: &DavObject, c: &Collection, from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<EventView> {
    let base = view_of(o, c);
    if !o.recurring {
        return vec![base];
    }
    match crate::recur::expand(&o.data, from, to, MAX_OCCURRENCES) {
        Some(occurrences) => occurrences
            .into_iter()
            .map(|oc| EventView {
                summary: oc.summary.unwrap_or_else(|| base.summary.clone()),
                description: oc.description.unwrap_or_else(|| base.description.clone()),
                location: oc.location.unwrap_or_else(|| base.location.clone()),
                starts_at: Some(oc.start),
                ends_at: Some(oc.end),
                recurrence_id: Some(oc.recurrence_id),
                series_start: base.starts_at,
                series_end: base.ends_at,
                ..base.clone()
            })
            .collect(),
        // A rule this server cannot expand: show the first occurrence as stored.
        None => vec![base],
    }
}

fn contact_view(o: DavObject) -> Result<ContactView> {
    let p = parse_object(&o.data)?;
    Ok(ContactView {
        id: o.id,
        uid: o.uid,
        name: p.summary,
        emails: p.emails,
        phones: p.phones,
        org: p.org,
        note: p.description,
    })
}

impl Core {
    /// The user's collections of a kind, creating the default one on first use.
    pub async fn dav_collections(&self, user_id: &str, kind: &str) -> Result<Vec<Collection>> {
        let rows: Vec<Collection> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {COL_COLS} FROM dav_collections WHERE owner_id = ? AND kind = ? ORDER BY created_at"
        )))
        .bind(user_id)
        .bind(kind)
        .fetch_all(&self.db)
        .await?;
        if !rows.is_empty() {
            return Ok(rows);
        }
        let (uri, name, comps) = if kind == KIND_CALENDAR {
            ("personal", "Personal", "VEVENT,VTODO")
        } else {
            ("contacts", "Contacts", "VCARD")
        };
        self.create_dav_collection(user_id, kind, uri, name, None, Some(comps)).await?;
        Box::pin(self.dav_collections(user_id, kind)).await
    }

    pub async fn dav_collection(&self, user_id: &str, kind: &str, uri: &str) -> Result<Collection> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {COL_COLS} FROM dav_collections WHERE owner_id = ? AND kind = ? AND uri = ?"
        )))
        .bind(user_id)
        .bind(kind)
        .bind(uri)
        .fetch_one(&self.db)
        .await?)
    }

    pub async fn create_dav_collection(
        &self,
        user_id: &str,
        kind: &str,
        uri: &str,
        name: &str,
        color: Option<&str>,
        components: Option<&str>,
    ) -> Result<Collection> {
        if !matches!(kind, KIND_CALENDAR | KIND_ADDRESSBOOK) {
            return Err(Error::bad("kind must be calendar or addressbook"));
        }
        crate::util::validate_name(uri)?;
        let id = new_id();
        sqlx::query("INSERT INTO dav_collections (id, owner_id, kind, uri, name, color, components, ctag, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(&id)
            .bind(user_id)
            .bind(kind)
            .bind(uri)
            .bind(if name.trim().is_empty() { uri } else { name.trim() })
            .bind(color.unwrap_or("#c2410c"))
            .bind(components.unwrap_or(if kind == KIND_CALENDAR { "VEVENT,VTODO" } else { "VCARD" }))
            .bind(short_token(16))
            .bind(now())
            .execute(&self.db)
            .await?;
        self.collections_changed(user_id, kind);
        self.dav_collection(user_id, kind, uri).await
    }

    fn collections_changed(&self, owner_id: &str, kind: &str) {
        self.hub
            .publish([owner_id], &serde_json::json!({"type": "groupware.changed", "kind": kind, "collections": true}));
    }

    pub async fn update_dav_collection(&self, c: &Collection, name: Option<&str>, color: Option<&str>) -> Result<()> {
        if let Some(n) = name.filter(|n| !n.trim().is_empty()) {
            sqlx::query("UPDATE dav_collections SET name = ? WHERE id = ?")
                .bind(n.trim())
                .bind(&c.id)
                .execute(&self.db)
                .await?;
        }
        if let Some(col) = color.filter(|c| c.starts_with('#') && c.len() <= 9) {
            sqlx::query("UPDATE dav_collections SET color = ? WHERE id = ?")
                .bind(col)
                .bind(&c.id)
                .execute(&self.db)
                .await?;
        }
        self.touch_collection(&c.id).await?;
        self.collections_changed(&c.owner_id, &c.kind);
        Ok(())
    }

    pub async fn delete_dav_collection(&self, c: &Collection) -> Result<()> {
        sqlx::query("DELETE FROM fts WHERE object_kind IN ('event', 'contact') AND object_id IN (SELECT id FROM dav_objects WHERE collection_id = ?)")
            .bind(&c.id)
            .execute(&self.db)
            .await?;
        sqlx::query("DELETE FROM dav_collections WHERE id = ?").bind(&c.id).execute(&self.db).await?;
        self.collections_changed(&c.owner_id, &c.kind);
        Ok(())
    }

    async fn touch_collection(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE dav_collections SET ctag = ? WHERE id = ?")
            .bind(short_token(16))
            .bind(id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    pub async fn dav_objects(&self, c: &Collection) -> Result<Vec<DavObject>> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {OBJ_COLS} FROM dav_objects WHERE collection_id = ? ORDER BY uri"
        )))
        .bind(&c.id)
        .fetch_all(&self.db)
        .await?)
    }

    /// Objects overlapping [from, to) (recurring ones always match).
    pub async fn dav_objects_in_range(&self, c: &Collection, from: Option<DateTime<Utc>>, to: Option<DateTime<Utc>>) -> Result<Vec<DavObject>> {
        let from = from.unwrap_or(DateTime::<Utc>::MIN_UTC);
        let to = to.unwrap_or(DateTime::<Utc>::MAX_UTC);
        let all = self.dav_objects(c).await?;
        Ok(all
            .into_iter()
            .filter(|o| {
                o.recurring
                    || match (o.starts_at, o.ends_at) {
                        (Some(s), e) => s < to && e.unwrap_or(s) >= from,
                        // Tasks without dates are always relevant.
                        (None, Some(e)) => e >= from,
                        (None, None) => true,
                    }
            })
            .collect())
    }

    pub async fn dav_object(&self, c: &Collection, uri: &str) -> Result<DavObject> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {OBJ_COLS} FROM dav_objects WHERE collection_id = ? AND uri = ?"
        )))
        .bind(&c.id)
        .bind(uri)
        .fetch_one(&self.db)
        .await?)
    }

    /// Creates or replaces an object. Returns (object, created).
    pub async fn put_dav_object(&self, c: &Collection, uri: &str, data: &str, if_match: Option<&str>, if_none_match: bool) -> Result<(DavObject, bool)> {
        crate::util::validate_name(uri)?;
        if data.len() > MAX_OBJECT {
            return Err(Error::bad("object too large"));
        }
        let p = parse_object(data)?;
        let allowed = if c.kind == KIND_ADDRESSBOOK {
            p.component == "VCARD"
        } else {
            c.components.split(',').any(|x| x == p.component)
        };
        if !allowed {
            return Err(Error::Forbidden);
        }
        let existing = self.dav_object(c, uri).await.ok();
        match (&existing, if_match, if_none_match) {
            (Some(_), _, true) => return Err(Error::PreconditionFailed),
            (Some(o), Some(m), _) if m.trim_matches('"') != o.etag && m != "*" => return Err(Error::PreconditionFailed),
            (None, Some(_), _) => return Err(Error::PreconditionFailed),
            _ => {}
        }
        // A UID may appear only once per collection (RFC 4791 no-uid-conflict).
        let clash: Option<String> = sqlx::query_scalar("SELECT uri FROM dav_objects WHERE collection_id = ? AND uid = ? AND uri != ?")
            .bind(&c.id)
            .bind(&p.uid)
            .bind(uri)
            .fetch_optional(&self.db)
            .await?;
        if clash.is_some() {
            return Err(Error::conflict("an object with this UID already exists in the collection"));
        }
        let etag = short_token(16);
        let t = now();
        let id = existing.as_ref().map(|o| o.id.clone()).unwrap_or_else(new_id);
        sqlx::query(
            "INSERT INTO dav_objects (id, collection_id, uri, uid, etag, component, summary, starts_at, ends_at, all_day, recurring, data, modified_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(collection_id, uri) DO UPDATE SET uid = excluded.uid, etag = excluded.etag, component = excluded.component,
               summary = excluded.summary, starts_at = excluded.starts_at, ends_at = excluded.ends_at, all_day = excluded.all_day,
               recurring = excluded.recurring, data = excluded.data, modified_at = excluded.modified_at",
        )
        .bind(&id)
        .bind(&c.id)
        .bind(uri)
        .bind(&p.uid)
        .bind(&etag)
        .bind(&p.component)
        .bind(&p.summary)
        .bind(p.starts_at)
        .bind(p.ends_at)
        .bind(p.all_day)
        .bind(p.recurring)
        .bind(data)
        .bind(t)
        .execute(&self.db)
        .await?;
        self.touch_collection(&c.id).await?;
        let kind = if p.component == "VCARD" { "contact" } else { "event" };
        sqlx::query("DELETE FROM fts WHERE object_kind = ? AND object_id = ?")
            .bind(kind)
            .bind(&id)
            .execute(&self.db)
            .await?;
        let body = [
            p.description.as_str(),
            p.location.as_str(),
            &p.emails.join(" "),
            &p.phones.join(" "),
            p.org.as_str(),
        ]
        .join(" ");
        sqlx::query("INSERT INTO fts (object_kind, object_id, scope, title, body) VALUES (?, ?, ?, ?, ?)")
            .bind(kind)
            .bind(&id)
            .bind(&c.owner_id)
            .bind(&p.summary)
            .bind(body)
            .execute(&self.db)
            .await?;
        self.hub
            .publish([c.owner_id.as_str()], &serde_json::json!({"type": "groupware.changed", "kind": c.kind}));
        Ok((self.dav_object(c, uri).await?, existing.is_none()))
    }

    pub async fn delete_dav_object(&self, c: &Collection, uri: &str, if_match: Option<&str>) -> Result<()> {
        let o = self.dav_object(c, uri).await?;
        if if_match.is_some_and(|m| m.trim_matches('"') != o.etag && m != "*") {
            return Err(Error::PreconditionFailed);
        }
        sqlx::query("DELETE FROM dav_objects WHERE id = ?").bind(&o.id).execute(&self.db).await?;
        sqlx::query("DELETE FROM fts WHERE object_kind IN ('event', 'contact') AND object_id = ?")
            .bind(&o.id)
            .execute(&self.db)
            .await?;
        self.touch_collection(&c.id).await?;
        self.hub
            .publish([c.owner_id.as_str()], &serde_json::json!({"type": "groupware.changed", "kind": c.kind}));
        Ok(())
    }

    // ---- convenience API for the web UI and MCP tools ----------------------------

    pub async fn events_between(&self, user_id: &str, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<EventView>> {
        let mut out = Vec::new();
        for c in self.dav_collections(user_id, KIND_CALENDAR).await? {
            for o in self.dav_objects_in_range(&c, Some(from), Some(to)).await? {
                out.extend(views_in_range(&o, &c, from, to));
            }
        }
        out.sort_by_key(|e| e.starts_at);
        Ok(out)
    }

    async fn calendar_for(&self, user_id: &str, uri: Option<&str>) -> Result<Collection> {
        let cals = self.dav_collections(user_id, KIND_CALENDAR).await?;
        match uri {
            Some(u) => cals.into_iter().find(|c| c.uri == u || c.name.eq_ignore_ascii_case(u)).ok_or(Error::NotFound),
            None => cals.into_iter().next().ok_or(Error::NotFound),
        }
    }

    async fn object_by_id(&self, user_id: &str, id: &str) -> Result<(Collection, DavObject)> {
        let o: DavObject = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {OBJ_COLS} FROM dav_objects WHERE id = ?")))
            .bind(id)
            .fetch_one(&self.db)
            .await?;
        let c: Collection = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {COL_COLS} FROM dav_collections WHERE id = ?")))
            .bind(&o.collection_id)
            .fetch_one(&self.db)
            .await?;
        if c.owner_id != user_id {
            return Err(Error::NotFound);
        }
        Ok((c, o))
    }

    pub async fn create_event(&self, user_id: &str, e: &EventInput) -> Result<EventView> {
        if e.summary.trim().is_empty() {
            return Err(Error::bad("summary required"));
        }
        if e.end.is_some_and(|end| end < e.start) {
            return Err(Error::bad("end must not be before start"));
        }
        let c = self.calendar_for(user_id, e.calendar.as_deref()).await?;
        let uid = format!("{}@ferrite", new_id());
        let (o, _) = self.put_dav_object(&c, &format!("{uid}.ics"), &build_event(&uid, e), None, true).await?;
        Ok(view_of(&o, &c))
    }

    /// Edits an event (for a recurring event, the whole series). When `e.calendar` names a
    /// different calendar, the event moves there.
    pub async fn update_event(&self, user_id: &str, id: &str, e: &EventInput) -> Result<EventView> {
        let (c, o) = self.object_by_id(user_id, id).await?;
        if o.component != "VEVENT" {
            return Err(Error::bad("not an event"));
        }
        if e.summary.trim().is_empty() {
            return Err(Error::bad("summary required"));
        }
        if e.end.is_some_and(|end| end < e.start) {
            return Err(Error::bad("end must not be before start"));
        }
        let data = patch_event(&o.data, e);
        let target = match e.calendar.as_deref() {
            Some(uri) if uri != c.uri => Some(self.calendar_for(user_id, Some(uri)).await?),
            _ => None,
        };
        match target {
            Some(to) => {
                if !to.components.split(',').any(|x| x == "VEVENT") {
                    return Err(Error::bad("that calendar does not hold events"));
                }
                let (moved, _) = self.put_dav_object(&to, &o.uri, &data, None, true).await?;
                self.delete_dav_object(&c, &o.uri, Some(&o.etag)).await?;
                // Keep the event's id across the move, so links and tool calls holding it work.
                let mut tx = self.db.begin().await?;
                sqlx::query("UPDATE dav_objects SET id = ? WHERE id = ?")
                    .bind(&o.id)
                    .bind(&moved.id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("UPDATE fts SET object_id = ? WHERE object_kind = 'event' AND object_id = ?")
                    .bind(&o.id)
                    .bind(&moved.id)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                let (to, moved) = self.object_by_id(user_id, &o.id).await?;
                Ok(view_of(&moved, &to))
            }
            None => {
                let (o, _) = self.put_dav_object(&c, &o.uri, &data, Some(&o.etag), false).await?;
                Ok(view_of(&o, &c))
            }
        }
    }

    pub async fn delete_event(&self, user_id: &str, id: &str) -> Result<()> {
        let (c, o) = self.object_by_id(user_id, id).await?;
        if c.kind != KIND_CALENDAR {
            return Err(Error::NotFound);
        }
        self.delete_dav_object(&c, &o.uri, None).await
    }

    pub async fn contacts(&self, user_id: &str, query: Option<&str>) -> Result<Vec<ContactView>> {
        let q = query.map(str::to_lowercase);
        let mut out = Vec::new();
        for c in self.dav_collections(user_id, KIND_ADDRESSBOOK).await? {
            for o in self.dav_objects(&c).await? {
                let p = parse_object(&o.data).unwrap_or_default();
                let v = ContactView {
                    id: o.id,
                    uid: o.uid,
                    name: p.summary,
                    emails: p.emails,
                    phones: p.phones,
                    org: p.org,
                    note: p.description,
                };
                let hay = format!("{} {} {} {}", v.name, v.emails.join(" "), v.phones.join(" "), v.org).to_lowercase();
                if q.as_deref().is_none_or(|q| hay.contains(q)) {
                    out.push(v);
                }
            }
        }
        out.sort_by_key(|c| c.name.to_lowercase());
        Ok(out)
    }

    pub async fn create_contact(&self, user_id: &str, input: &ContactInput) -> Result<ContactView> {
        if input.name.trim().is_empty() {
            return Err(Error::bad("name required"));
        }
        let books = self.dav_collections(user_id, KIND_ADDRESSBOOK).await?;
        let c = match input.addressbook.as_deref() {
            Some(uri) => books.into_iter().find(|b| b.uri == uri).ok_or(Error::NotFound)?,
            None => books.into_iter().next().ok_or(Error::NotFound)?,
        };
        let uid = format!("{}@ferrite", new_id());
        let (o, _) = self.put_dav_object(&c, &format!("{uid}.vcf"), &build_contact(&uid, input), None, true).await?;
        contact_view(o)
    }

    /// Edits a contact's name, emails, phones, organisation and note, keeping its other fields.
    pub async fn update_contact(&self, user_id: &str, id: &str, input: &ContactInput) -> Result<ContactView> {
        if input.name.trim().is_empty() {
            return Err(Error::bad("name required"));
        }
        let (c, o) = self.object_by_id(user_id, id).await?;
        if c.kind != KIND_ADDRESSBOOK {
            return Err(Error::NotFound);
        }
        let (o, _) = self.put_dav_object(&c, &o.uri, &patch_contact(&o.data, input), Some(&o.etag), false).await?;
        contact_view(o)
    }

    pub async fn delete_contact(&self, user_id: &str, id: &str) -> Result<()> {
        let (c, o) = self.object_by_id(user_id, id).await?;
        if c.kind != KIND_ADDRESSBOOK {
            return Err(Error::NotFound);
        }
        self.delete_dav_object(&c, &o.uri, None).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVENT: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTIMEZONE\r\nTZID:Europe/Berlin\r\nBEGIN:STANDARD\r\nDTSTART:19701025T030000\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:abc-123\r\nDTSTART;TZID=Europe/Berlin:20261105T090000\r\nDURATION:PT1H30M\r\nSUMMARY:Planning\\, Q4\r\nDESCRIPTION:Line one\\nLine two that is quite long and therefore gets folded by the\r\n  client at seventy five octets\r\nRRULE:FREQ=WEEKLY\r\nBEGIN:VALARM\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn parses_events() {
        let p = parse_object(EVENT).unwrap();
        assert_eq!(p.component, "VEVENT");
        assert_eq!(p.uid, "abc-123");
        assert_eq!(p.summary, "Planning, Q4");
        assert!(p.description.ends_with("seventy five octets"));
        assert!(p.recurring);
        // 09:00 in Berlin in November is CET (UTC+1).
        assert_eq!(p.starts_at.unwrap().to_rfc3339(), "2026-11-05T08:00:00+00:00");
        assert_eq!(p.ends_at.unwrap().to_rfc3339(), "2026-11-05T09:30:00+00:00");
        assert_eq!(p.tzid.as_deref(), Some("Europe/Berlin"));
        let day = parse_object("BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:d\nDTSTART;VALUE=DATE:20261224\nSUMMARY:Eve\nEND:VEVENT\nEND:VCALENDAR\n").unwrap();
        assert!(day.all_day);
        assert_eq!(day.ends_at.unwrap().to_rfc3339(), "2026-12-25T00:00:00+00:00");
        assert!(
            parse_object("BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:x\nEND:VEVENT\nEND:VCALENDAR").is_err(),
            "UID required"
        );
        assert!(parse_object("hello").is_err());
    }

    #[test]
    fn parses_vcards() {
        let p = parse_object("BEGIN:VCARD\nVERSION:3.0\nUID:u1\nFN:Ada Lovelace\nitem1.EMAIL;TYPE=INTERNET:ada@example.org\nTEL;TYPE=CELL:+44 1\nORG:Analytical Engines;R&D\nEND:VCARD\n").unwrap();
        assert_eq!((p.component.as_str(), p.summary.as_str()), ("VCARD", "Ada Lovelace"));
        assert_eq!(p.emails, vec!["ada@example.org"]);
        assert_eq!(p.org, "Analytical Engines R&D");
    }

    #[test]
    fn builds_round_trip_and_patches_preserving_extras() {
        let start = "2026-11-02T15:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let e = EventInput {
            summary: "Café; review".into(),
            start,
            end: None,
            description: Some("bring\nslides".into()),
            ..Default::default()
        };
        let ics = build_event("u-1", &e);
        assert!(ics.lines().all(|l| l.len() <= 76));
        let p = parse_object(&ics).unwrap();
        assert_eq!(p.summary, "Café; review");
        assert_eq!(p.description, "bring\nslides");
        assert_eq!(p.ends_at.unwrap() - p.starts_at.unwrap(), chrono::Duration::hours(1));
        let moved = EventInput {
            summary: "Planning".into(),
            start: start + chrono::Duration::days(1),
            end: None,
            ..Default::default()
        };
        let patched = patch_event(EVENT, &moved);
        let p2 = parse_object(&patched).unwrap();
        assert_eq!(p2.summary, "Planning");
        assert_eq!(p2.starts_at.unwrap(), start + chrono::Duration::days(1));
        assert!(patched.contains("RRULE:FREQ=WEEKLY") && patched.contains("TRIGGER:-PT15M") && patched.contains("TZID:Europe/Berlin"));
        let card = build_contact(
            "c1",
            &ContactInput {
                name: "Grace Hopper".into(),
                emails: vec!["grace@navy.mil".into()],
                ..Default::default()
            },
        );
        assert_eq!(parse_object(&card).unwrap().emails, vec!["grace@navy.mil"]);
        assert_eq!(parse_duration("P1DT2H").unwrap(), 93600);
    }

    #[test]
    fn patching_keeps_the_event_time_zone() {
        // Move the Berlin series to 10:00 local (09:00Z in November): stays a TZID time.
        let start = "2026-11-05T09:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let e = EventInput {
            summary: "Planning".into(),
            start,
            end: Some(start + chrono::Duration::hours(1)),
            ..Default::default()
        };
        let patched = patch_event(EVENT, &e);
        assert!(patched.contains("DTSTART;TZID=Europe/Berlin:20261105T100000"), "{patched}");
        assert!(patched.contains("RRULE:FREQ=WEEKLY") && patched.contains("TRIGGER:-PT15M"));
        assert_eq!(parse_object(&patched).unwrap().starts_at, Some(start));
    }

    #[test]
    fn patching_contacts_keeps_other_fields() {
        let card = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:u1\r\nFN:Ada\r\nEMAIL:old@example.org\r\nBDAY:1815-12-10\r\nPHOTO;VALUE=URI:https://example.org/ada.png\r\nEND:VCARD\r\n";
        let c = ContactInput {
            name: "Ada Lovelace".into(),
            emails: vec!["ada@example.org".into()],
            org: Some("Engines".into()),
            ..Default::default()
        };
        let out = patch_contact(card, &c);
        let p = parse_object(&out).unwrap();
        assert_eq!(
            (p.summary.as_str(), p.emails.clone(), p.org.as_str(), p.uid.as_str()),
            ("Ada Lovelace", vec!["ada@example.org".to_string()], "Engines", "u1")
        );
        assert!(out.contains("BDAY:1815-12-10") && out.contains("PHOTO;VALUE=URI:https://example.org/ada.png"));
        assert!(!out.contains("old@example.org"));
    }
}
