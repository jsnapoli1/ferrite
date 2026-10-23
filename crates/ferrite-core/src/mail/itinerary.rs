//! Travel and booking confirmations: schema.org reservations embedded in mail HTML, as JSON-LD
//! (`<script type="application/ld+json">`) or microdata (`itemscope`/`itemtype`/`itemprop`),
//! the markup airlines, rail and bus operators, hotels, ticket shops, car rentals and
//! restaurants send. Each reservation becomes a card with what, when and where, which the
//! reader can add to a calendar.
//!
//! Supported types: FlightReservation, LodgingReservation, TrainReservation, BusReservation,
//! EventReservation, RentalCarReservation and FoodEstablishmentReservation.

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, Utc};
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::{BufferQueue, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts};
use serde::Serialize;
use serde_json::{Map, Value};
use std::cell::RefCell;

const TYPES: [&str; 7] = [
    "FlightReservation",
    "LodgingReservation",
    "TrainReservation",
    "BusReservation",
    "EventReservation",
    "RentalCarReservation",
    "FoodEstablishmentReservation",
];
/// Reservations taken from one message at most.
const MAX: usize = 20;

/// A point in time as the markup gives it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct When {
    /// As written (ISO 8601).
    pub value: String,
    /// The instant, when the value has a zone offset.
    pub utc: Option<DateTime<Utc>>,
    /// A date without time.
    pub date_only: bool,
}

impl When {
    fn parse(s: &str) -> Option<When> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        if let Ok(t) = DateTime::<FixedOffset>::parse_from_rfc3339(s) {
            return Some(When {
                value: s.into(),
                utc: Some(t.with_timezone(&Utc)),
                date_only: false,
            });
        }
        // Offsets without a colon ("+0100") and minutes-only times.
        for f in ["%Y-%m-%dT%H:%M:%S%z", "%Y-%m-%dT%H:%M%z", "%Y-%m-%dT%H:%M:%S%.f%z"] {
            if let Ok(t) = DateTime::parse_from_str(s, f) {
                return Some(When {
                    value: s.into(),
                    utc: Some(t.with_timezone(&Utc)),
                    date_only: false,
                });
            }
        }
        for f in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
            if NaiveDateTime::parse_from_str(s, f).is_ok() {
                return Some(When {
                    value: s.into(),
                    utc: None,
                    date_only: false,
                });
            }
        }
        NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().map(|_| When {
            value: s.into(),
            utc: None,
            date_only: true,
        })
    }
}

/// One reservation card.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reservation {
    /// flight, lodging, train, bus, event, rental_car or restaurant
    pub kind: &'static str,
    pub title: String,
    pub reservation_number: Option<String>,
    pub under_name: Option<String>,
    pub start: Option<When>,
    pub end: Option<When>,
    /// Where it starts (airport, station, hotel, venue, pick-up).
    pub location: Option<String>,
    /// Further facts as label and value (to, seat, gate, party size, …).
    pub details: Vec<(String, String)>,
    /// A link to the booking (http/https only).
    pub url: Option<String>,
}

fn short_type(t: &str) -> &str {
    t.trim()
        .trim_start_matches("http://schema.org/")
        .trim_start_matches("https://schema.org/")
        .trim_start_matches("schema:")
}

fn type_of(v: &Value) -> Vec<String> {
    match v.get("@type") {
        Some(Value::String(s)) => vec![short_type(s).to_string()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(|s| short_type(s).to_string()).collect(),
        _ => Vec::new(),
    }
}

/// Text of a value: a string, a number, or an object's `name`.
fn text(v: Option<&Value>) -> Option<String> {
    let s = match v? {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(a) => return text(a.first()),
        Value::Object(o) => return text(o.get("name")),
        _ => return None,
    };
    (!s.is_empty()).then_some(s)
}

fn get<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cur = v;
    for k in path {
        cur = match cur {
            Value::Array(a) => a.first()?.get(*k)?,
            other => other.get(*k)?,
        };
    }
    Some(cur)
}

fn s(v: &Value, path: &[&str]) -> Option<String> {
    text(get(v, path))
}

