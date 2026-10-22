//! iCalendar component trees and iTIP (RFC 5546) messages for calendar scheduling.
//!
//! Stored objects are parsed into a small tree of components and properties that serializes
//! back losslessly enough for scheduling: property lines keep their parameters exactly as
//! written (quoting included), and only the properties scheduling touches are rewritten.
//! The helpers here are pure: they read organizers and attendees, detect significant changes
//! and reschedules, update participation status, and build the REQUEST, CANCEL and REPLY
//! messages that `scheduling.rs` delivers internally or by email (iMIP, RFC 6047).

use crate::groupware::{fold, unescape, unfold};
use crate::util::now;
use crate::{Error, Result};
use std::collections::{BTreeMap, BTreeSet};

/// One content line: NAME;PARAM=value;...:value. Parameter values are kept as written
/// (including double quotes) so untouched parameters round-trip exactly.
#[derive(Debug, Clone, PartialEq)]
pub struct Prop {
    pub name: String,
    pub params: Vec<(String, String)>,
    pub value: String,
}

fn unquote(v: &str) -> &str {
    v.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(v)
}

impl Prop {
    pub fn new(name: &str, value: impl Into<String>) -> Prop {
        Prop {
            name: name.to_ascii_uppercase(),
            params: Vec::new(),
            value: value.into(),
        }
    }

    pub fn parse(line: &str) -> Prop {
        let mut in_quotes = false;
        let mut cuts = Vec::new();
        let mut colon = line.len();
        for (i, c) in line.char_indices() {
            match c {
                '"' => in_quotes = !in_quotes,
                ';' if !in_quotes => cuts.push(i),
                ':' if !in_quotes => {
                    colon = i;
                    break;
                }
                _ => {}
            }
        }
        let head = &line[..colon];
        let value = line.get(colon + 1..).unwrap_or("").to_string();
        let mut bounds = vec![0];
        bounds.extend(cuts.iter().copied().filter(|&c| c < colon));
        bounds.push(head.len());
        let name = head[..bounds[1]].trim().to_ascii_uppercase();
        let params = bounds
            .windows(2)
            .skip(1)
            .filter_map(|w| head.get(w[0] + 1..w[1]))
            .filter_map(|p| p.split_once('='))
            .map(|(k, v)| (k.trim().to_ascii_uppercase(), v.to_string()))
            .collect();
        Prop { name, params, value }
    }

    /// A parameter's value without surrounding quotes.
    pub fn param(&self, key: &str) -> Option<&str> {
        self.params.iter().find(|(k, _)| k == key).map(|(_, v)| unquote(v))
    }

    pub fn set_param(&mut self, key: &str, value: &str) {
        let clean: String = value.chars().filter(|c| *c != '"' && !c.is_control()).collect();
        let v = if clean.contains([':', ';', ',']) { format!("\"{clean}\"") } else { clean };
        match self.params.iter_mut().find(|(k, _)| k == key) {
            Some(p) => p.1 = v,
            None => self.params.push((key.to_string(), v)),
        }
    }

    pub fn remove_param(&mut self, key: &str) {
        self.params.retain(|(k, _)| k != key);
    }

    pub fn to_line(&self) -> String {
        let mut s = self.name.clone();
        for (k, v) in &self.params {
            s.push(';');
            s.push_str(k);
            s.push('=');
            s.push_str(v);
        }
        s.push(':');
        s.push_str(&self.value);
        s
    }

    /// The calendar user address of an ORGANIZER/ATTENDEE, normalized (lowercase, no mailto:).
    pub fn address(&self) -> String {
        normalize_address(&self.value)
    }
}

/// "mailto:Ada@Example.org" → "ada@example.org".
pub fn normalize_address(v: &str) -> String {
    let v = v.trim();
    let v = if v.len() >= 7 && v[..7].eq_ignore_ascii_case("mailto:") { &v[7..] } else { v };
    v.trim().to_ascii_lowercase()
}

