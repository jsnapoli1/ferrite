//! Sieve scripts for visual filters and the autoresponder, in the same layout as Nextcloud
//! Mail (its FilterBuilder/FilterParser and OutOfOfficeParser), so scripts written by either
//! program are understood by the other.
//!
//! Generated parts sit between marker lines (`### Nextcloud Mail: Filters ### DON'T EDIT ###`
//! and `### Nextcloud Mail: Vacation Responder ### DON'T EDIT ###`) and carry their
//! definition as JSON on a `# FILTER: ` / `# DATA: ` line. Parsing a script returns those
//! definitions and the rest of the script untouched; building puts the generated sections
//! back around that rest. Everything a person wrote by hand outside the markers survives
//! any number of round trips.

use crate::{Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const FILTER_SEPARATOR: &str = "### Nextcloud Mail: Filters ### DON'T EDIT ###";
const FILTER_DATA: &str = "# FILTER: ";
pub const VACATION_SEPARATOR: &str = "### Nextcloud Mail: Vacation Responder ### DON'T EDIT ###";
const VACATION_DATA: &str = "# DATA: ";
const NL: &str = "\r\n";

/// Escapes a string for a Sieve quoted string (RFC 5228 §2.4.2).
pub fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// `["a", "b"]`
pub fn string_list(values: &[String]) -> String {
    format!("[{}]", values.iter().map(|v| format!("\"{}\"", escape(v))).collect::<Vec<_>>().join(", "))
}

fn yes() -> bool {
    true
}

fn allof() -> String {
    "allof".into()
}

/// One condition: a header compared with values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilterTest {
    /// subject, from or to
    pub field: String,
    /// is, contains or matches (wildcards `*` and `?`)
    pub operator: String,
    pub values: Vec<String>,
}

/// One action. Ferrite's editor offers fileinto, addflag and stop; keep, addsystemflag and
/// redirect (which Nextcloud also writes) are understood so their filters round-trip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilterAction {
    #[serde(rename = "type")]
    pub kind: String,
    /// For fileinto: the folder (its full name, UTF-8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mailbox: Option<String>,
    /// For addflag: an IMAP keyword (a tag such as `$label1`) or a system flag (`\Flagged`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Filter {
    pub name: String,
    #[serde(default = "yes")]
    pub enable: bool,
    /// allof (every condition) or anyof (any condition)
    #[serde(default = "allof")]
    pub operator: String,
    pub tests: Vec<FilterTest>,
    pub actions: Vec<FilterAction>,
    /// Position in the list (filters run in order).
    #[serde(default)]
    pub priority: i64,
}

const SYSTEM_FLAGS: [&str; 6] = ["\\answered", "\\deleted", "\\draft", "\\flagged", "\\recent", "\\seen"];

fn is_system_flag(f: &str) -> bool {
    SYSTEM_FLAGS.contains(&f.to_ascii_lowercase().as_str())
}

/// A tag usable as an IMAP keyword (RFC 3501 atom characters, no backslash).
fn valid_keyword(f: &str) -> bool {
    !f.is_empty() && f.len() <= 200 && f.chars().all(|c| c.is_ascii_graphic() && !"(){%*\"\\]".contains(c))
}

impl Filter {
    /// Checks a filter from the editor or an API call.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(Error::bad("a filter needs a name"));
        }
        if !matches!(self.operator.as_str(), "allof" | "anyof") {
            return Err(Error::bad("operator must be allof or anyof"));
        }
        if self.tests.is_empty() {
            return Err(Error::bad(format!("filter “{}” needs at least one condition", self.name)));
        }
        for t in &self.tests {
            if !matches!(t.field.as_str(), "subject" | "from" | "to") {
                return Err(Error::bad("conditions can test subject, from or to"));
            }
            if !matches!(t.operator.as_str(), "is" | "contains" | "matches") {
                return Err(Error::bad("conditions compare with is, contains or matches"));
            }
            if t.values.is_empty() || t.values.iter().any(|v| v.trim().is_empty()) {
                return Err(Error::bad(format!("a condition of “{}” has no value", self.name)));
            }
        }
        if self.actions.is_empty() {
            return Err(Error::bad(format!("filter “{}” needs at least one action", self.name)));
        }
        for a in &self.actions {
            match a.kind.as_str() {
                "fileinto" if a.mailbox.as_deref().is_some_and(|m| !m.trim().is_empty()) => {}
                "fileinto" => return Err(Error::bad("moving needs a folder")),
                "addflag" | "addsystemflag" => {
                    let f = a.flag.as_deref().unwrap_or("");
                    if !(is_system_flag(f) || valid_keyword(f)) {
                        return Err(Error::bad(format!("{f:?} is not a valid tag")));
                    }
                }
                "redirect" if a.recipient.as_deref().is_some_and(|r| r.contains('@')) => {}
                "keep" | "stop" => {}
                other => return Err(Error::bad(format!("unknown filter action {other:?}"))),
            }
        }
        Ok(())
    }
}