fn address(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Object(o) => {
            let parts: Vec<String> = ["streetAddress", "postalCode", "addressLocality", "addressRegion", "addressCountry"]
                .iter()
                .filter_map(|k| text(o.get(*k)))
                .collect();
            (!parts.is_empty()).then(|| parts.join(", "))
        }
        _ => None,
    }
}

/// A place: its name and address.
fn place(v: Option<&Value>) -> Option<String> {
    let v = v?;
    let name = text(v.get("name")).or_else(|| text(Some(v)).filter(|_| v.is_string()));
    let code = s(v, &["iataCode"]);
    let addr = address(v.get("address"));
    let head = match (name, code) {
        (Some(n), Some(c)) => Some(format!("{n} ({c})")),
        (n, c) => n.or(c),
    };
    match (head, addr) {
        (Some(h), Some(a)) => Some(format!("{h}, {a}")),
        (h, a) => h.or(a),
    }
}

fn when(v: &Value, keys: &[&[&str]]) -> Option<When> {
    keys.iter().find_map(|k| get(v, k).and_then(Value::as_str).and_then(When::parse))
}

fn push(details: &mut Vec<(String, String)>, label: &str, value: Option<String>) {
    if let Some(v) = value {
        details.push((label.to_string(), v));
    }
}

fn http_url(v: Option<&Value>) -> Option<String> {
    let u = text(v)?;
    let lower = u.to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://")).then_some(u)
}

