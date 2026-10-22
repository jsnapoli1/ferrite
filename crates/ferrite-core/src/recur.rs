//! iCalendar time zones and recurrence (RFC 5545 §3.3.5, §3.3.10, §3.8.5) for the calendar
//! API: `TZID` times are converted with the IANA database, and recurring events are expanded
//! into their occurrences within a time range, honouring EXDATE, RDATE and overridden
//! instances (RECURRENCE-ID). Expansion runs in the event's own time zone, so a weekly 09:00
//! meeting stays at 09:00 local time across daylight-saving changes.
//!
//! Supported RRULE parts: FREQ (DAILY, WEEKLY, MONTHLY, YEARLY), INTERVAL, COUNT, UNTIL, BYDAY
//! (with ordinals for MONTHLY/YEARLY-with-BYMONTH), BYMONTHDAY, BYMONTH, WKST. Rules using
//! other parts are reported as unsupported and callers fall back to the first occurrence.

use crate::groupware::{split_prop, unescape, unfold};
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc, Weekday};
use std::collections::{BTreeMap, BTreeSet};

/// The time zone of an iCalendar DATE-TIME.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Zone {
    Utc,
    /// No zone given ("floating" time); treated as UTC.
    Floating,
    Tz(chrono_tz::Tz),
}

impl Zone {
    /// The zone named by a `TZID` parameter. Unknown names (e.g. Windows zone names without
    /// a matching IANA zone) fall back to floating time.
    pub fn from_tzid(tzid: &str) -> Zone {
        // Some clients prefix IANA names with a vendor path, e.g. "/mozilla.org/.../Europe/Berlin".
        let mut name = tzid.trim().trim_matches('"');
        if let Ok(tz) = name.parse::<chrono_tz::Tz>() {
            return Zone::Tz(tz);
        }
        while let Some((_, rest)) = name.split_once('/') {
            name = rest;
            if let Ok(tz) = name.parse::<chrono_tz::Tz>() {
                return Zone::Tz(tz);
            }
        }
        Zone::Floating
    }

    pub fn name(&self) -> Option<&'static str> {
        match self {
            Zone::Tz(tz) => Some(tz.name()),
            _ => None,
        }
    }

    /// Local wall time in this zone → UTC. A time skipped by a DST change moves forward an
    /// hour; an ambiguous one takes the earlier instant (RFC 5545 §3.3.5).
    pub fn to_utc(&self, local: NaiveDateTime) -> DateTime<Utc> {
        match self {
            Zone::Utc | Zone::Floating => local.and_utc(),
            Zone::Tz(tz) => match tz.from_local_datetime(&local).earliest() {
                Some(t) => t.with_timezone(&Utc),
                None => tz
                    .from_local_datetime(&(local + Duration::hours(1)))
                    .earliest()
                    .map(|t| t.with_timezone(&Utc))
                    .unwrap_or_else(|| local.and_utc()),
            },
        }
    }

    /// UTC → local wall time in this zone.
    pub fn to_local(&self, t: DateTime<Utc>) -> NaiveDateTime {
        match self {
            Zone::Utc | Zone::Floating => t.naive_utc(),
            Zone::Tz(tz) => t.with_timezone(tz).naive_local(),
        }
    }
}

/// A parsed DATE or DATE-TIME value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IcalTime {
    pub local: NaiveDateTime,
    pub date_only: bool,
    pub zone: Zone,
}

impl IcalTime {
    pub fn utc(&self) -> DateTime<Utc> {
        if self.date_only { self.local.and_utc() } else { self.zone.to_utc(self.local) }
    }
}

/// Parses a property value with its parameters (`VALUE=DATE`, `TZID=...`).
pub fn parse_time(value: &str, params: &[(String, String)]) -> Option<IcalTime> {
    let v = value.trim();
    let date_param = params.iter().any(|(k, v)| k == "VALUE" && v.eq_ignore_ascii_case("DATE"));
    if v.len() == 8 || date_param {
        let d = NaiveDate::parse_from_str(&v[..8.min(v.len())], "%Y%m%d").ok()?;
        return Some(IcalTime {
            local: d.and_time(NaiveTime::MIN),
            date_only: true,
            zone: Zone::Floating,
        });
    }
    let (body, zone) = match v.strip_suffix('Z') {
        Some(b) => (b, Zone::Utc),
        None => match params.iter().find(|(k, _)| k == "TZID") {
            Some((_, tzid)) => (v, Zone::from_tzid(tzid)),
            None => (v, Zone::Floating),
        },
    };
    let local = NaiveDateTime::parse_from_str(body, "%Y%m%dT%H%M%S").ok()?;
    Some(IcalTime { local, date_only: false, zone })
}