/// A plausible email address (one '@', a dot in the domain, no spaces or brackets).
pub fn valid_email(e: &str) -> bool {
    let Some((local, domain)) = e.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains('@')
        && !e.chars().any(|c| c.is_whitespace() || c.is_control() || "<>\"(),;:[]\\".contains(c))
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Component {
    pub name: String,
    pub props: Vec<Prop>,
    pub subs: Vec<Component>,
}

impl Component {
    pub fn new(name: &str) -> Component {
        Component {
            name: name.to_string(),
            ..Default::default()
        }
    }

    /// Parses an iCalendar text into its VCALENDAR component.
    pub fn parse(text: &str) -> Result<Component> {
        let mut stack: Vec<Component> = Vec::new();
        let mut root = None;
        for line in unfold(text) {
            let p = Prop::parse(&line);
            match p.name.as_str() {
                "BEGIN" => stack.push(Component::new(&p.value.trim().to_ascii_uppercase())),
                "END" => {
                    let done = stack.pop().ok_or_else(|| Error::bad("unbalanced END in iCalendar data"))?;
                    match stack.last_mut() {
                        Some(parent) => parent.subs.push(done),
                        None => {
                            root = Some(done);
                            break;
                        }
                    }
                }
                _ => {
                    if let Some(cur) = stack.last_mut() {
                        cur.props.push(p);
                    }
                }
            }
        }
        match root {
            Some(r) if r.name == "VCALENDAR" => Ok(r),
            _ => Err(Error::bad("not an iCalendar object")),
        }
    }

    pub fn to_ics(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        out.push_str(&fold(&format!("BEGIN:{}", self.name)));
        for p in &self.props {
            out.push_str(&fold(&p.to_line()));
        }
        for s in &self.subs {
            s.write(out);
        }
        out.push_str(&fold(&format!("END:{}", self.name)));
    }

    pub fn get(&self, name: &str) -> Option<&Prop> {
        self.props.iter().find(|p| p.name == name)
    }

    pub fn value(&self, name: &str) -> Option<&str> {
        self.get(name).map(|p| p.value.as_str())
    }

    pub fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Prop> + 'a {
        self.props.iter().filter(move |p| p.name == name)
    }

    /// Replaces every `prop.name` property with `prop` (at the first one's position).
    pub fn set(&mut self, prop: Prop) {
        match self.props.iter().position(|p| p.name == prop.name) {
            Some(i) => {
                self.props[i] = prop.clone();
                let name = prop.name;
                let mut seen = false;
                self.props.retain(|p| {
                    if p.name != name {
                        return true;
                    }
                    let keep = !seen;
                    seen = true;
                    keep
                });
            }
            None => self.props.push(prop),
        }
    }

    pub fn remove(&mut self, name: &str) {
        self.props.retain(|p| p.name != name);
    }

    /// The scheduling components (VEVENT/VTODO) of a VCALENDAR.
    pub fn items(&self) -> impl Iterator<Item = &Component> {
        self.subs.iter().filter(|c| is_item(c))
    }

    pub fn items_mut(&mut self) -> impl Iterator<Item = &mut Component> {
        self.subs.iter_mut().filter(|c| is_item(c))
    }

    /// The component without RECURRENCE-ID (or the first one).
    pub fn master(&self) -> Option<&Component> {
        self.items().find(|c| c.get("RECURRENCE-ID").is_none()).or_else(|| self.items().next())
    }

    pub fn master_mut(&mut self) -> Option<&mut Component> {
        let idx = self
            .subs
            .iter()
            .position(|c| is_item(c) && c.get("RECURRENCE-ID").is_none())
            .or_else(|| self.subs.iter().position(is_item))?;
        self.subs.get_mut(idx)
    }
}

fn is_item(c: &Component) -> bool {
    matches!(c.name.as_str(), "VEVENT" | "VTODO")
}

/// Key of a component within its series: "" for the master, else its RECURRENCE-ID.
fn instance_key(c: &Component) -> String {
    c.get("RECURRENCE-ID").map(|p| p.value.trim().to_string()).unwrap_or_default()
}

/// An ORGANIZER or ATTENDEE.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Party {
    pub email: String,
    pub name: Option<String>,
    pub partstat: String,
    pub role: String,
    /// RFC 6638 SCHEDULE-AGENT: SERVER (default), CLIENT or NONE (unknown values act as NONE).
    pub agent: String,
}

impl Party {
    fn of(p: &Prop) -> Party {
        Party {
            email: p.address(),
            name: p.param("CN").map(str::to_string).filter(|n| !n.trim().is_empty()),
            partstat: p.param("PARTSTAT").unwrap_or("NEEDS-ACTION").to_ascii_uppercase(),
            role: p.param("ROLE").unwrap_or("REQ-PARTICIPANT").to_ascii_uppercase(),
            agent: p.param("SCHEDULE-AGENT").unwrap_or("SERVER").to_ascii_uppercase(),
        }
    }

    /// Whether the server is responsible for scheduling messages to this calendar user.
    pub fn server_scheduled(&self) -> bool {
        self.agent == "SERVER"
    }

    /// "Name <email>" (or the address alone) for mail headers.
    pub fn mailbox(&self) -> String {
        match &self.name {
            Some(n) => format!("\"{}\" <{}>", n.replace(['"', '\\'], ""), self.email),
            None => self.email.clone(),
        }
    }

