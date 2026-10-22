//! CalDAV (RFC 4791) and CardDAV (RFC 6352) at Nextcloud's URLs, so phones and desktop apps
//! (DAVx⁵, Apple Calendar/Contacts, Thunderbird, GNOME, Evolution) sync calendars, tasks and
//! contacts. Discovery: `/.well-known/caldav` and `/.well-known/carddav` redirect to
//! `/remote.php/dav/`, whose principal points at the calendar and address book homes.
//!
//! Supported: PROPFIND (Depth 0/1), PROPPATCH (name/color), REPORT calendar-query (with
//! time-range and component filters), calendar-multiget, addressbook-query,
//! addressbook-multiget, sync-collection (full sync; stale tokens get `valid-sync-token`),
//! MKCALENDAR, extended MKCOL for address books, GET/PUT/DELETE with ETags.
//!
//! Scheduling (RFC 6638, `calendar-auto-schedule`): writes go through
//! `ferrite_core::scheduling`, which sends invitations, updates, cancellations and replies;
//! the principal has a scheduling inbox (always empty: invitations land in calendars) and
//! an outbox that answers free/busy requests. Calendars shared with the caller appear in
//! their calendar home as `<uri>_shared_by_<owner>` with the privileges of the share.

use crate::AppState;
use crate::auth::{Auth, authenticate};
use crate::error::{ApiError, public_message, status_of};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, NaiveDateTime, Utc};
use ferrite_core::calendar_sharing::{CalAccess, CalendarRef};
use ferrite_core::groupware::{Collection, DavObject, KIND_ADDRESSBOOK, KIND_CALENDAR};
use ferrite_core::itip::{self, Component, Prop};
use ferrite_core::{Error, Result};
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};

const SEG: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'/');
const BASE: &str = "/remote.php/dav";
const NS: &str = "xmlns:d=\"DAV:\" xmlns:cal=\"urn:ietf:params:xml:ns:caldav\" xmlns:card=\"urn:ietf:params:xml:ns:carddav\" xmlns:cs=\"http://calendarserver.org/ns/\" xmlns:x1=\"http://apple.com/ns/ical/\"";
const MAX_BODY: usize = 2 * 1024 * 1024;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn enc(s: &str) -> String {
    utf8_percent_encode(s, SEG).to_string()
}

/// What the request body asks for, read namespace-agnostically.
#[derive(Debug, Default)]
struct Body {
    root: String,
    hrefs: Vec<String>,
    time_start: Option<DateTime<Utc>>,
    time_end: Option<DateTime<Utc>>,
    comp_filters: Vec<String>,
    /// `<comp name>` entries of a supported-calendar-component-set (MKCALENDAR).
    comps: Vec<String>,
    sync_token: Option<String>,
    displayname: Option<String>,
    color: Option<String>,
    is_addressbook: bool,
}

fn parse_time(v: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(v.trim_end_matches('Z'), "%Y%m%dT%H%M%S")
        .ok()
        .map(|t| t.and_utc())
}