/// One occurrence of a (possibly recurring) event.
#[derive(Debug, Clone, PartialEq)]
pub struct Occurrence {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// The occurrence's original start (its RECURRENCE-ID).
    pub recurrence_id: DateTime<Utc>,
    /// Set when this instance was overridden with its own properties.
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
}

/// A content line: (name, parameters, value).
type Prop = (String, Vec<(String, String)>, String);
type Props = Vec<Prop>;

/// The top-level VEVENTs of an object, each as its property list (VALARMs etc. excluded).
fn vevents(data: &str) -> Vec<Props> {
    let mut out = Vec::new();
    let mut current: Option<Props> = None;
    let mut nested = 0usize;
    for line in unfold(data) {
        let (name, params, value) = split_prop(&line);
        let comp = value.trim().to_ascii_uppercase();
        match (name.as_str(), current.is_some()) {
            ("BEGIN", false) if comp == "VEVENT" => current = Some(Vec::new()),
            ("BEGIN", true) => nested += 1,
            ("END", true) if nested > 0 => nested -= 1,
            ("END", true) if comp == "VEVENT" => out.extend(current.take()),
            (_, true) if nested == 0 => current.as_mut().expect("in VEVENT").push((name, params, value)),
            _ => {}
        }
    }
    out
}

fn prop<'a>(props: &'a Props, name: &str) -> Option<&'a Prop> {
    props.iter().find(|(n, _, _)| n == name)
}

/// Every time listed in EXDATE or RDATE lines (comma-separated values allowed).
fn time_list(props: &Props, name: &str) -> Vec<IcalTime> {
    props
        .iter()
        .filter(|(n, _, _)| n == name)
        .flat_map(|(_, params, value)| value.split(',').filter_map(|v| parse_time(v, params)).collect::<Vec<_>>())
        .collect()
}

#[derive(Debug, Default)]
struct Rule {
    freq: String,
    interval: i64,
    count: Option<usize>,
    until: Option<IcalTime>,
    by_day: Vec<(Option<i32>, Weekday)>,
    by_month_day: Vec<i32>,
    by_month: Vec<u32>,
    week_start: Option<Weekday>,
}

fn weekday(code: &str) -> Option<Weekday> {
    Some(match code {
        "MO" => Weekday::Mon,
        "TU" => Weekday::Tue,
        "WE" => Weekday::Wed,
        "TH" => Weekday::Thu,
        "FR" => Weekday::Fri,
        "SA" => Weekday::Sat,
        "SU" => Weekday::Sun,
        _ => return None,
    })
}

/// Parses an RRULE value; None when it uses parts this module does not implement.
fn parse_rule(value: &str) -> Option<Rule> {
    let mut r = Rule {
        interval: 1,
        ..Default::default()
    };
    for part in value.trim().split(';').filter(|p| !p.is_empty()) {
        let (k, v) = part.split_once('=')?;
        let v = v.trim();
        match k.trim().to_ascii_uppercase().as_str() {
            "FREQ" => r.freq = v.to_ascii_uppercase(),
            "INTERVAL" => r.interval = v.parse().ok().filter(|i: &i64| *i >= 1)?,
            "COUNT" => r.count = Some(v.parse().ok()?),
            "UNTIL" => r.until = Some(parse_time(v, &[])?),
            "WKST" => r.week_start = Some(weekday(&v.to_ascii_uppercase())?),
            "BYDAY" => {
                for d in v.split(',') {
                    let d = d.trim().to_ascii_uppercase();
                    let (num, code) = d.split_at(d.len().checked_sub(2)?);
                    let n = if num.is_empty() {
                        None
                    } else {
                        Some(num.parse::<i32>().ok().filter(|n| *n != 0)?)
                    };
                    r.by_day.push((n, weekday(code)?));
                }
            }
            "BYMONTHDAY" => {
                for d in v.split(',') {
                    r.by_month_day.push(d.trim().parse().ok().filter(|d: &i32| *d != 0 && d.abs() <= 31)?);
                }
            }
            "BYMONTH" => {
                for m in v.split(',') {
                    r.by_month.push(m.trim().parse().ok().filter(|m: &u32| (1..=12).contains(m))?);
                }
            }
            _ => return None,
        }
    }
    let ordinals = r.by_day.iter().any(|(n, _)| n.is_some());
    match r.freq.as_str() {
        "DAILY" | "WEEKLY" if ordinals => None,
        "YEARLY" if ordinals && r.by_month.is_empty() => None,
        "DAILY" | "WEEKLY" | "MONTHLY" | "YEARLY" => Some(r),
        _ => None,
    }
}