    pub fn display(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.email)
    }
}

pub fn organizer(cal: &Component) -> Option<Party> {
    cal.items().find_map(|c| c.get("ORGANIZER")).map(Party::of).filter(|p| !p.email.is_empty())
}

/// Every attendee of the series, once each (master first), excluding the organizer.
pub fn attendees(cal: &Component) -> Vec<Party> {
    let org = organizer(cal).map(|o| o.email).unwrap_or_default();
    let mut seen = BTreeSet::new();
    let mut items: Vec<&Component> = cal.items().collect();
    items.sort_by_key(|c| c.get("RECURRENCE-ID").is_some());
    let mut out = Vec::new();
    for c in items {
        for p in c.all("ATTENDEE") {
            let a = Party::of(p);
            if !a.email.is_empty() && a.email != org && seen.insert(a.email.clone()) {
                out.push(a);
            }
        }
    }
    out
}

/// The participation status of `email` in the series master (or the first component listing it).
pub fn partstat_of(cal: &Component, email: &str) -> Option<String> {
    let find = |c: &Component| c.all("ATTENDEE").find(|p| p.address() == email).map(|p| Party::of(p).partstat);
    cal.master().and_then(find).or_else(|| cal.items().find_map(find))
}

/// Participation status of `email` per component (keyed by RECURRENCE-ID, "" = master).
pub fn partstats(cal: &Component, email: &str) -> BTreeMap<String, String> {
    cal.items()
        .filter_map(|c| {
            c.all("ATTENDEE")
                .find(|p| p.address() == email)
                .map(|p| (instance_key(c), Party::of(p).partstat))
        })
        .collect()
}

pub fn sequence(cal: &Component) -> i64 {
    cal.items()
        .filter_map(|c| c.value("SEQUENCE").and_then(|v| v.trim().parse().ok()))
        .max()
        .unwrap_or(0)
}

pub fn set_sequence(cal: &mut Component, n: i64) {
    for c in cal.items_mut() {
        let at = c.props.iter().position(|p| p.name == "SEQUENCE");
        match at {
            Some(_) => c.set(Prop::new("SEQUENCE", n.to_string())),
            None => {
                let after_uid = c.props.iter().position(|p| p.name == "UID").map(|i| i + 1).unwrap_or(0);
                c.props.insert(after_uid, Prop::new("SEQUENCE", n.to_string()));
            }
        }
    }
}

pub fn uid(cal: &Component) -> Option<String> {
    cal.items().find_map(|c| c.value("UID")).map(|u| u.trim().to_string())
}

pub fn summary(cal: &Component) -> String {
    cal.master().and_then(|c| c.value("SUMMARY")).map(unescape).unwrap_or_default()
}

pub fn is_cancelled(cal: &Component) -> bool {
    cal.master()
        .and_then(|c| c.value("STATUS"))
        .is_some_and(|s| s.trim().eq_ignore_ascii_case("CANCELLED"))
}

/// Values of a property in comparable form: parameters that change meaning (TZID, VALUE)
/// plus the value; multi-valued properties sorted.
fn prop_signature(c: &Component, name: &str) -> Vec<String> {
    let mut v: Vec<String> = c
        .all(name)
        .flat_map(|p| {
            let tz = p.param("TZID").unwrap_or("").to_string();
            let kind = p.param("VALUE").unwrap_or("").to_ascii_uppercase();
            p.value
                .split(',')
                .map(move |x| format!("{tz}|{kind}|{}", x.trim().to_ascii_uppercase()))
                .collect::<Vec<_>>()
        })
        .collect();
    v.sort();
    v
}

const RESCHEDULE_PROPS: [&str; 7] = ["DTSTART", "DTEND", "DURATION", "DUE", "RRULE", "RDATE", "EXDATE"];
const DESCRIBE_PROPS: [&str; 2] = ["SUMMARY", "LOCATION"];

fn instances(cal: &Component) -> BTreeMap<String, &Component> {
    cal.items().map(|c| (instance_key(c), c)).collect()
}

/// A reschedule (RFC 6638 §3.2.8): the time or recurrence of any instance changed, or
/// instances were added or removed.
pub fn is_reschedule(old: &Component, new: &Component) -> bool {
    let (a, b) = (instances(old), instances(new));
    if a.keys().ne(b.keys()) {
        return true;
    }
    a.iter().any(|(k, oc)| {
        let nc = b[k];
        RESCHEDULE_PROPS.iter().any(|p| prop_signature(oc, p) != prop_signature(nc, p))
    })
}