fn parse_body(xml: &str) -> Body {
    use quick_xml::events::Event;
    let mut b = Body::default();
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut stack: Vec<String> = Vec::new();
    // Text of the innermost open element; quick-xml splits text around entity references.
    let mut text = String::new();
    loop {
        match reader.read_event() {
            Ok(ev @ (Event::Start(_) | Event::Empty(_))) => {
                let (e, is_start) = match &ev {
                    Event::Start(e) => (e, true),
                    Event::Empty(e) => (e, false),
                    _ => unreachable!("matched above"),
                };
                let name = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                if b.root.is_empty() {
                    b.root = name.clone();
                }
                let attr = |key: &str| {
                    e.attributes().flatten().find(|a| a.key.local_name().as_ref() == key.as_bytes()).map(|a| {
                        let raw = String::from_utf8_lossy(&a.value).to_string();
                        quick_xml::escape::unescape(&raw).map(|c| c.into_owned()).unwrap_or(raw)
                    })
                };
                match name.as_str() {
                    "time-range" => {
                        b.time_start = attr("start").and_then(|v| parse_time(&v));
                        b.time_end = attr("end").and_then(|v| parse_time(&v));
                    }
                    "comp-filter" => {
                        if let Some(n) = attr("name") {
                            b.comp_filters.push(n.to_ascii_uppercase());
                        }
                    }
                    "comp" => {
                        if let Some(n) = attr("name") {
                            b.comps.push(n.to_ascii_uppercase());
                        }
                    }
                    "addressbook" if stack.iter().any(|s| s == "resourcetype") => b.is_addressbook = true,
                    _ => {}
                }
                // Empty elements (`<a/>`) get no End event, so only Start elements nest.
                if is_start {
                    stack.push(name);
                    text.clear();
                }
            }
            Ok(Event::Text(t)) => text.push_str(&t.decode().map(|c| c.into_owned()).unwrap_or_default()),
            Ok(Event::CData(t)) => text.push_str(&String::from_utf8_lossy(&t)),
            Ok(Event::GeneralRef(r)) => {
                let resolved = match r.resolve_char_ref() {
                    Ok(Some(c)) => Some(c.to_string()),
                    _ => match &*String::from_utf8_lossy(&r) {
                        "amp" => Some("&".into()),
                        "lt" => Some("<".into()),
                        "gt" => Some(">".into()),
                        "quot" => Some("\"".into()),
                        "apos" => Some("'".into()),
                        _ => None,
                    },
                };
                text.push_str(&resolved.unwrap_or_default());
            }
            Ok(Event::End(_)) => {
                let value = text.trim().to_string();
                text.clear();
                if !value.is_empty() {
                    match stack.last().map(String::as_str) {
                        Some("href") => b.hrefs.push(value),
                        Some("sync-token") => b.sync_token = Some(value),
                        Some("displayname") => b.displayname = Some(value),
                        Some("calendar-color") => b.color = Some(value.chars().take(7).collect()),
                        _ => {}
                    }
                }
                stack.pop();
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    b
}

enum Target {
    Root,
    Principal,
    Home(&'static str),
    /// The scheduling inbox and outbox (RFC 6638 §2.1, §2.2).
    Inbox,
    Outbox,
    Collection(&'static str, String),
    Object(&'static str, String, String),
}

/// Maps a path under /remote.php/dav (or /dav) to a target, checking it is the caller's.
fn target(path: &str, username: &str) -> Option<Target> {
    let rest = path.strip_prefix(BASE).or_else(|| path.strip_prefix("/dav"))?;
    let parts: Vec<String> = rest
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned())
        .collect();
    let user_ok = |u: &str| u.eq_ignore_ascii_case(username);
    match parts.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => Some(Target::Root),
        ["principals", "users", u] if user_ok(u) => Some(Target::Principal),
        ["calendars", u] if user_ok(u) => Some(Target::Home(KIND_CALENDAR)),
        ["calendars", u, "inbox"] if user_ok(u) => Some(Target::Inbox),
        ["calendars", u, "outbox"] if user_ok(u) => Some(Target::Outbox),
        ["calendars", u, c] if user_ok(u) => Some(Target::Collection(KIND_CALENDAR, c.to_string())),
        ["calendars", u, c, o] if user_ok(u) => Some(Target::Object(KIND_CALENDAR, c.to_string(), o.to_string())),
        ["addressbooks", "users", u] if user_ok(u) => Some(Target::Home(KIND_ADDRESSBOOK)),
        ["addressbooks", "users", u, c] if user_ok(u) => Some(Target::Collection(KIND_ADDRESSBOOK, c.to_string())),
        ["addressbooks", "users", u, c, o] if user_ok(u) => Some(Target::Object(KIND_ADDRESSBOOK, c.to_string(), o.to_string())),
        _ => None,
    }
}

fn home_href(kind: &str, user: &str) -> String {
    if kind == KIND_CALENDAR {
        format!("{BASE}/calendars/{}/", enc(user))
    } else {
        format!("{BASE}/addressbooks/users/{}/", enc(user))
    }
}

fn col_href(c: &Collection, user: &str) -> String {
    format!("{}{}/", home_href(&c.kind, user), enc(&c.uri))
}

fn obj_href(c: &Collection, user: &str, o: &DavObject) -> String {
    format!("{}{}", col_href(c, user), enc(&o.uri))
}

fn multistatus(body: String) -> Response {
    let xml = format!("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<d:multistatus {NS}>{body}</d:multistatus>");
    let mut r = (StatusCode::MULTI_STATUS, xml).into_response();
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml; charset=utf-8"));
    r
}

fn propstat(href: &str, props: &str) -> String {
    format!(
        "<d:response><d:href>{}</d:href><d:propstat><d:prop>{props}</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
        esc(href)
    )
}

/// The current-user-privilege-set for a collection the caller reaches with `access`.
fn privileges(access: CalAccess) -> String {
    let list: &[&str] = match access {
        CalAccess::Owner => &[
            "<d:all/>",
            "<d:read/>",
            "<d:write/>",
            "<d:write-properties/>",
            "<d:write-content/>",
            "<d:bind/>",
            "<d:unbind/>",
            "<cal:read-free-busy/>",
        ],
        // Sharees can write events, and set their own name and colour for the calendar.
        CalAccess::ReadWrite => &[
            "<d:read/>",
            "<d:write/>",
            "<d:write-properties/>",
            "<d:write-content/>",
            "<d:bind/>",
            "<d:unbind/>",
            "<cal:read-free-busy/>",
        ],
        CalAccess::Read => &["<d:read/>", "<cal:read-free-busy/>"],
    };
    let inner: String = list.iter().map(|p| format!("<d:privilege>{p}</d:privilege>")).collect();
    format!("<d:current-user-privilege-set>{inner}</d:current-user-privilege-set>")
}

fn principal_props(auth: &Auth) -> String {
    let u = &auth.user.username;
    let email = auth
        .user
        .email
        .as_deref()
        .map(|e| format!("<d:href>mailto:{}</d:href>", esc(e)))
        .unwrap_or_default();
    format!(
        "<d:current-user-principal><d:href>{BASE}/principals/users/{0}/</d:href></d:current-user-principal>\
         <d:principal-URL><d:href>{BASE}/principals/users/{0}/</d:href></d:principal-URL>\
         <cal:calendar-home-set><d:href>{1}</d:href></cal:calendar-home-set>\
         <card:addressbook-home-set><d:href>{2}</d:href></card:addressbook-home-set>\
         <cal:calendar-user-address-set>{email}<d:href>{BASE}/principals/users/{0}/</d:href></cal:calendar-user-address-set>\
         <cal:schedule-inbox-URL><d:href>{1}inbox/</d:href></cal:schedule-inbox-URL>\
         <cal:schedule-outbox-URL><d:href>{1}outbox/</d:href></cal:schedule-outbox-URL>\
         <cal:calendar-user-type>INDIVIDUAL</cal:calendar-user-type>\
         <d:displayname>{3}</d:displayname>",
        enc(u),
        home_href(KIND_CALENDAR, u),
        home_href(KIND_ADDRESSBOOK, u),
        esc(&auth.user.display_name),
    )
}

/// Properties of a collection as the caller sees it (`owner`: the owner's username).
fn collection_props(c: &Collection, owner: &str, access: CalAccess) -> String {
    let owner = format!("<d:owner><d:href>{BASE}/principals/users/{}/</d:href></d:owner>", enc(owner));
    let privs = privileges(access);
    if c.kind == KIND_CALENDAR {
        let comps: String = c.components.split(',').map(|x| format!("<cal:comp name=\"{}\"/>", esc(x))).collect();
        format!(
            "<d:resourcetype><d:collection/><cal:calendar/></d:resourcetype><d:displayname>{}</d:displayname>\
             <x1:calendar-color>{}</x1:calendar-color><cs:getctag>{}</cs:getctag><d:sync-token>{}</d:sync-token>\
             <cal:supported-calendar-component-set>{comps}</cal:supported-calendar-component-set>{privs}{owner}",
            esc(&c.name),
            esc(&c.color),
            esc(&c.ctag),
            sync_token(c)
        )
    } else {
        format!(
            "<d:resourcetype><d:collection/><card:addressbook/></d:resourcetype><d:displayname>{}</d:displayname>\
             <cs:getctag>{}</cs:getctag><d:sync-token>{}</d:sync-token>\
             <card:supported-address-data><card:address-data-type content-type=\"text/vcard\" version=\"3.0\"/></card:supported-address-data>{privs}{owner}",
            esc(&c.name),
            esc(&c.ctag),
            sync_token(c)
        )
    }
}

fn mailbox_props(inbox: bool, default_calendar: Option<&str>) -> String {
    if inbox {
        let default = default_calendar
            .map(|h| {
                format!(
                    "<cal:schedule-default-calendar-URL><d:href>{}</d:href></cal:schedule-default-calendar-URL>",
                    esc(h)
                )
            })
            .unwrap_or_default();
        format!(
            "<d:resourcetype><d:collection/><cal:schedule-inbox/></d:resourcetype><d:displayname>Inbox</d:displayname>\
             <cs:getctag>inbox</cs:getctag><d:sync-token>http://ferrite/sync/inbox</d:sync-token>{default}\
             <d:current-user-privilege-set><d:privilege><d:read/></d:privilege><d:privilege><cal:schedule-deliver/></d:privilege></d:current-user-privilege-set>"
        )
    } else {
        "<d:resourcetype><d:collection/><cal:schedule-outbox/></d:resourcetype><d:displayname>Outbox</d:displayname>\
         <d:current-user-privilege-set><d:privilege><d:read/></d:privilege><d:privilege><cal:schedule-send/></d:privilege></d:current-user-privilege-set>"
            .to_string()
    }
}

fn sync_token(c: &Collection) -> String {
    format!("http://ferrite/sync/{}", c.ctag)
}

fn content_type(o: &DavObject) -> String {
    if o.component == "VCARD" {
        "text/vcard; charset=utf-8".into()
    } else {
        format!("text/calendar; charset=utf-8; component={}", o.component.to_lowercase())
    }
}

fn object_props(o: &DavObject, with_data: bool) -> String {
    let data = if !with_data {
        String::new()
    } else if o.component == "VCARD" {
        format!("<card:address-data>{}</card:address-data>", esc(&o.data))
    } else {
        format!("<cal:calendar-data>{}</cal:calendar-data>", esc(&o.data))
    };
    format!(
        "<d:getetag>&quot;{}&quot;</d:getetag><d:getcontenttype>{}</d:getcontenttype><d:resourcetype/><d:getcontentlength>{}</d:getcontentlength>{data}",
        esc(&o.etag),
        content_type(o),
        o.data.len()
    )
}

fn error_response(e: &Error) -> Response {
    (status_of(e), public_message(e)).into_response()
}

fn unauthorized() -> Response {
    let mut r = (StatusCode::UNAUTHORIZED, "authentication required").into_response();
    r.headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"Ferrite\", charset=\"UTF-8\""));
    r
}

pub async fn well_known() -> Response {
    let mut r = StatusCode::MOVED_PERMANENTLY.into_response();
    r.headers_mut().insert(header::LOCATION, HeaderValue::from_static("/remote.php/dav/"));
    r
}

fn options() -> Response {
    let mut r = StatusCode::OK.into_response();
    let h = r.headers_mut();
    // calendar-auto-schedule: the server sends invitations and replies (RFC 6638), so clients
    // must not send their own (see ferrite_core::scheduling).
    h.insert(
        "DAV",
        HeaderValue::from_static("1, 2, 3, access-control, calendar-access, calendar-auto-schedule, addressbook, extended-mkcol"),
    );
    h.insert(
        header::ALLOW,
        HeaderValue::from_static("OPTIONS, GET, HEAD, POST, PUT, DELETE, PROPFIND, PROPPATCH, REPORT, MKCALENDAR, MKCOL"),
    );
    r
}

pub async fn handle(State(s): State<AppState>, req: Request) -> Response {
    if req.method() == Method::OPTIONS {
        return options();
    }
    let auth = match authenticate(&s, req.headers(), req.method(), true).await {
        Ok(a) if a.token.allows("groupware") => a,
        Ok(_) => return (StatusCode::FORBIDDEN, "token lacks the groupware scope").into_response(),
        Err(ApiError(Error::Unavailable(m))) => return (StatusCode::TOO_MANY_REQUESTS, m).into_response(),
        Err(_) => return unauthorized(),
    };
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let headers = req.headers().clone();
    let Some(t) = target(&path, &auth.user.username) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let body = match axum::body::to_bytes(req.into_body(), MAX_BODY).await {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response(),
    };
    let result = match method.as_str() {
        "PROPFIND" => propfind(&s, &auth, t, &headers).await,
        "PROPPATCH" => proppatch(&s, &auth, t, &path, &body).await,
        "REPORT" => report(&s, &auth, t, &body).await,
        "GET" | "HEAD" => get(&s, &auth, t).await,
        "PUT" => put(&s, &auth, t, &headers, &body).await,
        "DELETE" => delete(&s, &auth, t, &headers).await,
        "MKCALENDAR" | "MKCOL" => mkcol(&s, &auth, t, &body, method == "MKCALENDAR").await,
        "POST" => match t {
            Target::Outbox => free_busy(&s, &auth, &body).await,
            _ => Ok(StatusCode::METHOD_NOT_ALLOWED.into_response()),
        },
        _ => Ok(StatusCode::METHOD_NOT_ALLOWED.into_response()),
    };
    result.unwrap_or_else(|e| error_response(&e))
}

/// A collection the caller can reach: an address book of theirs, or a calendar of theirs or
/// shared with them (under the uri they see it by).
async fn collection(s: &AppState, auth: &Auth, kind: &str, uri: &str) -> Result<CalendarRef> {
    if kind == KIND_CALENDAR {
        s.core.calendar_ref(&auth.user.id, uri).await
    } else {
        Ok(CalendarRef::owned(s.core.dav_collection(&auth.user.id, kind, uri).await?))
    }
}

fn owner_name<'a>(cal: &'a CalendarRef, auth: &'a Auth) -> &'a str {
    cal.shared_by.as_ref().map(|o| o.username.as_str()).unwrap_or(&auth.user.username)
}

async fn default_calendar_href(s: &AppState, auth: &Auth) -> Result<Option<String>> {
    Ok(s.core
        .dav_collections(&auth.user.id, KIND_CALENDAR)
        .await?
        .iter()
        .find(|c| c.components.split(',').any(|x| x == "VEVENT"))
        .map(|c| col_href(c, &auth.user.username)))
}

async fn propfind(s: &AppState, auth: &Auth, t: Target, headers: &HeaderMap) -> Result<Response> {
    let depth1 = headers.get("depth").and_then(|v| v.to_str().ok()).unwrap_or("0") != "0";
    let user = auth.user.username.clone();
    let core = &s.core;
    let mut out = String::new();
    match t {
        Target::Root => {
            out.push_str(&propstat(
                &format!("{BASE}/"),
                &format!("<d:resourcetype><d:collection/></d:resourcetype>{}", principal_props(auth)),
            ));
        }
        Target::Principal => {
            out.push_str(&propstat(
                &format!("{BASE}/principals/users/{}/", enc(&user)),
                &format!("<d:resourcetype><d:principal/></d:resourcetype>{}", principal_props(auth)),
            ));
        }
        Target::Home(kind) => {
            out.push_str(&propstat(
                &home_href(kind, &user),
                &format!(
                    "<d:resourcetype><d:collection/></d:resourcetype>{}{}",
                    principal_props(auth),
                    privileges(CalAccess::Owner)
                ),
            ));
            if depth1 {
                if kind == KIND_CALENDAR {
                    for cal in core.user_calendars(&auth.user.id).await? {
                        let props = collection_props(&cal.collection, owner_name(&cal, auth), cal.access);
                        out.push_str(&propstat(&col_href(&cal.collection, &user), &props));
                    }
                    let default = default_calendar_href(s, auth).await?;
                    let home = home_href(KIND_CALENDAR, &user);
                    out.push_str(&propstat(&format!("{home}inbox/"), &mailbox_props(true, default.as_deref())));
                    out.push_str(&propstat(&format!("{home}outbox/"), &mailbox_props(false, None)));
                } else {
                    for c in core.dav_collections(&auth.user.id, kind).await? {
                        out.push_str(&propstat(&col_href(&c, &user), &collection_props(&c, &user, CalAccess::Owner)));
                    }
                }
            }
        }
        Target::Inbox | Target::Outbox => {
            let inbox = matches!(t, Target::Inbox);
            let default = default_calendar_href(s, auth).await?;
            let href = format!("{}{}/", home_href(KIND_CALENDAR, &user), if inbox { "inbox" } else { "outbox" });
            out.push_str(&propstat(&href, &mailbox_props(inbox, default.as_deref())));
        }
        Target::Collection(kind, uri) => {
            let cal = collection(s, auth, kind, &uri).await?;
            let c = &cal.collection;
            out.push_str(&propstat(&col_href(c, &user), &collection_props(c, owner_name(&cal, auth), cal.access)));
            if depth1 {
                for o in core.dav_objects(c).await? {
                    out.push_str(&propstat(&obj_href(c, &user, &o), &object_props(&o, false)));
                }
            }
        }
        Target::Object(kind, col, uri) => {
            let cal = collection(s, auth, kind, &col).await?;
            let o = core.dav_object(&cal.collection, &uri).await?;
            out.push_str(&propstat(&obj_href(&cal.collection, &user, &o), &object_props(&o, false)));
        }
    }
    Ok(multistatus(out))
}

async fn proppatch(s: &AppState, auth: &Auth, t: Target, path: &str, body: &str) -> Result<Response> {
    let b = parse_body(body);
    if let Target::Collection(kind, uri) = t {
        let cal = collection(s, auth, kind, &uri).await?;
        if cal.is_owner() {
            s.core
                .update_dav_collection(&cal.collection, b.displayname.as_deref(), b.color.as_deref())
                .await?;
        } else {
            // A sharee renames or recolours the calendar for themselves only.
            s.core
                .set_shared_calendar_prefs(&auth.user.id, &cal, b.displayname.as_deref(), b.color.as_deref())
                .await?;
        }
    }
    Ok(multistatus(format!(
        "<d:response><d:href>{}</d:href><d:propstat><d:prop><d:displayname/><x1:calendar-color/></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
        esc(path)
    )))
}

async fn report(s: &AppState, auth: &Auth, t: Target, body: &str) -> Result<Response> {
    let b = parse_body(body);
    let user = auth.user.username.clone();
    let (kind, uri) = match t {
        Target::Collection(k, u) => (k, u),
        // The inbox stays empty: invitations are delivered straight into calendars.
        Target::Inbox => {
            let token = if b.root == "sync-collection" {
                "<d:sync-token>http://ferrite/sync/inbox</d:sync-token>"
            } else {
                ""
            };
            return Ok(multistatus(token.to_string()));
        }
        _ => return Err(Error::bad("REPORT is supported on calendars and address books")),
    };
    let cal = collection(s, auth, kind, &uri).await?;
    let c = &cal.collection;
    let mut out = String::new();
    match b.root.as_str() {
        "calendar-multiget" | "addressbook-multiget" => {
            for href in &b.hrefs {
                let name = href.trim_end_matches('/').rsplit('/').next().unwrap_or("");
                let name = percent_decode_str(name).decode_utf8_lossy();
                match s.core.dav_object(c, &name).await {
                    Ok(o) => out.push_str(&propstat(&obj_href(c, &user, &o), &object_props(&o, true))),
                    Err(_) => out.push_str(&format!(
                        "<d:response><d:href>{}</d:href><d:status>HTTP/1.1 404 Not Found</d:status></d:response>",
                        esc(href)
                    )),
                }
            }
        }
        "calendar-query" | "addressbook-query" => {
            let wanted: Vec<&String> = b.comp_filters.iter().filter(|c| c.as_str() != "VCALENDAR").collect();
            for o in s.core.dav_objects_in_range(c, b.time_start, b.time_end).await? {
                if !wanted.is_empty() && !wanted.iter().any(|w| **w == o.component) {
                    continue;
                }
                out.push_str(&propstat(&obj_href(c, &user, &o), &object_props(&o, true)));
            }
        }
        "sync-collection" => {
            let current = sync_token(c);
            match b.sync_token.as_deref() {
                None | Some("") => {
                    for o in s.core.dav_objects(c).await? {
                        out.push_str(&propstat(&obj_href(c, &user, &o), &object_props(&o, false)));
                    }
                }
                Some(tok) if tok == current => {}
                Some(_) => {
                    // We keep no change log: ask the client to resynchronize from scratch.
                    let xml = format!("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<d:error {NS}><d:valid-sync-token/></d:error>");
                    let mut r = (StatusCode::FORBIDDEN, xml).into_response();
                    r.headers_mut()
                        .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml; charset=utf-8"));
                    return Ok(r);
                }
            }
            out.push_str(&format!("<d:sync-token>{}</d:sync-token>", esc(&current)));
        }
        other => return Err(Error::bad(format!("unsupported REPORT {other}"))),
    }
    Ok(multistatus(out))
}

async fn get(s: &AppState, auth: &Auth, t: Target) -> Result<Response> {
    let Target::Object(kind, col, uri) = t else {
        return Ok((StatusCode::OK, "Ferrite CalDAV/CardDAV").into_response());
    };
    let cal = collection(s, auth, kind, &col).await?;
    let o = s.core.dav_object(&cal.collection, &uri).await?;
    let mut r = (StatusCode::OK, o.data.clone()).into_response();
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&content_type(&o)).unwrap_or_else(|_| HeaderValue::from_static("text/plain")),
    );
    h.insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{}\"", o.etag)).unwrap_or_else(|_| HeaderValue::from_static("\"\"")),
    );
    Ok(r)
}