/// Turns one reservation object into a card.
fn card(r: &Value, ty: &str) -> Option<Reservation> {
    let f = r.get("reservationFor").unwrap_or(&Value::Null);
    let mut details = Vec::new();
    let reservation_number = s(r, &["reservationNumber"]).or_else(|| s(r, &["reservationId"]));
    let under_name = s(r, &["underName"]);
    let url = http_url(r.get("url")).or_else(|| http_url(r.get("modifyReservationUrl")));
    let mut c = match ty {
        "FlightReservation" => {
            let airline = s(f, &["airline", "iataCode"]).or_else(|| s(f, &["airline"]));
            let number = s(f, &["flightNumber"]);
            let flight = match (airline.clone(), number) {
                (Some(a), Some(n)) if !n.starts_with(&a) => format!("{a} {n}"),
                (_, Some(n)) => n,
                (Some(a), None) => a,
                (None, None) => "Flight".into(),
            };
            let from = s(f, &["departureAirport", "iataCode"]).or_else(|| s(f, &["departureAirport"]));
            let to = s(f, &["arrivalAirport", "iataCode"]).or_else(|| s(f, &["arrivalAirport"]));
            let title = match (&from, &to) {
                (Some(a), Some(b)) => format!("Flight {flight} {a} → {b}"),
                _ => format!("Flight {flight}"),
            };
            push(&mut details, "From", place(f.get("departureAirport")));
            push(&mut details, "To", place(f.get("arrivalAirport")));
            push(&mut details, "Airline", s(f, &["airline", "name"]));
            push(&mut details, "Terminal", s(f, &["departureTerminal"]));
            push(&mut details, "Gate", s(f, &["departureGate"]));
            push(
                &mut details,
                "Seat",
                s(r, &["airplaneSeat"]).or_else(|| s(r, &["reservedTicket", "ticketedSeat", "seatNumber"])),
            );
            push(&mut details, "Boarding", s(f, &["boardingTime"]).or_else(|| s(r, &["boardingTime"])));
            Reservation {
                kind: "flight",
                title,
                reservation_number: None,
                under_name: None,
                start: when(f, &[&["departureTime"]]),
                end: when(f, &[&["arrivalTime"]]),
                location: place(f.get("departureAirport")),
                details,
                url: None,
            }
        }
        "TrainReservation" | "BusReservation" => {
            let train = ty == "TrainReservation";
            let (num, name, dep, arr, mode) = if train {
                ("trainNumber", "trainName", "departureStation", "arrivalStation", "Train")
            } else {
                ("busNumber", "busName", "departureBusStop", "arrivalBusStop", "Bus")
            };
            let label = match (s(f, &[name]), s(f, &[num])) {
                (Some(n), Some(x)) if !n.contains(&x) => format!("{n} {x}"),
                (Some(n), _) => n,
                (None, Some(x)) => x,
                (None, None) => String::new(),
            };
            let (from, to) = (s(f, &[dep]), s(f, &[arr]));
            let title = match (&from, &to) {
                (Some(a), Some(b)) => format!("{mode} {label} {a} → {b}").replace("  ", " "),
                _ => format!("{mode} {label}").trim().to_string(),
            };
            push(&mut details, "From", place(f.get(dep)));
            push(&mut details, "To", place(f.get(arr)));
            push(&mut details, "Platform", s(f, &["departurePlatform"]));
            push(&mut details, "Operator", s(f, &["provider"]).or_else(|| s(r, &["provider"])));
            let seat = s(r, &["reservedTicket", "ticketedSeat", "seatNumber"]);
            let coach = s(r, &["reservedTicket", "ticketedSeat", "seatSection"]);
            push(&mut details, "Coach", coach);
            push(&mut details, "Seat", seat);
            Reservation {
                kind: if train { "train" } else { "bus" },
                title,
                reservation_number: None,
                under_name: None,
                start: when(f, &[&["departureTime"]]),
                end: when(f, &[&["arrivalTime"]]),
                location: place(f.get(dep)),
                details,
                url: None,
            }
        }
        "LodgingReservation" => {
            let name = s(f, &["name"]).unwrap_or_else(|| "Hotel".into());
            push(&mut details, "Address", address(f.get("address")));
            push(&mut details, "Phone", s(f, &["telephone"]));
            push(&mut details, "Guests", s(r, &["numGuests"]).or_else(|| s(r, &["numAdults"])));
            Reservation {
                kind: "lodging",
                title: name,
                reservation_number: None,
                under_name: None,
                start: when(r, &[&["checkinTime"], &["checkinDate"]]),
                end: when(r, &[&["checkoutTime"], &["checkoutDate"]]),
                location: address(f.get("address")).or_else(|| s(f, &["name"])),
                details,
                url: None,
            }
        }
        "EventReservation" => {
            let name = s(f, &["name"]).unwrap_or_else(|| "Event".into());
            push(&mut details, "Venue", place(f.get("location")));
            push(&mut details, "Seat", s(r, &["reservedTicket", "ticketedSeat", "seatNumber"]));
            push(&mut details, "Ticket", s(r, &["reservedTicket", "ticketNumber"]));
            Reservation {
                kind: "event",
                title: name,
                reservation_number: None,
                under_name: None,
                start: when(f, &[&["startDate"]]),
                end: when(f, &[&["endDate"]]),
                location: place(f.get("location")),
                details,
                url: None,
            }
        }
        "RentalCarReservation" => {
            let company = s(f, &["rentalCompany"]).or_else(|| s(r, &["provider"]));
            let car = [s(f, &["brand"]), s(f, &["model"])].into_iter().flatten().collect::<Vec<_>>().join(" ");
            let car = if car.is_empty() { s(f, &["name"]).unwrap_or_default() } else { car };
            let title = match (&company, car.is_empty()) {
                (Some(c), false) => format!("Rental car: {car} ({c})"),
                (Some(c), true) => format!("Rental car ({c})"),
                (None, false) => format!("Rental car: {car}"),
                (None, true) => "Rental car".into(),
            };
            push(&mut details, "Pick-up", place(r.get("pickupLocation")));
            push(&mut details, "Drop-off", place(r.get("dropoffLocation")));
            Reservation {
                kind: "rental_car",
                title,
                reservation_number: None,
                under_name: None,
                start: when(r, &[&["pickupTime"]]),
                end: when(r, &[&["dropoffTime"]]),
                location: place(r.get("pickupLocation")),
                details,
                url: None,
            }
        }
        "FoodEstablishmentReservation" => {
            let name = s(f, &["name"]).unwrap_or_else(|| "Restaurant".into());
            push(&mut details, "Address", address(f.get("address")));
            push(&mut details, "Party size", s(r, &["partySize"]));
            Reservation {
                kind: "restaurant",
                title: format!("Table at {name}"),
                reservation_number: None,
                under_name: None,
                start: when(r, &[&["startTime"]]),
                end: when(r, &[&["endTime"]]),
                location: address(f.get("address")).or_else(|| s(f, &["name"])),
                details,
                url: None,
            }
        }
        _ => return None,
    };
    c.reservation_number = reservation_number;
    c.under_name = under_name;
    c.url = url;
    Some(c)
}