/// A change attendees must hear about: a reschedule, or a new title or location.
pub fn is_significant(old: &Component, new: &Component) -> bool {
    if is_reschedule(old, new) {
        return true;
    }
    let (a, b) = (instances(old), instances(new));
    a.iter().any(|(k, oc)| {
        let nc = b[k];
        DESCRIBE_PROPS.iter().any(|p| oc.value(p).map(str::trim) != nc.value(p).map(str::trim))
    })
}

/// Resets every server-scheduled attendee (except the organizer) to NEEDS-ACTION.
pub fn reset_partstats(cal: &mut Component, organizer: &str) {
    for c in cal.items_mut() {
        for p in c.props.iter_mut().filter(|p| p.name == "ATTENDEE") {
            let party = Party::of(p);
            if party.email != organizer && party.server_scheduled() {
                p.set_param("PARTSTAT", "NEEDS-ACTION");
                p.set_param("RSVP", "TRUE");
            }
        }
    }
}

/// Sets `email`'s PARTSTAT on every component listing it. Returns whether anything changed.
pub fn set_partstat(cal: &mut Component, email: &str, partstat: &str) -> bool {
    let mut changed = false;
    for c in cal.items_mut() {
        for p in c.props.iter_mut().filter(|p| p.name == "ATTENDEE" && p.address() == email) {
            if p.param("PARTSTAT").map(str::to_ascii_uppercase).as_deref() != Some(partstat) {
                p.set_param("PARTSTAT", partstat);
                changed = true;
            }
            if partstat != "NEEDS-ACTION" && p.param("RSVP").is_some() {
                p.remove_param("RSVP");
                changed = true;
            }
        }
    }
    changed
}

/// Applies per-instance statuses (from `partstats`) to the matching components.
pub fn apply_partstats(cal: &mut Component, email: &str, statuses: &BTreeMap<String, String>) -> bool {
    let mut changed = false;
    for c in cal.items_mut() {
        let Some(ps) = statuses.get(&instance_key(c)) else { continue };
        for p in c.props.iter_mut().filter(|p| p.name == "ATTENDEE" && p.address() == email) {
            if p.param("PARTSTAT").map(str::to_ascii_uppercase).as_deref() != Some(ps.as_str()) {
                p.set_param("PARTSTAT", ps);
                p.remove_param("RSVP");
                changed = true;
            }
        }
    }
    changed
}

/// Sets SCHEDULE-STATUS on the ATTENDEE (or ORGANIZER) properties of `email`.
pub fn set_schedule_status(cal: &mut Component, prop: &str, email: &str, code: &str) {
    for c in cal.items_mut() {
        for p in c.props.iter_mut().filter(|p| p.name == prop && p.address() == email) {
            p.set_param("SCHEDULE-STATUS", code);
        }
    }
}

/// Removes SCHEDULE-FORCE-SEND parameters (servers must not keep them) and returns the
/// attendees that carried SCHEDULE-FORCE-SEND=REQUEST and whether the organizer asked
/// for a forced REPLY.
pub fn take_force_send(cal: &mut Component) -> (BTreeSet<String>, bool) {
    let mut requests = BTreeSet::new();
    let mut reply = false;
    for c in cal.items_mut() {
        for p in c.props.iter_mut().filter(|p| p.name == "ATTENDEE" || p.name == "ORGANIZER") {
            if let Some(v) = p.param("SCHEDULE-FORCE-SEND").map(str::to_ascii_uppercase) {
                if p.name == "ATTENDEE" && v == "REQUEST" {
                    requests.insert(p.address());
                }
                if p.name == "ORGANIZER" && v == "REPLY" {
                    reply = true;
                }
                p.remove_param("SCHEDULE-FORCE-SEND");
            }
        }
    }
    (requests, reply)
}

/// Sets the ORGANIZER of every component (`sent_by`: someone acting for the organizer).
pub fn set_organizer(cal: &mut Component, email: &str, name: &str, sent_by: Option<&str>) {
    for c in cal.items_mut() {
        let mut p = Prop::new("ORGANIZER", format!("mailto:{email}"));
        if !name.trim().is_empty() {
            p.set_param("CN", name.trim());
        }
        if let Some(s) = sent_by {
            p.set_param("SENT-BY", &format!("mailto:{s}"));
        }
        c.set(p);
    }
}

/// An attendee to invite: an address and an optional display name.
#[derive(Debug, Clone, PartialEq)]
pub struct Invitee {
    pub email: String,
    pub name: Option<String>,
}