/// A script split into its generated parts and the rest.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed<T> {
    pub data: T,
    /// The script without the generated sections, exactly as written otherwise.
    pub untouched: String,
}

/// Splits `script` at `separator` sections: lines inside them are dropped (and the data
/// lines handed to `data`), the others are kept.
fn split_sections(script: &str, separator: &str, marker: &str, mut data: impl FnMut(&str) -> Result<()>) -> Result<String> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in script.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)) {
        if line.starts_with(separator) {
            inside = !inside;
            continue;
        }
        if !inside {
            out.push(line);
        } else if let Some(json) = line.strip_prefix(marker) {
            data(json)?;
        }
    }
    Ok(out.join(NL))
}

/// The filters defined in a script.
pub fn parse_filters(script: &str) -> Result<Parsed<Vec<Filter>>> {
    let mut filters: Vec<Filter> = Vec::new();
    let untouched = split_sections(script, FILTER_SEPARATOR, FILTER_DATA, |json| {
        let list: Vec<Filter> = serde_json::from_str(json).map_err(|e| Error::bad(format!("the filter data in the Sieve script is invalid: {e}")))?;
        filters.extend(list);
        Ok(())
    })?;
    filters.sort_by_key(|f| f.priority);
    Ok(Parsed { data: filters, untouched })
}

fn test_command(t: &FilterTest) -> String {
    let values = string_list(&t.values);
    match t.field.as_str() {
        "subject" => format!("header :{} \"Subject\" {values}", t.operator),
        "to" => format!("address :{} :all \"To\" {values}", t.operator),
        _ => format!("address :{} :all \"From\" {values}", t.operator),
    }
}

/// Builds the script: the filter `require`s, the untouched rest, then the filters (their
/// JSON and the commands), in the layout Nextcloud Mail writes.
pub fn build_filters(filters: &[Filter], untouched: &str) -> String {
    let mut commands: Vec<String> = Vec::new();
    let mut extensions: Vec<&str> = Vec::new();
    let need = |e: &'static str, extensions: &mut Vec<&str>| {
        if !extensions.contains(&e) {
            extensions.push(e);
        }
    };
    for f in filters.iter().filter(|f| f.enable) {
        commands.push(format!("# {}", f.name.replace(['\r', '\n'], " ")));
        let tests: Vec<String> = f
            .tests
            .iter()
            .filter(|t| matches!(t.field.as_str(), "subject" | "from" | "to"))
            .map(test_command)
            .collect();
        if tests.is_empty() {
            commands.push("# No valid tests found".into());
            continue;
        }
        let mut actions = Vec::new();
        for a in &f.actions {
            match a.kind.as_str() {
                "fileinto" => {
                    need("fileinto", &mut extensions);
                    actions.push(format!("fileinto \"{}\";", escape(a.mailbox.as_deref().unwrap_or(""))));
                }
                "addflag" | "addsystemflag" => {
                    need("imap4flags", &mut extensions);
                    let flag = a.flag.as_deref().unwrap_or("");
                    let flag = if is_system_flag(flag) || valid_keyword(flag) {
                        flag
                    } else {
                        "placeholder_for_invalid_label"
                    };
                    actions.push(format!("addflag \"{}\";", escape(flag)));
                }
                "keep" => actions.push("keep;".into()),
                "stop" => actions.push("stop;".into()),
                "redirect" if a.recipient.as_deref().is_some_and(|r| !r.is_empty()) => {
                    need("copy", &mut extensions);
                    actions.push(format!("redirect :copy \"{}\";", escape(a.recipient.as_deref().unwrap_or(""))));
                }
                _ => {}
            }
        }
        let cond = if tests.len() > 1 {
            format!("{} ({})", f.operator, tests.join(", "))
        } else {
            tests[0].clone()
        };
        let body: Vec<String> = actions.iter().map(|a| format!("\t{a}")).collect();
        commands.push(format!("if {cond} {{{NL}{}{NL}}}", body.join(NL)));
    }
    let mut lines: Vec<String> = Vec::new();
    if !extensions.is_empty() {
        let ext: Vec<String> = extensions.iter().map(|e| e.to_string()).collect();
        lines.push(FILTER_SEPARATOR.into());
        lines.push(format!("require {};", string_list(&ext)));
        lines.push(FILTER_SEPARATOR.into());
    }
    let rest = untouched
        .strip_suffix("\r\n\r\n")
        .map(|r| format!("{r}\r\n"))
        .unwrap_or_else(|| untouched.to_string());
    lines.push(rest);
    if !filters.is_empty() {
        let defs: Vec<Filter> = filters
            .iter()
            .enumerate()
            .map(|(i, f)| Filter {
                priority: if f.priority == 0 { i as i64 + 1 } else { f.priority },
                ..f.clone()
            })
            .collect();
        lines.push(FILTER_SEPARATOR.into());
        lines.push(format!("{FILTER_DATA}{}", serde_json::to_string(&defs).expect("filters serialize")));
        lines.extend(commands);
        lines.push(FILTER_SEPARATOR.into());
    }
    lines.join(NL)
}