/// A 403 with the DAV:need-privileges precondition (RFC 3744 §7.1.1).
fn need_privilege(href: &str, privilege: &str) -> Response {
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<d:error {NS}><d:need-privileges><d:resource><d:href>{}</d:href><d:privilege><d:{privilege}/></d:privilege></d:resource></d:need-privileges></d:error>",
        esc(href)
    );
    let mut r = (StatusCode::FORBIDDEN, xml).into_response();
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml; charset=utf-8"));
    r
}

async fn put(s: &AppState, auth: &Auth, t: Target, headers: &HeaderMap, body: &str) -> Result<Response> {
    let Target::Object(kind, col, uri) = t else {
        return Err(Error::bad("PUT targets an object inside a calendar or address book"));
    };
    let cal = collection(s, auth, kind, &col).await?;
    if !cal.writable {
        return Ok(need_privilege(&col_href(&cal.collection, &auth.user.username), "bind"));
    }
    let if_match = headers.get(header::IF_MATCH).and_then(|v| v.to_str().ok());
    let if_none = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some("*");
    let (o, created, rewritten) = if kind == KIND_CALENDAR {
        let saved = s.core.save_calendar_object(&auth.user, &cal, &uri, body, if_match, if_none).await?;
        (saved.object, saved.created, saved.rewritten)
    } else {
        let (o, created) = s.core.put_dav_object(&cal.collection, &uri, body, if_match, if_none).await?;
        (o, created, false)
    };
    let mut r = if created { StatusCode::CREATED } else { StatusCode::NO_CONTENT }.into_response();
    // When scheduling changed the stored data (status parameters, sequence), the client must
    // fetch it again, so no ETag is returned (RFC 4791 §5.3.4).
    if !rewritten {
        r.headers_mut().insert(
            header::ETAG,
            HeaderValue::from_str(&format!("\"{}\"", o.etag)).unwrap_or_else(|_| HeaderValue::from_static("\"\"")),
        );
    }
    Ok(r)
}