/// Parses "Ada Lovelace <ada@example.org>", "mailto:ada@example.org" or a bare address.
pub fn parse_invitee(s: &str) -> Result<Invitee> {
    let s = s.trim();
    let (name, addr) = match (s.rfind('<'), s.ends_with('>')) {
        (Some(i), true) => (Some(s[..i].trim().trim_matches('"').trim().to_string()), &s[i + 1..s.len() - 1]),
        _ => (None, s),
    };
    let email = normalize_address(addr);
    if !valid_email(&email) {
        return Err(Error::bad(format!("not an email address: {s:?}")));
    }
    Ok(Invitee {
        email,
        name: name.filter(|n| !n.is_empty()),
    })
}

/// Makes the attendee list of every component `list`: attendees not in it are removed,
/// existing ones keep their parameters (status included), new ones are added to the master
/// (and to overridden instances that list attendees) as NEEDS-ACTION with RSVP requested.
pub fn set_attendees(cal: &mut Component, list: &[Invitee]) {
    let wanted: BTreeSet<&str> = list.iter().map(|i| i.email.as_str()).collect();
    for c in cal.items_mut() {
        let is_master = c.get("RECURRENCE-ID").is_none();
        let had_any = c.get("ATTENDEE").is_some();
        c.props.retain(|p| p.name != "ATTENDEE" || wanted.contains(p.address().as_str()));
        if !is_master && !had_any {
            continue;
        }
        let present: BTreeSet<String> = c.all("ATTENDEE").map(Prop::address).collect();
        let insert_at = c
            .props
            .iter()
            .rposition(|p| p.name == "ATTENDEE" || p.name == "ORGANIZER")
            .map(|i| i + 1)
            .unwrap_or(c.props.len());
        let mut added = Vec::new();
        for inv in list.iter().filter(|i| !present.contains(&i.email)) {
            let mut p = Prop::new("ATTENDEE", format!("mailto:{}", inv.email));
            if let Some(n) = &inv.name {
                p.set_param("CN", n);
            }
            p.set_param("CUTYPE", "INDIVIDUAL");
            p.set_param("ROLE", "REQ-PARTICIPANT");
            p.set_param("PARTSTAT", "NEEDS-ACTION");
            p.set_param("RSVP", "TRUE");
            added.push(p);
        }
        c.props.splice(insert_at..insert_at, added);
    }
}

/// Removes the ORGANIZER when nobody is invited any more.
pub fn drop_organizer_without_attendees(cal: &mut Component) {
    if cal.items().all(|c| c.get("ATTENDEE").is_none()) {
        for c in cal.items_mut() {
            c.remove("ORGANIZER");
        }
    }
}

pub fn mark_cancelled(cal: &mut Component, seq: i64) {
    for c in cal.items_mut() {
        c.set(Prop::new("STATUS", "CANCELLED"));
    }
    set_sequence(cal, seq);
}

fn utc_stamp() -> String {
    now().format("%Y%m%dT%H%M%SZ").to_string()
}

fn strip_scheduling_params(c: &mut Component) {
    for p in c.props.iter_mut().filter(|p| p.name == "ATTENDEE" || p.name == "ORGANIZER") {
        for k in ["SCHEDULE-AGENT", "SCHEDULE-STATUS", "SCHEDULE-FORCE-SEND"] {
            p.remove_param(k);
        }
    }
}

fn listed(c: &Component, email: &str) -> bool {
    c.all("ATTENDEE").any(|p| p.address() == email)
}

/// The part of the series `email` is invited to (RFC 6638 §3.2.6): the master with the
/// overridden instances that list them, and an EXDATE for each instance they were removed
/// from; or only the instances listing them when they are not invited to the series.
/// Returns None when `email` is not invited at all.
pub fn for_attendee(cal: &Component, email: &str) -> Option<Component> {
    let mut out = Component::new("VCALENDAR");
    out.props = cal.props.iter().filter(|p| p.name != "METHOD").cloned().collect();
    let master_listed = cal.master().is_some_and(|m| m.get("RECURRENCE-ID").is_none() && listed(m, email));
    let mut excluded = Vec::new();
    let mut any = false;
    for c in &cal.subs {
        if !is_item(c) {
            out.subs.push(c.clone());
            continue;
        }
        let is_override = c.get("RECURRENCE-ID").is_some();
        if listed(c, email) && (is_override || master_listed) {
            out.subs.push(c.clone());
            any = true;
        } else if is_override && master_listed {
            excluded.push(c.get("RECURRENCE-ID").cloned().expect("override has RECURRENCE-ID"));
        }
    }
    if !any {
        return None;
    }
    if let Some(m) = out.master_mut().filter(|m| m.get("RECURRENCE-ID").is_none()) {
        for rid in excluded {
            let mut ex = Prop::new("EXDATE", rid.value.clone());
            ex.params = rid.params.into_iter().filter(|(k, _)| k != "RANGE").collect();
            m.props.push(ex);
        }
    }
    Some(out)
}