/// Collects reservation objects (top-level, in arrays, in `@graph`, nested in other things).
fn collect(v: &Value, out: &mut Vec<Reservation>) {
    if out.len() >= MAX {
        return;
    }
    match v {
        Value::Array(a) => a.iter().for_each(|x| collect(x, out)),
        Value::Object(o) => {
            let types = type_of(v);
            if let Some(t) = types.iter().find(|t| TYPES.contains(&t.as_str())) {
                if let Some(c) = card(v, t) {
                    if !out.contains(&c) {
                        out.push(c);
                    }
                }
                return;
            }
            for (k, x) in o {
                if k != "@context" {
                    collect(x, out);
                }
            }
        }
        _ => {}
    }
}

// ---- a small DOM, enough for microdata and scripts -----------------------------------------

/// Elements that never have content.
const VOID: [&str; 14] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr",
];

enum Child {
    Node(usize),
    Text(String),
}

struct Node {
    tag: String,
    attrs: Vec<(String, String)>,
    children: Vec<Child>,
}

fn root() -> Node {
    Node {
        tag: String::new(),
        attrs: Vec::new(),
        children: Vec::new(),
    }
}

/// Builds the tree from html5ever's tokenizer: start tags open elements (void ones do not),
/// end tags close up to the matching open element, text goes to the current element.
struct DomSink {
    nodes: RefCell<Vec<Node>>,
    stack: RefCell<Vec<usize>>,
}