// ---- autoresponder ------------------------------------------------------------------------

fn default_version() -> i64 {
    1
}

fn default_days() -> i64 {
    4
}

/// The autoresponder (Sieve `vacation`, RFC 5230) as stored in the script's `# DATA:` line.
/// `version`, `enabled`, `start`, `end`, `subject` and `message` are Nextcloud's fields;
/// `days` and `addresses` are Ferrite's (Nextcloud ignores them and uses 4 days).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Vacation {
    #[serde(default = "default_version")]
    pub version: i64,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<DateTime<Utc>>,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub message: String,
    /// Each sender gets at most one reply per this many days.
    #[serde(default = "default_days")]
    pub days: i64,
    /// Addresses of this mailbox: only mail sent to one of them is answered.
    #[serde(default)]
    pub addresses: Vec<String>,
}

impl Default for Vacation {
    fn default() -> Self {
        Vacation {
            version: 1,
            enabled: false,
            start: None,
            end: None,
            subject: String::new(),
            message: String::new(),
            days: 4,
            addresses: Vec::new(),
        }
    }
}

impl Vacation {
    pub fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.start.is_none() {
            return Err(Error::bad("the autoresponder needs a first day"));
        }
        if let (Some(s), Some(e)) = (self.start, self.end) {
            if e < s {
                return Err(Error::bad("the last day must not be before the first day"));
            }
        }
        if self.message.trim().is_empty() {
            return Err(Error::bad("the autoresponder needs a message"));
        }
        if !(1..=365).contains(&self.days) {
            return Err(Error::bad("reply to each sender at most every 1 to 365 days"));
        }
        if let Some(a) = self.addresses.iter().find(|a| !a.contains('@') || a.contains(['"', '\\', '\r', '\n'])) {
            return Err(Error::bad(format!("{a:?} is not an email address")));
        }
        Ok(())
    }
}

/// The autoresponder defined in a script (None when the script has none).
pub fn parse_vacation(script: &str) -> Result<Parsed<Option<Vacation>>> {
    let mut state = None;
    let untouched = split_sections(script, VACATION_SEPARATOR, VACATION_DATA, |json| {
        state = Some(serde_json::from_str(json).map_err(|e| Error::bad(format!("the autoresponder data in the Sieve script is invalid: {e}")))?);
        Ok(())
    })?;
    Ok(Parsed { data: state, untouched })
}