fn days_in_month(y: i32, m: u32) -> u32 {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    NaiveDate::from_ymd_opt(ny, nm, 1).and_then(|d| d.pred_opt()).map(|d| d.day()).unwrap_or(28)
}

/// The days of month `y-m` selected by BYMONTHDAY/BYDAY, or `default_day` when neither is set.
fn month_days(y: i32, m: u32, r: &Rule, default_day: u32) -> Vec<NaiveDate> {
    let len = days_in_month(y, m);
    let date = |d: u32| NaiveDate::from_ymd_opt(y, m, d);
    let mut days: BTreeSet<NaiveDate> = BTreeSet::new();
    if !r.by_month_day.is_empty() {
        for &d in &r.by_month_day {
            let day = if d > 0 { d } else { len as i32 + d + 1 };
            if day >= 1 && day as u32 <= len {
                days.extend(date(day as u32));
            }
        }
    }
    if !r.by_day.is_empty() {
        let mut from_by_day = BTreeSet::new();
        for &(n, wd) in &r.by_day {
            let all: Vec<NaiveDate> = (1..=len).filter_map(date).filter(|d| d.weekday() == wd).collect();
            match n {
                None => from_by_day.extend(all),
                Some(n) if n > 0 => from_by_day.extend(all.get(n as usize - 1).copied()),
                Some(n) => from_by_day.extend(all.len().checked_sub(n.unsigned_abs() as usize).and_then(|i| all.get(i)).copied()),
            }
        }
        // Both given: a day must satisfy both (RFC 5545 BYxxx parts limit each other).
        days = if r.by_month_day.is_empty() {
            from_by_day
        } else {
            days.intersection(&from_by_day).copied().collect()
        };
    }
    if r.by_month_day.is_empty() && r.by_day.is_empty() {
        days.extend(date(default_day));
    }
    days.into_iter().collect()
}