impl TokenSink for DomSink {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        let mut nodes = self.nodes.borrow_mut();
        let mut stack = self.stack.borrow_mut();
        let current = stack.last().copied().unwrap_or(0);
        match token {
            Token::TagToken(t) => {
                let name = (*t.name).to_ascii_lowercase();
                match t.kind {
                    TagKind::StartTag => {
                        let idx = nodes.len();
                        nodes.push(Node {
                            tag: name.clone(),
                            attrs: t.attrs.iter().map(|a| ((*a.name.local).to_ascii_lowercase(), a.value.to_string())).collect(),
                            children: Vec::new(),
                        });
                        nodes[current].children.push(Child::Node(idx));
                        if !t.self_closing && !VOID.contains(&name.as_str()) {
                            stack.push(idx);
                            match name.as_str() {
                                "script" => return TokenSinkResult::RawData(RawKind::ScriptData),
                                "style" => return TokenSinkResult::RawData(RawKind::Rawtext),
                                "title" | "textarea" => return TokenSinkResult::RawData(RawKind::Rcdata),
                                _ => {}
                            }
                        }
                    }
                    TagKind::EndTag => {
                        if let Some(pos) = stack.iter().rposition(|i| nodes[*i].tag == name) {
                            stack.truncate(pos);
                        }
                    }
                }
            }
            Token::CharacterTokens(text) => {
                let children = &mut nodes[current].children;
                match children.last_mut() {
                    Some(Child::Text(t)) => t.push_str(&text),
                    _ => children.push(Child::Text(text.to_string())),
                }
            }
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

struct Dom {
    nodes: Vec<Node>,
}

impl Dom {
    fn parse(html: &str) -> Dom {
        let sink = DomSink {
            nodes: RefCell::new(vec![root()]),
            stack: RefCell::new(Vec::new()),
        };
        let tok = Tokenizer::new(sink, TokenizerOpts::default());
        let input = BufferQueue::default();
        input.push_back(StrTendril::from(html));
        let _ = tok.feed(&input);
        tok.end();
        Dom {
            nodes: tok.sink.nodes.into_inner(),
        }
    }

    fn attr(&self, n: usize, name: &str) -> Option<String> {
        self.nodes[n].attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }

    fn children(&self, n: usize) -> impl Iterator<Item = usize> + '_ {
        self.nodes[n].children.iter().filter_map(|c| match c {
            Child::Node(i) => Some(*i),
            Child::Text(_) => None,
        })
    }

    fn text(&self, n: usize, out: &mut String) {
        for c in &self.nodes[n].children {
            match c {
                Child::Text(t) => out.push_str(t),
                Child::Node(i) => self.text(*i, out),
            }
        }
    }

    /// The value of a microdata property element (HTML §5.2.4).
    fn prop_value(&self, n: usize) -> Value {
        if self.attr(n, "itemscope").is_some() {
            return Value::Object(self.item(n));
        }
        let v = match self.nodes[n].tag.as_str() {
            "meta" => self.attr(n, "content"),
            "a" | "link" | "area" => self.attr(n, "href"),
            "img" | "audio" | "video" | "source" | "iframe" | "embed" => self.attr(n, "src"),
            "object" => self.attr(n, "data"),
            "time" => self.attr(n, "datetime"),
            "data" | "meter" => self.attr(n, "value"),
            _ => None,
        }
        .or_else(|| self.attr(n, "content"));
        Value::String(v.unwrap_or_else(|| {
            let mut s = String::new();
            self.text(n, &mut s);
            s.split_whitespace().collect::<Vec<_>>().join(" ")
        }))
    }

    /// Properties of an item: descendants with `itemprop` that are not inside a nested item.
    fn props(&self, n: usize, props: &mut Map<String, Value>) {
        for child in self.children(n) {
            if let Some(names) = self.attr(child, "itemprop") {
                let value = self.prop_value(child);
                for name in names.split_whitespace() {
                    props.entry(name.to_string()).or_insert_with(|| value.clone());
                }
                if self.attr(child, "itemscope").is_some() {
                    continue;
                }
            } else if self.attr(child, "itemscope").is_some() {
                continue;
            }
            self.props(child, props);
        }
    }

    fn item(&self, n: usize) -> Map<String, Value> {
        let mut m = Map::new();
        if let Some(t) = self.attr(n, "itemtype") {
            let t = t.split_whitespace().next().unwrap_or("").to_string();
            m.insert("@type".into(), Value::String(short_type(&t).to_string()));
        }
        self.props(n, &mut m);
        m
    }

    /// JSON-LD script texts and top-level microdata items.
    fn walk(&self, n: usize, json_ld: &mut Vec<String>, items: &mut Vec<Value>) {
        let node = &self.nodes[n];
        if node.tag == "script" && self.attr(n, "type").is_some_and(|t| t.to_ascii_lowercase().contains("ld+json")) {
            let mut s = String::new();
            self.text(n, &mut s);
            json_ld.push(s);
            return;
        }
        if self.attr(n, "itemscope").is_some() && self.attr(n, "itemprop").is_none() {
            items.push(Value::Object(self.item(n)));
            return;
        }
        for c in self.children(n) {
            self.walk(c, json_ld, items);
        }
    }
}

/// The reservations in a message's HTML.
pub fn extract(html: &str) -> Vec<Reservation> {
    let lower = html.to_ascii_lowercase();
    if !lower.contains("ld+json") && !lower.contains("itemscope") {
        return Vec::new();
    }
    let dom = Dom::parse(html);
    let (mut json_ld, mut items) = (Vec::new(), Vec::new());
    dom.walk(0, &mut json_ld, &mut items);
    let mut out = Vec::new();
    for block in json_ld {
        // Some senders wrap the JSON in HTML comments or CDATA.
        let cleaned = block
            .trim()
            .trim_start_matches("<!--")
            .trim_end_matches("-->")
            .trim()
            .trim_start_matches("//<![CDATA[")
            .trim_end_matches("//]]>")
            .trim();
        if let Ok(v) = serde_json::from_str::<Value>(cleaned) {
            collect(&v, &mut out);
        }
    }
    for it in &items {
        collect(it, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ld(json: &str) -> String {
        format!("<html><head><script type=\"application/ld+json\">{json}</script></head><body><p>Your booking</p></body></html>")
    }

    #[test]
    fn flights_from_json_ld() {
        let html = ld(
            r#"{"@context":"http://schema.org","@type":"FlightReservation","reservationNumber":"RXJ34P","reservationStatus":"http://schema.org/ReservationConfirmed",
            "underName":{"@type":"Person","name":"Eva Green"},
            "reservationFor":{"@type":"Flight","flightNumber":"110","airline":{"@type":"Airline","name":"United","iataCode":"UA"},
              "departureAirport":{"@type":"Airport","name":"San Francisco Airport","iataCode":"SFO"},"departureTime":"2027-03-04T20:15:00-08:00",
              "arrivalAirport":{"@type":"Airport","name":"John F. Kennedy International Airport","iataCode":"JFK"},"arrivalTime":"2027-03-05T06:30:00-05:00"},
            "airplaneSeat":"9A"}"#,
        );
        let r = extract(&html);
        assert_eq!(r.len(), 1);
        let f = &r[0];
        assert_eq!((f.kind, f.title.as_str()), ("flight", "Flight UA 110 SFO → JFK"));
        assert_eq!(f.reservation_number.as_deref(), Some("RXJ34P"));
        assert_eq!(f.under_name.as_deref(), Some("Eva Green"));
        assert_eq!(f.start.as_ref().unwrap().utc.unwrap().to_rfc3339(), "2027-03-05T04:15:00+00:00");
        assert_eq!(f.end.as_ref().unwrap().utc.unwrap().to_rfc3339(), "2027-03-05T11:30:00+00:00");
        assert_eq!(f.location.as_deref(), Some("San Francisco Airport (SFO)"));
        assert!(f.details.contains(&("Seat".into(), "9A".into())));
    }

    #[test]
    fn every_type_from_a_graph() {
        let html = ld(r#"{"@context":"https://schema.org","@graph":[
          {"@type":"LodgingReservation","reservationNumber":"abc456","checkinDate":"2027-04-11T16:00:00-08:00","checkoutDate":"2027-04-13T11:00:00-08:00",
           "reservationFor":{"@type":"LodgingBusiness","name":"Hilton San Francisco Union Square","telephone":"415-771-1400",
             "address":{"@type":"PostalAddress","streetAddress":"333 O'Farrell St","addressLocality":"San Francisco","addressRegion":"CA","postalCode":"94102","addressCountry":"US"}}},
          {"@type":"TrainReservation","reservationNumber":"Y4X4G","reservationFor":{"@type":"TrainTrip","trainNumber":"ICE 597","provider":{"@type":"Organization","name":"DB"},
             "departureStation":{"@type":"TrainStation","name":"Berlin Hbf"},"departureTime":"2027-05-02T09:28:00+02:00","departurePlatform":"14",
             "arrivalStation":{"@type":"TrainStation","name":"München Hbf"},"arrivalTime":"2027-05-02T13:31:00+02:00"},
           "reservedTicket":{"@type":"Ticket","ticketedSeat":{"@type":"Seat","seatNumber":"55","seatSection":"7"}}},
          {"@type":"BusReservation","reservationNumber":"B1","reservationFor":{"@type":"BusTrip","busName":"FlixBus","busNumber":"N123",
             "departureBusStop":{"@type":"BusStop","name":"Hamburg ZOB"},"departureTime":"2027-06-01T08:00:00+02:00",
             "arrivalBusStop":{"@type":"BusStop","name":"Berlin ZOB"},"arrivalTime":"2027-06-01T11:15:00+02:00"}},
          {"@type":"EventReservation","reservationNumber":"E123","reservationFor":{"@type":"MusicEvent","name":"Foo Fighters Concert","startDate":"2027-03-06T19:30:00-08:00",
             "location":{"@type":"Place","name":"AT&T Park","address":{"@type":"PostalAddress","streetAddress":"24 Willie Mays Plaza","addressLocality":"San Francisco"}}}},
          {"@type":"RentalCarReservation","reservationNumber":"546323","pickupTime":"2027-08-05T16:00:00-07:00","dropoffTime":"2027-08-06T20:00:00-07:00",
             "pickupLocation":{"@type":"Place","name":"Hertz San Diego Airport","address":{"@type":"PostalAddress","streetAddress":"1500 Orange Avenue","addressLocality":"San Diego"}},
             "dropoffLocation":{"@type":"Place","name":"Hertz LAX"},
             "reservationFor":{"@type":"Car","name":"Economy Class Car","model":"Civic","brand":{"@type":"Brand","name":"Honda"},"rentalCompany":{"@type":"Organization","name":"Hertz"}}},
          {"@type":"FoodEstablishmentReservation","reservationNumber":"OT12345","startTime":"2027-04-10T08:00:00-07:00","partySize":"2",
             "reservationFor":{"@type":"FoodEstablishment","name":"Wagamama","address":{"@type":"PostalAddress","streetAddress":"1 Tottenham Court Road","addressLocality":"London"}}}
        ]}"#);
        let r = extract(&html);
        let kinds: Vec<&str> = r.iter().map(|x| x.kind).collect();
        assert_eq!(kinds, ["lodging", "train", "bus", "event", "rental_car", "restaurant"]);
        assert_eq!(r[0].title, "Hilton San Francisco Union Square");
        assert_eq!(r[0].location.as_deref(), Some("333 O'Farrell St, 94102, San Francisco, CA, US"));
        assert_eq!(r[1].title, "Train ICE 597 Berlin Hbf → München Hbf");
        assert!(r[1].details.contains(&("Platform".into(), "14".into())) && r[1].details.contains(&("Coach".into(), "7".into())));
        assert_eq!(r[2].title, "Bus FlixBus N123 Hamburg ZOB → Berlin ZOB");
        assert_eq!(r[3].title, "Foo Fighters Concert");
        assert_eq!(r[3].location.as_deref(), Some("AT&T Park, 24 Willie Mays Plaza, San Francisco"));
        assert!(r[3].end.is_none());
        assert_eq!(r[4].title, "Rental car: Honda Civic (Hertz)");
        assert!(r[4].details.contains(&("Drop-off".into(), "Hertz LAX".into())));
        assert_eq!(r[5].title, "Table at Wagamama");
        assert!(r[5].details.contains(&("Party size".into(), "2".into())));
    }

    #[test]
    fn microdata() {
        let html = r#"<html><body><div itemscope itemtype="http://schema.org/FlightReservation">
          <meta itemprop="reservationNumber" content="RXJ34P"/>
          <link itemprop="reservationStatus" href="http://schema.org/Confirmed"/>
          <div itemprop="underName" itemscope itemtype="http://schema.org/Person"><meta itemprop="name" content="Eva Green"/></div>
          <div itemprop="reservationFor" itemscope itemtype="http://schema.org/Flight">
            <meta itemprop="flightNumber" content="110"/>
            <div itemprop="airline" itemscope itemtype="http://schema.org/Airline"><meta itemprop="name" content="United"/><meta itemprop="iataCode" content="UA"/></div>
            <div itemprop="departureAirport" itemscope itemtype="http://schema.org/Airport"><span itemprop="name">San Francisco Airport</span><meta itemprop="iataCode" content="SFO"/></div>
            <time itemprop="departureTime" datetime="2027-03-04T20:15:00-08:00">Mar 4, 8:15 PM</time>
            <div itemprop="arrivalAirport" itemscope itemtype="http://schema.org/Airport"><meta itemprop="name" content="JFK"/><meta itemprop="iataCode" content="JFK"/></div>
            <meta itemprop="arrivalTime" content="2027-03-05T06:30:00-05:00"/>
          </div></div><p>Thanks for flying</p></body></html>"#;
        let r = extract(html);
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].title, "Flight UA 110 SFO → JFK");
        assert_eq!(r[0].under_name.as_deref(), Some("Eva Green"));
        assert_eq!(r[0].start.as_ref().unwrap().utc.unwrap().to_rfc3339(), "2027-03-05T04:15:00+00:00");
    }

    #[test]
    fn ignores_other_markup_and_bad_json() {
        assert!(extract("<p>Hello</p>").is_empty());
        assert!(extract(&ld(r#"{"@type":"Organization","name":"Shop"}"#)).is_empty());
        assert!(extract(&ld("{not json")).is_empty());
        let w = When::parse("2027-04-11").unwrap();
        assert!(w.date_only && w.utc.is_none());
        assert!(When::parse("2027-04-11T16:00:00").unwrap().utc.is_none());
        assert!(When::parse("2027-04-11T16:00:00+0100").unwrap().utc.is_some());
    }
}