async fn delete(s: &AppState, auth: &Auth, t: Target, headers: &HeaderMap) -> Result<Response> {
    match t {
        Target::Object(kind, col, uri) => {
            let cal = collection(s, auth, kind, &col).await?;
            if !cal.writable {
                return Ok(need_privilege(&col_href(&cal.collection, &auth.user.username), "unbind"));
            }
            let if_match = headers.get(header::IF_MATCH).and_then(|v| v.to_str().ok());
            if kind == KIND_CALENDAR {
                // Schedule-Reply: F asks the server not to decline on the attendee's behalf.
                let reply = headers.get("schedule-reply").and_then(|v| v.to_str().ok()).map(str::trim) != Some("F");
                s.core.delete_calendar_object(&auth.user, &cal, &uri, if_match, reply).await?;
            } else {
                s.core.delete_dav_object(&cal.collection, &uri, if_match).await?;
            }
        }
        Target::Collection(kind, uri) => {
            let cal = collection(s, auth, kind, &uri).await?;
            if cal.is_owner() {
                s.core.delete_dav_collection(&cal.collection).await?;
            } else {
                // Deleting a shared calendar removes it from the sharee's list only.
                s.core.leave_shared_calendar(&auth.user.id, &cal).await?;
            }
        }
        _ => return Ok(StatusCode::FORBIDDEN.into_response()),
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn mkcol(s: &AppState, auth: &Auth, t: Target, body: &str, is_calendar: bool) -> Result<Response> {
    let Target::Collection(kind, uri) = t else {
        return Ok(StatusCode::FORBIDDEN.into_response());
    };
    let b = parse_body(body);
    // Plain MKCOL in the address book home (or with an addressbook resourcetype) makes a book.
    let kind = if is_calendar {
        KIND_CALENDAR
    } else if kind == KIND_ADDRESSBOOK || b.is_addressbook {
        KIND_ADDRESSBOOK
    } else {
        kind
    };
    if s.core.dav_collection(&auth.user.id, kind, &uri).await.is_ok() {
        return Ok(StatusCode::METHOD_NOT_ALLOWED.into_response());
    }
    let comps = if kind == KIND_CALENDAR {
        let comps: Vec<&str> = b
            .comps
            .iter()
            .map(String::as_str)
            .filter(|c| matches!(*c, "VEVENT" | "VTODO" | "VJOURNAL"))
            .collect();
        if comps.is_empty() { "VEVENT,VTODO".to_string() } else { comps.join(",") }
    } else {
        "VCARD".to_string()
    };
    s.core
        .create_dav_collection(
            &auth.user.id,
            kind,
            &uri,
            b.displayname.as_deref().unwrap_or(&uri),
            b.color.as_deref(),
            Some(&comps),
        )
        .await?;
    Ok(StatusCode::CREATED.into_response())
}

/// Free/busy lookup through the scheduling outbox (RFC 6638 §5): a VFREEBUSY request from
/// the caller, answered for each attendee who has a Ferrite account.
async fn free_busy(s: &AppState, auth: &Auth, body: &str) -> Result<Response> {
    let cal = Component::parse(body)?;
    if !cal.value("METHOD").is_some_and(|m| m.trim().eq_ignore_ascii_case("REQUEST")) {
        return Err(Error::bad("POST a METHOD:REQUEST VFREEBUSY to the outbox"));
    }
    let fb = cal
        .subs
        .iter()
        .find(|c| c.name == "VFREEBUSY")
        .ok_or_else(|| Error::bad("only free/busy requests are accepted"))?;
    let my_email = auth.user.email.as_deref().map(itip::normalize_address);
    let organizer = fb.get("ORGANIZER").map(|p| p.address());
    if organizer.is_none() || organizer != my_email {
        return Err(Error::Forbidden);
    }
    let time = |name: &str| {
        fb.get(name).and_then(|p| {
            let params: Vec<(String, String)> = p.params.iter().map(|(k, _)| (k.clone(), p.param(k).unwrap_or("").to_string())).collect();
            ferrite_core::recur::parse_time(&p.value, &params).map(|t| t.utc())
        })
    };
    let (Some(from), Some(to)) = (time("DTSTART"), time("DTEND")) else {
        return Err(Error::bad("the request needs DTSTART and DTEND"));
    };
    if to <= from || to - from > chrono::Duration::days(366) {
        return Err(Error::bad("ask for at most a year of free/busy time"));
    }
    let stamp = |t: DateTime<Utc>| t.format("%Y%m%dT%H%M%SZ").to_string();
    let mut out = String::new();
    for a in fb.all("ATTENDEE") {
        let email = a.address();
        let recipient = format!("<cal:recipient><d:href>mailto:{}</d:href></cal:recipient>", esc(&email));
        let Some(user) = s.core.user_by_email(&email).await? else {
            out.push_str(&format!(
                "<cal:response>{recipient}<cal:request-status>3.7;Invalid calendar user</cal:request-status></cal:response>"
            ));
            continue;
        };
        let busy = s.core.busy_periods(&user, from, to).await?;
        let mut reply = Component::new("VCALENDAR");
        reply.props.push(Prop::new("VERSION", "2.0"));
        reply.props.push(Prop::new("PRODID", "-//Ferrite//Ferrite//EN"));
        reply.props.push(Prop::new("METHOD", "REPLY"));
        let mut v = Component::new("VFREEBUSY");
        if let Some(uid) = fb.get("UID") {
            v.props.push(uid.clone());
        }
        v.props.push(Prop::new("DTSTAMP", stamp(Utc::now())));
        v.props.push(Prop::new("DTSTART", stamp(from)));
        v.props.push(Prop::new("DTEND", stamp(to)));
        if let Some(o) = fb.get("ORGANIZER") {
            v.props.push(o.clone());
        }
        v.props.push(a.clone());
        if !busy.is_empty() {
            let mut p = Prop::new(
                "FREEBUSY",
                busy.iter().map(|(a, b)| format!("{}/{}", stamp(*a), stamp(*b))).collect::<Vec<_>>().join(","),
            );
            p.set_param("FBTYPE", "BUSY");
            v.props.push(p);
        }
        reply.subs.push(v);
        out.push_str(&format!(
            "<cal:response>{recipient}<cal:request-status>2.0;Success</cal:request-status><cal:calendar-data>{}</cal:calendar-data></cal:response>",
            esc(&reply.to_ics())
        ));
    }
    let xml = format!("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<cal:schedule-response {NS}>{out}</cal:schedule-response>");
    let mut r = (StatusCode::OK, xml).into_response();
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml; charset=utf-8"));
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_report_bodies() {
        let q = r#"<?xml version="1.0"?><C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
            <D:prop><D:getetag/><C:calendar-data/></D:prop>
            <C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT">
            <C:time-range start="20261101T000000Z" end="20261201T000000Z"/></C:comp-filter></C:comp-filter></C:filter></C:calendar-query>"#;
        let b = parse_body(q);
        assert_eq!(b.root, "calendar-query");
        assert_eq!(b.comp_filters, vec!["VCALENDAR", "VEVENT"]);
        assert_eq!(b.time_start.unwrap().to_rfc3339(), "2026-11-01T00:00:00+00:00");
        let m = r#"<c:calendar-multiget xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/></d:prop>
            <d:href>/remote.php/dav/calendars/a/personal/x%20y.ics</d:href><d:href>/remote.php/dav/calendars/a/personal/z.ics</d:href></c:calendar-multiget>"#;
        assert_eq!(parse_body(m).hrefs.len(), 2);
        let s = r#"<d:sync-collection xmlns:d="DAV:"><d:sync-token>http://ferrite/sync/abc</d:sync-token><d:sync-level>1</d:sync-level></d:sync-collection>"#;
        assert_eq!(parse_body(s).sync_token.as_deref(), Some("http://ferrite/sync/abc"));
        let mk = r#"<d:mkcol xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav"><d:set><d:prop><d:resourcetype><d:collection/><card:addressbook/></d:resourcetype><d:displayname>Work &amp; friends</d:displayname></d:prop></d:set></d:mkcol>"#;
        let b = parse_body(mk);
        assert!(b.is_addressbook);
        let mkcal = r#"<C:mkcalendar xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:set><D:prop><D:displayname>Tasks</D:displayname>
            <C:supported-calendar-component-set><C:comp name="VTODO"/></C:supported-calendar-component-set></D:prop></D:set></C:mkcalendar>"#;
        assert_eq!(parse_body(mkcal).comps, vec!["VTODO"]);
        assert_eq!(b.displayname.as_deref(), Some("Work & friends"));
    }

    #[test]
    fn targets() {
        assert!(matches!(
            target("/remote.php/dav/calendars/Alice/personal/x.ics", "alice"),
            Some(Target::Object(KIND_CALENDAR, _, _))
        ));
        assert!(target("/remote.php/dav/calendars/bob/personal/", "alice").is_none());
        assert!(matches!(
            target("/remote.php/dav/addressbooks/users/alice/", "alice"),
            Some(Target::Home(KIND_ADDRESSBOOK))
        ));
        assert!(matches!(target("/remote.php/dav/", "alice"), Some(Target::Root)));
    }
}