/// An empty iTIP message with the series' time zones.
fn message_shell(cal: &Component, method: &str) -> Component {
    let mut out = Component::new("VCALENDAR");
    out.props.push(Prop::new("PRODID", "-//Ferrite//Ferrite//EN"));
    out.props.push(Prop::new("VERSION", "2.0"));
    out.props.push(Prop::new("CALSCALE", "GREGORIAN"));
    out.props.push(Prop::new("METHOD", method));
    out.subs.extend(cal.subs.iter().filter(|c| c.name == "VTIMEZONE").cloned());
    out
}

/// METHOD:REQUEST for one attendee's view of the series (see `for_attendee`): organizer
/// alarms and scheduling parameters are removed and DTSTAMP is set to now.
pub fn request(view: &Component) -> String {
    let mut out = message_shell(view, "REQUEST");
    for c in view.items() {
        let mut c = c.clone();
        c.subs.retain(|s| s.name != "VALARM");
        strip_scheduling_params(&mut c);
        c.set(Prop::new("DTSTAMP", utc_stamp()));
        out.subs.push(c);
    }
    out.to_ics()
}

const CANCEL_KEEP: [&str; 13] = [
    "UID",
    "RECURRENCE-ID",
    "SEQUENCE",
    "ORGANIZER",
    "DTSTART",
    "DTEND",
    "DURATION",
    "DUE",
    "SUMMARY",
    "LOCATION",
    "RRULE",
    "RDATE",
    "EXDATE",
];

/// METHOD:CANCEL to `recipients` (RFC 5546 §3.2.5). `whole_event`: the event itself is
/// cancelled (STATUS:CANCELLED); otherwise the recipients were uninvited.
pub fn cancel(view: &Component, recipients: &[String], seq: i64, whole_event: bool) -> String {
    let mut out = message_shell(view, "CANCEL");
    for c in view.items() {
        let mut m = Component::new(&c.name);
        m.props.push(Prop::new("DTSTAMP", utc_stamp()));
        for p in c.props.iter().filter(|p| CANCEL_KEEP.contains(&p.name.as_str())) {
            m.props.push(p.clone());
        }
        for p in c.all("ATTENDEE").filter(|p| recipients.contains(&p.address())) {
            m.props.push(p.clone());
        }
        m.set(Prop::new("SEQUENCE", seq.to_string()));
        if whole_event {
            m.props.push(Prop::new("STATUS", "CANCELLED"));
        }
        strip_scheduling_params(&mut m);
        out.subs.push(m);
    }
    out.to_ics()
}

/// METHOD:REPLY from `attendee` for the components whose status is in `statuses`
/// (RFC 5546 §3.2.3): only the replying ATTENDEE is listed.
pub fn reply(cal: &Component, attendee: &str, statuses: &BTreeMap<String, String>) -> String {
    let mut out = message_shell(cal, "REPLY");
    for c in cal.items() {
        let Some(ps) = statuses.get(&instance_key(c)) else { continue };
        let Some(a) = c.all("ATTENDEE").find(|p| p.address() == attendee) else {
            continue;
        };
        let mut m = Component::new(&c.name);
        m.props.push(Prop::new("DTSTAMP", utc_stamp()));
        for p in c.props.iter().filter(|p| {
            matches!(
                p.name.as_str(),
                "UID" | "RECURRENCE-ID" | "SEQUENCE" | "ORGANIZER" | "DTSTART" | "DTEND" | "DURATION" | "SUMMARY"
            )
        }) {
            m.props.push(p.clone());
        }
        let mut a = a.clone();
        a.set_param("PARTSTAT", ps);
        a.remove_param("RSVP");
        m.props.push(a);
        strip_scheduling_params(&mut m);
        out.subs.push(m);
    }
    out.to_ics()
}