/// Start times (local wall time) of the rule in chronological order, from `start`, until a
/// local time after `stop` or the rule's own end. Bounded by `max` instances.
fn rule_starts(r: &Rule, start: NaiveDateTime, zone: Zone, stop: NaiveDateTime, max: usize) -> Vec<NaiveDateTime> {
    let time = start.time();
    let d0 = start.date();
    let mut out = vec![start];
    let past_until = |t: NaiveDateTime| match &r.until {
        Some(u) if u.date_only => t.date() > u.local.date(),
        Some(u) => zone.to_utc(t) > u.utc(),
        None => false,
    };
    let done = |out: &Vec<NaiveDateTime>| r.count.is_some_and(|c| out.len() >= c) || out.len() >= max;
    let month_ok = |d: NaiveDate| r.by_month.is_empty() || r.by_month.contains(&d.month());
    // Periods (days, weeks, months or years) to walk; generous, since filters may skip many.
    for k in 1..=100_000i64 {
        if done(&out) {
            break;
        }
        let step = k * r.interval;
        let candidates: Vec<NaiveDate> = match r.freq.as_str() {
            "DAILY" => {
                let d = d0 + Duration::days(step);
                let wd_ok = r.by_day.is_empty() || r.by_day.iter().any(|(_, w)| *w == d.weekday());
                let md_ok = r.by_month_day.is_empty() || month_days(d.year(), d.month(), r, d.day()).contains(&d);
                if month_ok(d) && wd_ok && md_ok { vec![d] } else { vec![] }
            }
            "WEEKLY" => {
                let wkst = r.week_start.unwrap_or(Weekday::Mon);
                let back = (7 + d0.weekday().num_days_from_monday() as i64 - wkst.num_days_from_monday() as i64) % 7;
                // k counts periods from the one after the first; include the first week too.
                let week = d0 - Duration::days(back) + Duration::weeks(step - r.interval);
                let days: Vec<Weekday> = if r.by_day.is_empty() {
                    vec![d0.weekday()]
                } else {
                    r.by_day.iter().map(|(_, w)| *w).collect()
                };
                let mut v: Vec<NaiveDate> = (0..7)
                    .map(|i| week + Duration::days(i))
                    .filter(|d| days.contains(&d.weekday()) && month_ok(*d))
                    .collect();
                v.sort();
                v
            }
            "MONTHLY" => {
                // As with WEEKLY, period 0 is the first month itself.
                let months = d0.month0() as i64 + step - r.interval;
                let (y, m) = (d0.year() + (months.div_euclid(12)) as i32, (months.rem_euclid(12)) as u32 + 1);
                if month_ok(NaiveDate::from_ymd_opt(y, m, 1).expect("valid month")) {
                    month_days(y, m, r, d0.day())
                } else {
                    vec![]
                }
            }
            "YEARLY" => {
                let y = d0.year() + (step - r.interval) as i32;
                let months: Vec<u32> = if r.by_month.is_empty() { vec![d0.month()] } else { r.by_month.clone() };
                months.into_iter().flat_map(|m| month_days(y, m, r, d0.day())).collect()
            }
            _ => return out,
        };
        let mut beyond = false;
        for d in candidates {
            let t = d.and_time(time);
            if t <= start {
                continue;
            }
            if past_until(t) || t > stop {
                beyond = true;
                break;
            }
            out.push(t);
            if done(&out) {
                break;
            }
        }
        if beyond {
            break;
        }
    }
    out
}