fn sieve_time(t: &DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Builds the script with the autoresponder around the untouched rest. A disabled
/// autoresponder only keeps its data (so the settings are remembered).
pub fn build_vacation(v: &Vacation, untouched: &str) -> String {
    let mut v = v.clone();
    if !v.enabled {
        v.start = None;
        v.end = None;
        return [
            untouched.to_string(),
            VACATION_SEPARATOR.into(),
            format!("{VACATION_DATA}{}", serde_json::to_string(&v).expect("vacation serializes")),
            VACATION_SEPARATOR.into(),
        ]
        .join(NL);
    }
    // Both sides in UTC: ISO 8601 strings in the same zone compare like times.
    let date = |op: &str, t: &DateTime<Utc>| format!("currentdate :zone \"+0000\" :value \"{op}\" \"iso8601\" \"{}\"", sieve_time(t));
    let start = v.start.unwrap_or_else(crate::util::now);
    let condition = match &v.end {
        Some(end) => format!("allof({}, {})", date("ge", &start), date("le", end)),
        None => date("ge", &start),
    };
    let automatic = "anyof(exists \"List-Id\", exists \"List-Unsubscribe\", address :all :matches [\"From\", \"Sender\"] [\"noreply@*\", \"no-reply@*\"])";
    let mut cmd = vec![
        "vacation".to_string(),
        format!(":days {}", v.days.clamp(1, 365)),
        format!(":subject \"{}\"", escape(&v.subject)),
    ];
    if !v.addresses.is_empty() {
        cmd.push(format!(":addresses {}", string_list(&v.addresses)));
    }
    cmd.push(format!("\"{}\"", escape(&v.message)));
    let placeholder = v.subject.contains("${subject}") || v.message.contains("${subject}");
    let mut out: Vec<String> = vec![
        VACATION_SEPARATOR.into(),
        "require \"date\";".into(),
        "require \"relational\";".into(),
        "require \"vacation\";".into(),
    ];
    if placeholder {
        out.push("require \"variables\";".into());
    }
    out.push(VACATION_SEPARATOR.into());
    out.push(untouched.to_string());
    out.push(VACATION_SEPARATOR.into());
    out.push(format!("{VACATION_DATA}{}", serde_json::to_string(&v).expect("vacation serializes")));
    if placeholder {
        out.extend(
            [
                "set \"subject\" \"\";",
                "if header :matches \"subject\" \"*\" {",
                "\tset \"subject\" \"${1}\";",
                "}",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
    }
    out.push(format!("if {condition} {{"));
    out.push(format!("\tif not {automatic} {{"));
    out.push(format!("\t\t{};", cmd.join(" ")));
    out.push("\t}".into());
    out.push("}".into());
    out.push(VACATION_SEPARATOR.into());
    out.join(NL)
}

/// The script as stored: lines end with CRLF and it ends with exactly one.
pub fn normalize_script(script: &str) -> String {
    let unix = script.replace("\r\n", "\n");
    let trimmed = unix.trim_end_matches('\n');
    if trimmed.is_empty() {
        return String::new();
    }
    format!("{}\r\n", trimmed.replace('\n', "\r\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(name: &str, prio: i64) -> Filter {
        Filter {
            name: name.into(),
            enable: true,
            operator: "anyof".into(),
            tests: vec![
                FilterTest {
                    field: "from".into(),
                    operator: "is".into(),
                    values: vec!["boss@example.org".into()],
                },
                FilterTest {
                    field: "subject".into(),
                    operator: "contains".into(),
                    values: vec!["Urgent \"now\"".into(), "Dringend".into()],
                },
            ],
            actions: vec![
                FilterAction {
                    kind: "fileinto".into(),
                    mailbox: Some("Work/Bosses".into()),
                    flag: None,
                    recipient: None,
                },
                FilterAction {
                    kind: "addflag".into(),
                    mailbox: None,
                    flag: Some("$label1".into()),
                    recipient: None,
                },
                FilterAction {
                    kind: "stop".into(),
                    mailbox: None,
                    flag: None,
                    recipient: None,
                },
            ],
            priority: prio,
        }
    }

    const HANDWRITTEN: &str = "require [\"fileinto\"];\r\n# my own rule\r\nif header :contains \"X-Spam\" \"yes\" {\r\n\tfileinto \"Junk\";\r\n}\r\n";

    #[test]
    fn builds_filters_like_nextcloud() {
        let script = build_filters(&[filter("Boss", 1)], HANDWRITTEN);
        let expected = format!(
            "{FILTER_SEPARATOR}\r\nrequire [\"fileinto\", \"imap4flags\"];\r\n{FILTER_SEPARATOR}\r\n{HANDWRITTEN}\r\n{FILTER_SEPARATOR}\r\n# FILTER: {}\r\n# Boss\r\nif anyof (address :is :all \"From\" [\"boss@example.org\"], header :contains \"Subject\" [\"Urgent \\\"now\\\"\", \"Dringend\"]) {{\r\n\tfileinto \"Work/Bosses\";\r\n\taddflag \"$label1\";\r\n\tstop;\r\n}}\r\n{FILTER_SEPARATOR}",
            serde_json::to_string(&[filter("Boss", 1)]).unwrap()
        );
        assert_eq!(script, expected);
    }

    #[test]
    fn filters_round_trip_and_keep_handwritten_parts() {
        let filters = vec![filter("Boss", 1), filter("Second", 2)];
        let s1 = build_filters(&filters, HANDWRITTEN);
        let p = parse_filters(&s1).unwrap();
        assert_eq!(p.data, filters);
        assert_eq!(normalize_script(&p.untouched), HANDWRITTEN);
        // Rebuilding from the parse is stable.
        let s2 = build_filters(&p.data, &p.untouched);
        assert_eq!(s2, s1);
        // Removing every filter leaves exactly the handwritten script.
        assert_eq!(normalize_script(&build_filters(&[], &p.untouched)), HANDWRITTEN);
        // Disabled filters keep their definition but generate no commands.
        let mut off = filter("Off", 1);
        off.enable = false;
        let s = build_filters(&[off.clone()], "");
        assert!(!s.contains("if ") && !s.contains("require"));
        assert_eq!(parse_filters(&s).unwrap().data, vec![off]);
    }

    #[test]
    fn parses_scripts_written_by_nextcloud() {
        // As Nextcloud Mail writes it (PHP's json_encode escapes slashes; extra fields).
        let nc = "### Nextcloud Mail: Filters ### DON'T EDIT ###\r\nrequire [\"fileinto\"];\r\n### Nextcloud Mail: Filters ### DON'T EDIT ###\r\n\r\n### Nextcloud Mail: Filters ### DON'T EDIT ###\r\n# FILTER: [{\"name\":\"News\",\"enable\":true,\"operator\":\"allof\",\"tests\":[{\"operator\":\"contains\",\"values\":[\"news\"],\"field\":\"from\"}],\"actions\":[{\"type\":\"fileinto\",\"mailbox\":\"INBOX\\/News\"}],\"priority\":10}]\r\n# News\r\nif address :contains :all \"From\" [\"news\"] {\r\n\tfileinto \"INBOX/News\";\r\n}\r\n### Nextcloud Mail: Filters ### DON'T EDIT ###";
        let p = parse_filters(nc).unwrap();
        assert_eq!(p.data.len(), 1);
        assert_eq!(p.data[0].actions[0].mailbox.as_deref(), Some("INBOX/News"));
        assert_eq!(p.data[0].priority, 10);
        assert_eq!(p.untouched, "");
        assert!(parse_filters("### Nextcloud Mail: Filters ### DON'T EDIT ###\n# FILTER: {nope\n").is_err());
    }

    #[test]
    fn validates_filters() {
        assert!(filter("Boss", 1).validate().is_ok());
        let mut f = filter("x", 1);
        f.tests[0].field = "cc".into();
        assert!(f.validate().is_err());
        let mut f = filter("x", 1);
        f.actions[1].flag = Some("bad flag".into());
        assert!(f.validate().is_err());
        let mut f = filter("x", 1);
        f.actions[1].flag = Some("\\Flagged".into());
        assert!(f.validate().is_ok());
        let mut f = filter("", 1);
        assert!(f.validate().is_err());
        f.name = "y".into();
        f.actions.clear();
        assert!(f.validate().is_err());
    }

    #[test]
    fn builds_and_parses_the_autoresponder() {
        let v = Vacation {
            enabled: true,
            start: Some("2026-10-24T00:00:00Z".parse().unwrap()),
            end: Some("2026-11-02T22:59:59Z".parse().unwrap()),
            subject: "Re: ${subject}".into(),
            message: "I am away until November.\nUrgent: call \"Bob\".".into(),
            days: 7,
            addresses: vec!["alice@example.test".into()],
            ..Vacation::default()
        };
        let script = build_vacation(&v, HANDWRITTEN);
        assert!(script.starts_with(&format!("{VACATION_SEPARATOR}\r\nrequire \"date\";\r\nrequire \"relational\";\r\nrequire \"vacation\";\r\nrequire \"variables\";\r\n{VACATION_SEPARATOR}\r\n")));
        assert!(script.contains("if allof(currentdate :zone \"+0000\" :value \"ge\" \"iso8601\" \"2026-10-24T00:00:00Z\", currentdate :zone \"+0000\" :value \"le\" \"iso8601\" \"2026-11-02T22:59:59Z\") {"));
        assert!(
            script.contains(
                "vacation :days 7 :subject \"Re: ${subject}\" :addresses [\"alice@example.test\"] \"I am away until November.\nUrgent: call \\\"Bob\\\".\";"
            ),
            "{script}"
        );
        assert!(script.contains("\tset \"subject\" \"${1}\";"));
        let p = parse_vacation(&script).unwrap();
        assert_eq!(p.data.as_ref(), Some(&v));
        assert_eq!(normalize_script(&p.untouched), HANDWRITTEN);
        assert_eq!(build_vacation(&v, &p.untouched), script);
        // Disabled: only the data line, no dates.
        let off = build_vacation(&Vacation { enabled: false, ..v.clone() }, &p.untouched);
        assert!(!off.contains("vacation :days") && !off.contains("require \"vacation\""));
        let back = parse_vacation(&off).unwrap().data.unwrap();
        assert!(!back.enabled && back.start.is_none() && back.message == v.message);
        // Nextcloud's own data line (no days/addresses) parses with the defaults.
        let nc = parse_vacation("### Nextcloud Mail: Vacation Responder ### DON'T EDIT ###\r\n# DATA: {\"version\":1,\"enabled\":true,\"start\":\"2026-10-24T00:00:00+00:00\",\"subject\":\"Away\",\"message\":\"Back soon\"}\r\n### Nextcloud Mail: Vacation Responder ### DON'T EDIT ###").unwrap();
        let nc = nc.data.unwrap();
        assert!(nc.enabled && nc.days == 4 && nc.end.is_none() && nc.start.is_some());
    }

    #[test]
    fn filters_and_autoresponder_coexist() {
        let filters = vec![filter("Boss", 1)];
        let v = Vacation {
            enabled: true,
            start: Some("2026-10-24T00:00:00Z".parse().unwrap()),
            subject: "Away".into(),
            message: "Back soon".into(),
            ..Vacation::default()
        };
        let with_filters = build_filters(&filters, HANDWRITTEN);
        let both = build_vacation(&v, &parse_vacation(&with_filters).unwrap().untouched);
        // Updating the filters keeps the autoresponder, and the reverse.
        assert_eq!(parse_vacation(&both).unwrap().data.unwrap(), v);
        assert_eq!(parse_filters(&both).unwrap().data, filters);
        let refiltered = build_filters(&[], &parse_filters(&both).unwrap().untouched);
        assert_eq!(parse_vacation(&refiltered).unwrap().data.unwrap(), v);
        assert!(parse_filters(&refiltered).unwrap().data.is_empty());
        // Every require stays ahead of the first command.
        let first_cmd = both.find("if ").unwrap();
        assert!(both.rfind("require").unwrap() < first_cmd, "{both}");
    }

    #[test]
    fn validates_the_autoresponder() {
        let mut v = Vacation {
            enabled: true,
            start: Some("2026-10-24T00:00:00Z".parse().unwrap()),
            message: "x".into(),
            ..Vacation::default()
        };
        assert!(v.validate().is_ok());
        v.end = Some("2026-10-01T00:00:00Z".parse().unwrap());
        assert!(v.validate().is_err());
        v.end = None;
        v.days = 0;
        assert!(v.validate().is_err());
        v.days = 3;
        v.start = None;
        assert!(v.validate().is_err());
        assert_eq!(normalize_script("a\nb\n\n\n"), "a\r\nb\r\n");
        assert_eq!(normalize_script("\r\n"), "");
    }
}