/// The copy delivered into an attendee's calendar: the organizer's data without the
/// organizer's alarms and scheduling parameters, keeping the attendee's own alarms (from
/// `previous`, their current copy) on the matching components.
pub fn attendee_copy(view: &Component, previous: Option<&Component>) -> String {
    let mut out = view.clone();
    out.props.retain(|p| p.name != "METHOD");
    let old_alarms: BTreeMap<String, Vec<Component>> = previous
        .map(|prev| {
            prev.items()
                .map(|c| (instance_key(c), c.subs.iter().filter(|s| s.name == "VALARM").cloned().collect()))
                .collect()
        })
        .unwrap_or_default();
    for c in out.items_mut() {
        c.subs.retain(|s| s.name != "VALARM");
        if let Some(alarms) = old_alarms.get(&instance_key(c)) {
            c.subs.extend(alarms.iter().cloned());
        }
        strip_scheduling_params(c);
    }
    out.to_ics()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERIES: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VTIMEZONE\r\nTZID:Europe/Berlin\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:s1\r\nSEQUENCE:2\r\nDTSTART;TZID=Europe/Berlin:20261105T090000\r\nDTEND;TZID=Europe/Berlin:20261105T100000\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:Standup\r\nORGANIZER;CN=\"Archer, Alice\";SCHEDULE-AGENT=SERVER:mailto:alice@example.org\r\nATTENDEE;CN=Bob;PARTSTAT=ACCEPTED;SCHEDULE-STATUS=1.2:mailto:Bob@Example.org\r\nATTENDEE;PARTSTAT=TENTATIVE;RSVP=TRUE:mailto:carol@example.org\r\nATTENDEE;SCHEDULE-AGENT=CLIENT:mailto:dan@example.org\r\nBEGIN:VALARM\r\nTRIGGER:-PT10M\r\nACTION:DISPLAY\r\nEND:VALARM\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:s1\r\nRECURRENCE-ID;TZID=Europe/Berlin:20261112T090000\r\nDTSTART;TZID=Europe/Berlin:20261112T110000\r\nDTEND;TZID=Europe/Berlin:20261112T120000\r\nSUMMARY:Standup (moved)\r\nORGANIZER:mailto:alice@example.org\r\nATTENDEE:mailto:bob@example.org\r\nATTENDEE:mailto:erin@example.org\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn parses_parties_and_round_trips() {
        let cal = Component::parse(SERIES).unwrap();
        assert_eq!(Component::parse(&cal.to_ics()).unwrap(), cal);
        let org = organizer(&cal).unwrap();
        assert_eq!((org.email.as_str(), org.name.as_deref()), ("alice@example.org", Some("Archer, Alice")));
        assert_eq!(org.mailbox(), "\"Archer, Alice\" <alice@example.org>");
        let att = attendees(&cal);
        let emails: Vec<&str> = att.iter().map(|a| a.email.as_str()).collect();
        assert_eq!(emails, ["bob@example.org", "carol@example.org", "dan@example.org", "erin@example.org"]);
        assert_eq!(att[0].partstat, "ACCEPTED");
        assert!(!att[2].server_scheduled());
        assert_eq!(sequence(&cal), 2);
        assert_eq!(partstat_of(&cal, "carol@example.org").as_deref(), Some("TENTATIVE"));
        let p = Prop::parse("ATTENDEE;CN=\"Doe; Jane: Q\";PARTSTAT=ACCEPTED:mailto:jane@example.org");
        assert_eq!(p.param("CN"), Some("Doe; Jane: Q"));
        assert_eq!(p.address(), "jane@example.org");
        assert_eq!(Prop::parse(&p.to_line()), p);
    }

    #[test]
    fn detects_significant_changes_and_reschedules() {
        let old = Component::parse(SERIES).unwrap();
        let mut new = old.clone();
        new.subs[1].set(Prop::new("DESCRIPTION", "agenda"));
        set_partstat(&mut new, "carol@example.org", "DECLINED");
        assert!(!is_significant(&old, &new), "notes and statuses are not significant");
        let mut moved = old.clone();
        moved.subs[1].set(Prop::parse("DTSTART;TZID=Europe/Berlin:20261105T093000"));
        assert!(is_reschedule(&old, &moved) && is_significant(&old, &moved));
        let mut renamed = old.clone();
        renamed.subs[1].set(Prop::new("LOCATION", "Room 4"));
        assert!(is_significant(&old, &renamed) && !is_reschedule(&old, &renamed));
        let mut fewer = old.clone();
        fewer.subs.pop();
        assert!(is_reschedule(&old, &fewer), "removing an override reschedules that instance");
        reset_partstats(&mut moved, "alice@example.org");
        assert_eq!(partstat_of(&moved, "bob@example.org").as_deref(), Some("NEEDS-ACTION"));
        assert_eq!(
            partstat_of(&moved, "dan@example.org").as_deref(),
            Some("NEEDS-ACTION"),
            "absent PARTSTAT reads as NEEDS-ACTION"
        );
        assert!(
            moved.to_ics().contains("ATTENDEE;SCHEDULE-AGENT=CLIENT:mailto:dan@example.org"),
            "client-scheduled attendees untouched"
        );
    }

    #[test]
    fn filters_instances_per_attendee() {
        let cal = Component::parse(SERIES).unwrap();
        let carol = for_attendee(&cal, "carol@example.org").unwrap();
        assert_eq!(carol.items().count(), 1, "carol is not on the moved instance");
        assert!(carol.to_ics().contains("EXDATE;TZID=Europe/Berlin:20261112T090000"));
        let erin = for_attendee(&cal, "erin@example.org").unwrap();
        assert_eq!(erin.items().count(), 1);
        assert!(erin.items().next().unwrap().get("RECURRENCE-ID").is_some());
        assert_eq!(for_attendee(&cal, "bob@example.org").unwrap().items().count(), 2);
        assert!(for_attendee(&cal, "zoe@example.org").is_none());
        let msg = request(&for_attendee(&cal, "bob@example.org").unwrap());
        assert!(msg.contains("METHOD:REQUEST") && msg.contains("BEGIN:VTIMEZONE") && msg.contains("RRULE:FREQ=WEEKLY"));
        assert!(!msg.contains("VALARM") && !msg.contains("SCHEDULE-"), "{msg}");
    }

    #[test]
    fn builds_cancel_and_reply_messages() {
        let cal = Component::parse(SERIES).unwrap();
        let c = cancel(&for_attendee(&cal, "carol@example.org").unwrap(), &["carol@example.org".into()], 3, false);
        assert!(c.contains("METHOD:CANCEL") && c.contains("SEQUENCE:3") && c.contains("mailto:carol@example.org"));
        assert!(!c.contains("bob@example.org") && !c.contains("STATUS:CANCELLED"));
        let all = cancel(&cal, &["bob@example.org".into()], 3, true);
        assert_eq!(all.matches("STATUS:CANCELLED").count(), 2);
        let mut mine = cal.clone();
        set_partstat(&mut mine, "bob@example.org", "DECLINED");
        let r = reply(&mine, "bob@example.org", &partstats(&mine, "bob@example.org"));
        assert!(r.contains("METHOD:REPLY") && r.contains("PARTSTAT=DECLINED") && r.contains("ORGANIZER"));
        assert!(!r.contains("carol") && !r.contains("SCHEDULE-STATUS"), "{r}");
    }

    #[test]
    fn edits_attendee_lists() {
        let mut cal = Component::parse(SERIES).unwrap();
        let keep = vec![parse_invitee("bob@example.org").unwrap(), parse_invitee("Fay Fox <FAY@example.org>").unwrap()];
        set_attendees(&mut cal, &keep);
        let emails: Vec<String> = attendees(&cal).into_iter().map(|a| a.email).collect();
        assert_eq!(emails, ["bob@example.org", "fay@example.org"]);
        assert_eq!(
            partstat_of(&cal, "bob@example.org").as_deref(),
            Some("ACCEPTED"),
            "kept attendees keep their status"
        );
        let fay = cal.master().unwrap().all("ATTENDEE").map(Prop::to_line).find(|l| l.contains("fay@")).unwrap();
        assert_eq!(
            fay,
            "ATTENDEE;CN=Fay Fox;CUTYPE=INDIVIDUAL;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:fay@example.org"
        );
        assert_eq!(cal.subs[2].all("ATTENDEE").count(), 2, "the override listing attendees gets the new one too");
        set_attendees(&mut cal, &[]);
        drop_organizer_without_attendees(&mut cal);
        assert!(organizer(&cal).is_none());
        assert!(parse_invitee("not an address").is_err() && parse_invitee("a@b").is_err());
        let mut forced = Component::parse(&SERIES.replace("RSVP=TRUE", "SCHEDULE-FORCE-SEND=REQUEST")).unwrap();
        let (req, _) = take_force_send(&mut forced);
        assert!(req.contains("carol@example.org") && !forced.to_ics().contains("FORCE"));
    }

    #[test]
    fn attendee_copies_keep_their_own_alarms() {
        let cal = Component::parse(SERIES).unwrap();
        let mut prev = Component::parse(&attendee_copy(&cal, None)).unwrap();
        assert!(prev.items().all(|c| c.subs.is_empty()), "organizer alarms are not copied");
        let mut alarm = Component::new("VALARM");
        alarm.props.push(Prop::new("TRIGGER", "-PT1H"));
        prev.master_mut().unwrap().subs.push(alarm);
        let next = attendee_copy(&cal, Some(&prev));
        assert!(next.contains("TRIGGER:-PT1H") && !next.contains("TRIGGER:-PT10M"));
    }
}