/// Expands the recurring event in `data` into the occurrences overlapping `[from, to)`, at
/// most `max` of them. Returns None when the object has no recurrence this module can expand
/// (callers then show the event's first occurrence as stored).
pub fn expand(data: &str, from: DateTime<Utc>, to: DateTime<Utc>, max: usize) -> Option<Vec<Occurrence>> {
    let events = vevents(data);
    let master = events.iter().find(|p| prop(p, "RECURRENCE-ID").is_none())?;
    let (_, sp, sv) = prop(master, "DTSTART")?;
    let start = parse_time(sv, sp)?;
    let rrule = prop(master, "RRULE");
    let rdates = time_list(master, "RDATE");
    if rrule.is_none() && rdates.is_empty() {
        return None;
    }
    let length = match prop(master, "DTEND").and_then(|(_, p, v)| parse_time(v, p)) {
        Some(end) => end.utc() - start.utc(),
        None => match prop(master, "DURATION").and_then(|(_, _, v)| crate::groupware::parse_duration(v.trim())) {
            Some(secs) => Duration::seconds(secs),
            None if start.date_only => Duration::days(1),
            None => Duration::zero(),
        },
    };
    let zone = start.zone;
    // Local stop: a little past `to` so DST shifts cannot drop the last occurrence.
    let stop = zone.to_local(to) + Duration::days(2);
    let mut starts: BTreeSet<DateTime<Utc>> = BTreeSet::new();
    match rrule {
        Some((_, _, rule)) => {
            let rule = parse_rule(rule)?;
            let generated = rule_starts(&rule, start.local, zone, stop, 100_000);
            for t in generated {
                starts.insert(if start.date_only { t.and_utc() } else { zone.to_utc(t) });
            }
        }
        None => {
            starts.insert(start.utc());
        }
    }
    starts.extend(rdates.iter().map(IcalTime::utc));
    for ex in time_list(master, "EXDATE") {
        starts.remove(&ex.utc());
    }
    // Overridden instances, keyed by the start they replace.
    let mut overrides: BTreeMap<DateTime<Utc>, &Props> = BTreeMap::new();
    for e in events.iter().filter(|p| prop(p, "RECURRENCE-ID").is_some()) {
        if let Some(rid) = prop(e, "RECURRENCE-ID").and_then(|(_, p, v)| parse_time(v, p)) {
            overrides.insert(rid.utc(), e);
        }
    }
    let text = |p: &Props, name: &str| prop(p, name).map(|(_, _, v)| unescape(v));
    let mut out = Vec::new();
    for rid in starts {
        let occ = match overrides.get(&rid) {
            Some(o) => {
                let s = prop(o, "DTSTART").and_then(|(_, p, v)| parse_time(v, p)).map(|t| t.utc()).unwrap_or(rid);
                let e = prop(o, "DTEND").and_then(|(_, p, v)| parse_time(v, p)).map(|t| t.utc()).unwrap_or(s + length);
                if prop(o, "STATUS").is_some_and(|(_, _, v)| v.trim().eq_ignore_ascii_case("CANCELLED")) {
                    continue;
                }
                Occurrence {
                    start: s,
                    end: e,
                    recurrence_id: rid,
                    summary: text(o, "SUMMARY"),
                    description: text(o, "DESCRIPTION"),
                    location: text(o, "LOCATION"),
                }
            }
            None => Occurrence {
                start: rid,
                end: rid + length,
                recurrence_id: rid,
                summary: None,
                description: None,
                location: None,
            },
        };
        // Zero-length events still occupy their start instant.
        if occ.start < to && (occ.end > from || (occ.end == occ.start && occ.start >= from)) {
            out.push(occ);
            if out.len() >= max {
                break;
            }
        }
    }
    out.sort_by_key(|o| o.start);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cal(body: &str) -> String {
        format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{body}END:VCALENDAR\r\n")
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn starts(data: &str, from: &str, to: &str) -> Vec<String> {
        expand(data, at(from), at(to), 1000)
            .expect("expandable")
            .into_iter()
            .map(|o| o.start.format("%Y-%m-%dT%H:%MZ").to_string())
            .collect()
    }

    #[test]
    fn tzid_times_convert_with_dst() {
        let p = |v: &str| parse_time(v, &[("TZID".into(), "Europe/Berlin".into())]).unwrap().utc();
        assert_eq!(p("20260115T090000"), at("2026-01-15T08:00:00Z")); // CET, UTC+1
        assert_eq!(p("20260715T090000"), at("2026-07-15T07:00:00Z")); // CEST, UTC+2
        assert_eq!(Zone::from_tzid("/mozilla.org/20050126_1/Europe/Berlin"), Zone::Tz(chrono_tz::Europe::Berlin));
        assert_eq!(Zone::from_tzid("W. Europe Standard Time"), Zone::Floating);
        assert_eq!(parse_time("20260115T090000Z", &[]).unwrap().utc(), at("2026-01-15T09:00:00Z"));
    }

    #[test]
    fn weekly_keeps_local_time_across_dst_with_exdate_and_override() {
        let data = cal(concat!(
            "BEGIN:VEVENT\r\nUID:w\r\nDTSTART;TZID=Europe/Berlin:20261019T090000\r\nDTEND;TZID=Europe/Berlin:20261019T100000\r\n",
            "RRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=4\r\nEXDATE;TZID=Europe/Berlin:20261102T090000\r\nSUMMARY:Standup\r\nEND:VEVENT\r\n",
            "BEGIN:VEVENT\r\nUID:w\r\nRECURRENCE-ID;TZID=Europe/Berlin:20261026T090000\r\nDTSTART;TZID=Europe/Berlin:20261026T110000\r\n",
            "DTEND;TZID=Europe/Berlin:20261026T120000\r\nSUMMARY:Standup (moved)\r\nEND:VEVENT\r\n",
        ));
        // DST ends 2026-10-25 in Berlin: 09:00 is 07:00Z before and 08:00Z after.
        assert_eq!(
            starts(&data, "2026-10-01T00:00:00Z", "2026-12-01T00:00:00Z"),
            ["2026-10-19T07:00Z", "2026-10-26T10:00Z", "2026-11-09T08:00Z"]
        );
        let occ = expand(&data, at("2026-10-26T00:00:00Z"), at("2026-10-27T00:00:00Z"), 10).unwrap();
        assert_eq!(occ[0].summary.as_deref(), Some("Standup (moved)"));
        assert_eq!(occ[0].recurrence_id, at("2026-10-26T08:00:00Z"));
    }

    #[test]
    fn monthly_yearly_daily_rules() {
        let monthly = cal("BEGIN:VEVENT\r\nUID:m\r\nDTSTART:20260130T120000Z\r\nRRULE:FREQ=MONTHLY;BYDAY=-1FR;UNTIL=20260601T000000Z\r\nEND:VEVENT\r\n");
        assert_eq!(
            starts(&monthly, "2026-01-01T00:00:00Z", "2027-01-01T00:00:00Z"),
            [
                "2026-01-30T12:00Z",
                "2026-02-27T12:00Z",
                "2026-03-27T12:00Z",
                "2026-04-24T12:00Z",
                "2026-05-29T12:00Z"
            ]
        );
        // The 31st only exists in some months.
        let day31 = cal("BEGIN:VEVENT\r\nUID:d\r\nDTSTART:20260131T080000Z\r\nRRULE:FREQ=MONTHLY;COUNT=3\r\nEND:VEVENT\r\n");
        assert_eq!(
            starts(&day31, "2026-01-01T00:00:00Z", "2027-01-01T00:00:00Z"),
            ["2026-01-31T08:00Z", "2026-03-31T08:00Z", "2026-05-31T08:00Z"]
        );
        let birthday = cal("BEGIN:VEVENT\r\nUID:b\r\nDTSTART;VALUE=DATE:20000229\r\nRRULE:FREQ=YEARLY\r\nEND:VEVENT\r\n");
        assert_eq!(starts(&birthday, "2026-01-01T00:00:00Z", "2029-01-01T00:00:00Z"), ["2028-02-29T00:00Z"]);
        let weekdays = cal("BEGIN:VEVENT\r\nUID:x\r\nDTSTART:20261022T090000Z\r\nRRULE:FREQ=DAILY;BYDAY=MO,TU,WE,TH,FR\r\nEND:VEVENT\r\n");
        assert_eq!(
            starts(&weekdays, "2026-10-23T00:00:00Z", "2026-10-28T00:00:00Z"),
            ["2026-10-23T09:00Z", "2026-10-26T09:00Z", "2026-10-27T09:00Z"]
        );
        let biweekly = cal("BEGIN:VEVENT\r\nUID:y\r\nDTSTART:20261021T090000Z\r\nRRULE:FREQ=WEEKLY;INTERVAL=2;BYDAY=WE,FR\r\nEND:VEVENT\r\n");
        assert_eq!(
            starts(&biweekly, "2026-10-20T00:00:00Z", "2026-11-10T00:00:00Z"),
            ["2026-10-21T09:00Z", "2026-10-23T09:00Z", "2026-11-04T09:00Z", "2026-11-06T09:00Z"]
        );
    }

    #[test]
    fn unsupported_rules_and_plain_events_are_not_expanded() {
        let hourly = cal("BEGIN:VEVENT\r\nUID:h\r\nDTSTART:20261022T090000Z\r\nRRULE:FREQ=HOURLY\r\nEND:VEVENT\r\n");
        assert!(expand(&hourly, at("2026-10-01T00:00:00Z"), at("2026-11-01T00:00:00Z"), 10).is_none());
        let setpos = cal("BEGIN:VEVENT\r\nUID:s\r\nDTSTART:20261022T090000Z\r\nRRULE:FREQ=MONTHLY;BYDAY=MO,TU;BYSETPOS=-1\r\nEND:VEVENT\r\n");
        assert!(expand(&setpos, at("2026-10-01T00:00:00Z"), at("2026-11-01T00:00:00Z"), 10).is_none());
        let plain = cal("BEGIN:VEVENT\r\nUID:p\r\nDTSTART:20261022T090000Z\r\nEND:VEVENT\r\n");
        assert!(expand(&plain, at("2026-10-01T00:00:00Z"), at("2026-11-01T00:00:00Z"), 10).is_none());
    }
}
