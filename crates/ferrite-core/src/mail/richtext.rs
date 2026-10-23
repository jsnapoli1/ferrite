//! Rich text for composing: the whitelist sanitizer for what users write (the rich editor,
//! signatures, text blocks, quoted originals), the faithful plain-text rendering sent as the
//! `text/plain` alternative of HTML mail, text → HTML (switching modes, quoting plain
//! originals), `format=flowed` (RFC 3676) for plain-text parts, and finding and rewriting
//! image sources (local images become `cid:` related parts when a message is built).
//!
//! The web composer runs the same whitelist on paste (`web/js/editor/sanitize.js`); the
//! server applies it again to everything it stores or sends, so the client is never trusted.

use html5ever::buffer_queue::BufferQueue;
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::{TagKind, Token, TokenSink, TokenSinkResult, Tokenizer};
use std::cell::RefCell;
use std::collections::HashSet;

/// Elements kept in composed HTML.
const TAGS: &[&str] = &[
    "a",
    "b",
    "big",
    "blockquote",
    "br",
    "caption",
    "center",
    "code",
    "del",
    "div",
    "em",
    "font",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "i",
    "img",
    "ins",
    "li",
    "mark",
    "ol",
    "p",
    "pre",
    "s",
    "small",
    "span",
    "strike",
    "strong",
    "sub",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "u",
    "ul",
];

/// CSS properties kept in `style` attributes of composed HTML.
const STYLES: &[&str] = &[
    "background-color",
    "border",
    "border-collapse",
    "border-left",
    "border-left-color",
    "border-left-style",
    "border-left-width",
    "color",
    "font-family",
    "font-size",
    "font-style",
    "font-weight",
    "height",
    "line-height",
    "list-style-type",
    "margin",
    "margin-bottom",
    "margin-left",
    "margin-right",
    "margin-top",
    "max-width",
    "padding",
    "padding-left",
    "text-align",
    "text-decoration",
    "text-decoration-line",
    "vertical-align",
    "width",
];

/// The class marking a signature block (swapped when the sender changes; rendered after a
/// `-- ` line in plain text).
pub const SIGNATURE_CLASS: &str = "ferrite-signature";
/// The class marking the quoted original of a reply or forward.
pub const QUOTE_CLASS: &str = "ferrite-quote";

/// Image sources the composer may use: its own uploads and outbox copies, inline parts of
/// cached messages (quoted originals), embedded data and `cid:` references.
fn local_image(src: &str) -> bool {
    let s = src.trim();
    let lower = s.to_ascii_lowercase();
    (s.starts_with("/api/mail/") && !s.contains("..") && !s.contains("//"))
        || lower.starts_with("cid:")
        || ["data:image/png;", "data:image/jpeg;", "data:image/gif;", "data:image/webp;"]
            .iter()
            .any(|p| lower.starts_with(p))
}

fn link_ok(href: &str) -> bool {
    let l = href.trim().to_ascii_lowercase();
    ["http://", "https://", "mailto:", "tel:"].iter().any(|p| l.starts_with(p))
}

fn builder() -> ammonia::Builder<'static> {
    let mut b = ammonia::Builder::empty();
    b.tags(TAGS.iter().copied().collect())
        .generic_attributes(["style", "align", "dir", "title"].into_iter().collect())
        .add_tag_attributes("a", ["href"])
        .add_tag_attributes("img", ["src", "alt", "width", "height"])
        .add_tag_attributes("font", ["color", "size", "face"])
        .add_tag_attributes("blockquote", ["type"])
        .add_tag_attributes("ol", ["start", "type"])
        .add_tag_attributes("table", ["cellpadding", "cellspacing", "border", "width"])
        .add_tag_attributes("td", ["colspan", "rowspan", "width", "valign"])
        .add_tag_attributes("th", ["colspan", "rowspan", "width", "valign"])
        .add_allowed_classes("div", [SIGNATURE_CLASS, QUOTE_CLASS])
        .filter_style_properties(STYLES.iter().copied().collect::<HashSet<_>>())
        .clean_content_tags(
            ["script", "style", "head", "title", "template", "textarea", "select", "noscript"]
                .into_iter()
                .collect(),
        )
        .strip_comments(true)
        .link_rel(None)
        .url_schemes(["http", "https", "mailto", "tel", "cid", "data"].into_iter().collect())
        .attribute_filter(|element, attribute, value| match (element, attribute) {
            ("img", "src") => local_image(value).then(|| value.trim().into()),
            ("a", "href") => link_ok(value).then(|| value.trim().into()),
            // Declarations that could load or run something go; the property whitelist does
            // the rest.
            (_, "style") => Some(
                value
                    .split(';')
                    .filter(|d| {
                        let l = d.to_ascii_lowercase();
                        !l.contains("url(") && !l.contains("expression(") && !l.contains("javascript:")
                    })
                    .collect::<Vec<_>>()
                    .join(";")
                    .into(),
            ),
            _ => Some(value.into()),
        });
    b
}

// ---- a small DOM ------------------------------------------------------------------------
//
// Everything parsed here is ammonia's (or this module's own) serialized output, which is
// well formed: balanced tags, quoted attributes, no raw-text elements. A tree built from
// the tokenizer's output is all that needs.

const VOID: &[&str] = &[
    "br", "hr", "img", "wbr", "col", "area", "input", "meta", "link", "source", "base", "embed", "track", "param",
];

#[derive(Debug, Clone)]
enum Node {
    El(El),
    Text(String),
}

#[derive(Debug, Clone, Default)]
struct El {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<Node>,
}

impl El {
    fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
    fn has_class(&self, class: &str) -> bool {
        self.attr("class").is_some_and(|c| c.split_whitespace().any(|x| x == class))
    }
}

struct TreeSink {
    stack: RefCell<Vec<El>>,
}

impl TreeSink {
    fn close_top(stack: &mut Vec<El>) {
        if stack.len() > 1 {
            let el = stack.pop().expect("stack has a child");
            stack.last_mut().expect("stack has a root").children.push(Node::El(el));
        }
    }
}

impl TokenSink for TreeSink {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        let mut stack = self.stack.borrow_mut();
        match token {
            Token::TagToken(tag) => {
                let name = tag.name.to_string();
                match tag.kind {
                    TagKind::StartTag => {
                        let el = El {
                            attrs: tag.attrs.iter().map(|a| (a.name.local.to_string(), a.value.to_string())).collect(),
                            name,
                            children: Vec::new(),
                        };
                        if VOID.contains(&el.name.as_str()) || tag.self_closing {
                            stack.last_mut().expect("stack has a root").children.push(Node::El(el));
                        } else {
                            stack.push(el);
                        }
                    }
                    TagKind::EndTag => {
                        if let Some(pos) = stack.iter().skip(1).rposition(|e| e.name == name) {
                            while stack.len() > pos + 1 {
                                Self::close_top(&mut stack);
                            }
                        }
                    }
                }
            }
            Token::CharacterTokens(t) => {
                let top = stack.last_mut().expect("stack has a root");
                match top.children.last_mut() {
                    Some(Node::Text(prev)) => prev.push_str(&t),
                    _ => top.children.push(Node::Text(t.to_string())),
                }
            }
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

/// Parses well-formed HTML into the children of a root element.
fn parse(html: &str) -> El {
    let sink = TreeSink {
        stack: RefCell::new(vec![El::default()]),
    };
    let queue = BufferQueue::default();
    queue.push_back(StrTendril::from_slice(html));
    let tok = Tokenizer::new(sink, Default::default());
    let _ = tok.feed(&queue);
    tok.end();
    let mut stack = tok.sink.stack.into_inner();
    while stack.len() > 1 {
        TreeSink::close_top(&mut stack);
    }
    stack.pop().unwrap_or_default()
}

fn escape_text(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\u{a0}' => out.push_str("&nbsp;"),
            _ => out.push(c),
        }
    }
}

fn escape_attr(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\u{a0}' => out.push_str("&nbsp;"),
            _ => out.push(c),
        }
    }
}

fn serialize(nodes: &[Node], out: &mut String) {
    for n in nodes {
        match n {
            Node::Text(t) => escape_text(t, out),
            Node::El(e) => {
                out.push('<');
                out.push_str(&e.name);
                for (k, v) in &e.attrs {
                    out.push(' ');
                    out.push_str(k);
                    out.push_str("=\"");
                    escape_attr(v, out);
                    out.push('"');
                }
                out.push('>');
                if VOID.contains(&e.name.as_str()) {
                    continue;
                }
                serialize(&e.children, out);
                out.push_str("</");
                out.push_str(&e.name);
                out.push('>');
            }
        }
    }
}

fn to_html(root: &El) -> String {
    let mut out = String::new();
    serialize(&root.children, &mut out);
    out
}

fn walk(el: &El, f: &mut dyn FnMut(&El)) {
    for c in &el.children {
        if let Node::El(e) = c {
            f(e);
            walk(e, f);
        }
    }
}

/// Changes elements depth first; those for which `f` returns false are removed.
fn transform(el: &mut El, f: &mut dyn FnMut(&mut El) -> bool) {
    el.children.retain_mut(|c| match c {
        Node::El(e) => {
            transform(e, f);
            f(e)
        }
        Node::Text(_) => true,
    });
}

/// Sanitizes composed HTML through the whitelist: no scripts, styles blocks, forms, frames,
/// event handlers, remote images or CSS resources; links only to http(s), mailto and tel.
/// Images without an allowed source are removed rather than left broken.
pub fn sanitize(html: &str) -> String {
    // Zero-width spaces only hold the caret in the editor.
    let clean = builder().clean(&html.replace('\u{200b}', "")).to_string();
    let mut root = parse(&clean);
    transform(&mut root, &mut |e| !(e.name == "img" && e.attr("src").is_none()));
    to_html(&root)
}

/// The image sources of (sanitized) HTML, in document order, without duplicates.
pub fn image_sources(html: &str) -> Vec<String> {
    let root = parse(html);
    let mut out: Vec<String> = Vec::new();
    walk(&root, &mut |e| {
        if e.name == "img" {
            if let Some(src) = e.attr("src") {
                if !out.iter().any(|x| x == src) {
                    out.push(src.to_string());
                }
            }
        }
    });
    out
}

/// Replaces image sources with `map(src)`; images mapped to `None` are removed.
pub fn rewrite_images(html: &str, map: impl Fn(&str) -> Option<String>) -> String {
    let mut root = parse(html);
    transform(&mut root, &mut |e| {
        if e.name != "img" {
            return true;
        }
        match e.attr("src").and_then(&map) {
            Some(new) => {
                if let Some(a) = e.attrs.iter_mut().find(|(k, _)| k == "src") {
                    a.1 = new;
                }
                true
            }
            None => false,
        }
    });
    to_html(&root)
}

/// Whether HTML has any visible content (text or images).
pub fn has_content(html: &str) -> bool {
    !html_to_text(html).trim().is_empty() || !image_sources(html).is_empty()
}

// ---- HTML → plain text ------------------------------------------------------------------

struct TextWriter {
    /// Finished lines with their quote depth.
    lines: Vec<(usize, String)>,
    cur: String,
    /// Quote depth of the line being written (fixed when it gets its first character).
    cur_depth: usize,
    depth: usize,
    space: bool,
    pre: usize,
    lists: Vec<Option<usize>>,
}

impl TextWriter {
    fn new() -> Self {
        TextWriter {
            lines: Vec::new(),
            cur: String::new(),
            cur_depth: 0,
            depth: 0,
            space: false,
            pre: 0,
            lists: Vec::new(),
        }
    }

    fn start_line_if_needed(&mut self) {
        if self.cur.is_empty() {
            self.cur_depth = self.depth;
        }
    }

    fn word(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.start_line_if_needed();
        if self.space && !self.cur.is_empty() {
            self.cur.push(' ');
        }
        self.space = false;
        self.cur.push_str(s);
    }

    fn text(&mut self, t: &str) {
        if self.pre > 0 {
            let mut first = true;
            for part in t.split('\n') {
                if !first {
                    self.force_line();
                }
                first = false;
                self.start_line_if_needed();
                self.cur.push_str(part);
            }
            return;
        }
        let mut word = String::new();
        for c in t.chars().filter(|c| *c != '\u{200b}') {
            if c.is_whitespace() && c != '\u{a0}' {
                if !word.is_empty() {
                    self.word(&std::mem::take(&mut word));
                }
                self.space = true;
            } else {
                word.push(if c == '\u{a0}' { ' ' } else { c });
            }
        }
        if !word.is_empty() {
            self.word(&word);
        }
    }

    /// Ends the current line (a line break, even when the line is empty).
    fn force_line(&mut self) {
        self.start_line_if_needed();
        let line = std::mem::take(&mut self.cur);
        let line = if line == "-- " { line } else { line.trim_end().to_string() };
        self.lines.push((self.cur_depth, line));
        self.space = false;
    }

    /// Ends the current line if anything is on it.
    fn newline(&mut self) {
        if !self.cur.is_empty() {
            self.force_line();
        }
        self.space = false;
    }

    /// A paragraph break: one blank line (never two, none at the start or right after the
    /// signature separator).
    fn blank(&mut self) {
        self.newline();
        match self.lines.last() {
            Some((_, l)) if l.is_empty() || l == "-- " => {}
            None => {}
            _ => self.lines.push((self.depth, String::new())),
        }
    }

    fn finish(mut self) -> String {
        self.newline();
        let mut out: Vec<String> = self
            .lines
            .into_iter()
            .map(|(d, t)| {
                if d == 0 {
                    t
                } else if t.is_empty() {
                    ">".repeat(d)
                } else {
                    format!("{} {t}", ">".repeat(d))
                }
            })
            .collect();
        while out.last().is_some_and(|l| l.is_empty()) {
            out.pop();
        }
        while out.first().is_some_and(|l| l.is_empty()) {
            out.remove(0);
        }
        // Blank quote lines between quote and text collapse like blank lines.
        let mut text = String::new();
        let mut prev_blank = false;
        for l in out {
            let blank = l.trim_start_matches('>').is_empty();
            if blank && prev_blank && l.is_empty() {
                continue;
            }
            prev_blank = blank;
            text.push_str(&l);
            text.push('\n');
        }
        text.trim_end_matches('\n').to_string()
    }
}

fn has_text(nodes: &[Node]) -> bool {
    nodes.iter().any(|n| match n {
        Node::Text(t) => !t.trim().is_empty(),
        Node::El(e) => e.name == "img" || has_text(&e.children),
    })
}

fn text_of(nodes: &[Node]) -> String {
    fn go(nodes: &[Node], s: &mut String) {
        for n in nodes {
            match n {
                Node::Text(t) => s.push_str(t),
                Node::El(e) => go(&e.children, s),
            }
        }
    }
    let mut s = String::new();
    go(nodes, &mut s);
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn render_all(nodes: &[Node], w: &mut TextWriter) {
    for n in nodes {
        match n {
            Node::Text(t) => w.text(t),
            Node::El(e) => render(e, w),
        }
    }
}

fn render(e: &El, w: &mut TextWriter) {
    let children = |w: &mut TextWriter| render_all(&e.children, w);
    match e.name.as_str() {
        "script" | "style" | "head" | "title" | "template" => {}
        "br" => w.force_line(),
        "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "table" => {
            w.blank();
            children(w);
            w.blank();
        }
        "hr" => {
            w.blank();
            w.word("----------");
            w.blank();
        }
        "pre" => {
            w.blank();
            w.pre += 1;
            children(w);
            w.pre -= 1;
            w.blank();
        }
        "blockquote" => {
            w.blank();
            w.depth += 1;
            children(w);
            w.newline();
            // A blank quoted line at the end of the quote is not wanted.
            if w.lines.last().is_some_and(|(d, l)| *d == w.depth && l.is_empty()) {
                w.lines.pop();
            }
            w.depth -= 1;
            w.blank();
        }
        "ul" | "ol" => {
            if w.lists.is_empty() {
                w.blank();
            } else {
                w.newline();
            }
            let start = e.attr("start").and_then(|s| s.parse().ok()).unwrap_or(1usize);
            w.lists.push((e.name == "ol").then_some(start));
            children(w);
            w.lists.pop();
            if w.lists.is_empty() {
                w.blank();
            } else {
                w.newline();
            }
        }
        "li" => {
            w.newline();
            let indent = "  ".repeat(w.lists.len().saturating_sub(1));
            let marker = match w.lists.last_mut() {
                Some(Some(k)) => {
                    let m = format!("{k}.");
                    *k += 1;
                    m
                }
                _ => "-".to_string(),
            };
            w.word(&format!("{indent}{marker}"));
            w.space = true;
            children(w);
            w.newline();
        }
        "div" if e.has_class(SIGNATURE_CLASS) => {
            w.newline();
            if has_text(&e.children) {
                w.word("-- ");
                w.force_line();
                children(w);
            }
            w.newline();
        }
        "div" | "tr" | "center" | "caption" | "thead" | "tbody" | "tfoot" => {
            w.newline();
            children(w);
            w.newline();
        }
        "td" | "th" => {
            children(w);
            w.space = true;
        }
        "b" | "strong" | "u" | "i" | "em" if has_text(&e.children) => {
            let mark = match e.name.as_str() {
                "b" | "strong" => '*',
                "u" => '_',
                _ => '/',
            };
            w.word(&mark.to_string());
            w.space = false;
            children(w);
            let space = w.space;
            w.start_line_if_needed();
            w.cur.push(mark);
            w.space = space;
        }
        "a" => {
            children(w);
            if let Some(href) = e.attr("href") {
                let label = text_of(&e.children);
                let bare = href.strip_prefix("mailto:").unwrap_or(href);
                if !label.is_empty() && label != href && label != bare {
                    w.space = true;
                    w.word(&format!("<{href}>"));
                } else if label.is_empty() {
                    w.word(href);
                }
            }
        }
        "img" => {
            if let Some(alt) = e.attr("alt").filter(|a| !a.trim().is_empty()) {
                w.word(&format!("[{}]", alt.trim()));
            }
        }
        _ => children(w),
    }
}

/// The plain-text rendering of HTML sent as the `text/plain` alternative: paragraphs and
/// line breaks, `> ` quoting for blockquotes, `-`/`1.` lists, `*bold*`, `/italic/`,
/// `_underline_`, links as `text <url>`, image alt text, a `-- ` line before signatures.
pub fn html_to_text(html: &str) -> String {
    let mut w = TextWriter::new();
    render_all(&parse(html).children, &mut w);
    w.finish()
}

// ---- text → HTML ------------------------------------------------------------------------

/// Text escaped for HTML (element content and quoted attributes).
pub(crate) fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Escaped text with http(s) URLs as links.
fn linkify(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(p) = rest.find("http://").into_iter().chain(rest.find("https://")).min() {
        out.push_str(&escape(&rest[..p]));
        let tail = &rest[p..];
        let end = tail.find(|c: char| c.is_whitespace() || "<>\"')]".contains(c)).unwrap_or(tail.len());
        let url = tail[..end].trim_end_matches(['.', ',', ';', ':', '!', '?']);
        out.push_str(&format!("<a href=\"{}\">{}</a>", escape(url), escape(url)));
        rest = &tail[url.len()..];
    }
    out.push_str(&escape(rest));
    out
}

/// Quote depth and content of a plain-text line (`> > text`, `>> text`).
fn quote_depth(line: &str) -> (usize, &str) {
    let mut depth = 0;
    let mut rest = line;
    while let Some(r) = rest.strip_prefix('>') {
        depth += 1;
        rest = r.strip_prefix(' ').unwrap_or(r);
        if !rest.starts_with('>') {
            break;
        }
    }
    (depth, rest)
}

/// Plain text as HTML: one `<div>` per line (`<div><br></div>` for empty ones, which is what
/// the browser's editor produces too), `>` quoting as nested blockquotes, URLs as links.
pub fn text_to_html(text: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for line in text.replace("\r\n", "\n").split('\n') {
        let (d, content) = quote_depth(line);
        while depth < d {
            out.push_str("<blockquote type=\"cite\">");
            depth += 1;
        }
        while depth > d {
            out.push_str("</blockquote>");
            depth -= 1;
        }
        if content.is_empty() {
            out.push_str("<div><br></div>");
        } else {
            out.push_str(&format!("<div>{}</div>", linkify(content)));
        }
    }
    while depth > 0 {
        out.push_str("</blockquote>");
        depth -= 1;
    }
    out
}

/// A signature block: the HTML signature, or the text one converted.
pub fn signature_block(html: Option<&str>, text: &str) -> String {
    let inner = match html.map(sanitize).filter(|h| has_content(h)) {
        Some(h) => h,
        None if !text.trim().is_empty() => text_to_html(text.trim_end()),
        None => return String::new(),
    };
    format!("<div class=\"{SIGNATURE_CLASS}\">{inner}</div>")
}

// ---- format=flowed ----------------------------------------------------------------------

/// Longest line of a flowed part (RFC 3676 recommends 78 characters at most).
const FLOWED_WIDTH: usize = 76;

/// Encodes text as `format=flowed` (RFC 3676, DelSp=No): long lines are wrapped at spaces
/// with a trailing space marking each soft break, trailing spaces of the original lines are
/// removed (they would read as soft breaks), quote markers are written without spaces
/// between them followed by one space, and lines starting with a space, `From ` or `>` are
/// space-stuffed. The signature separator `-- ` stays as it is.
pub fn flowed(text: &str) -> String {
    let mut out = Vec::new();
    for raw in text.replace("\r\n", "\n").split('\n') {
        if raw == "-- " {
            out.push(raw.to_string());
            continue;
        }
        let (depth, content) = if raw.starts_with('>') { quote_depth(raw) } else { (0, raw) };
        let content = content.trim_end_matches(' ');
        let prefix = if depth > 0 { format!("{} ", ">".repeat(depth)) } else { String::new() };
        let stuff = depth == 0 && (content.starts_with(' ') || content.starts_with("From ") || content.starts_with('>'));
        let lead = if stuff { format!("{prefix} ") } else { prefix };
        let width = FLOWED_WIDTH.saturating_sub(lead.chars().count()).max(20);
        if content.chars().count() <= width {
            out.push(format!("{lead}{content}"));
            continue;
        }
        // Wrap at spaces: every piece but the last keeps its trailing space (a soft break).
        let cont = if depth > 0 { format!("{} ", ">".repeat(depth)) } else { String::new() };
        let line_lead = |first: bool, piece: &str| {
            if first {
                lead.clone()
            } else if depth == 0 && (piece.starts_with(' ') || piece.starts_with("From ") || piece.starts_with('>')) {
                format!("{cont} ")
            } else {
                cont.clone()
            }
        };
        let mut piece = String::new();
        let mut first = true;
        for word in content.split_inclusive(' ') {
            if !piece.is_empty() && piece.chars().count() + word.trim_end_matches(' ').chars().count() > width {
                out.push(format!("{}{piece}", line_lead(first, &piece)));
                piece.clear();
                first = false;
            }
            piece.push_str(word);
        }
        out.push(format!("{}{}", line_lead(first, &piece), piece.trim_end_matches(' ')));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decodes format=flowed (DelSp=No) the way a receiving client does.
    fn unflow(s: &str) -> String {
        let mut out: Vec<String> = Vec::new();
        let mut open: Option<(usize, String)> = None;
        for line in s.split('\n') {
            if line == "-- " {
                if let Some((d, t)) = open.take() {
                    out.push(format!("{}{}", if d > 0 { format!("{} ", ">".repeat(d)) } else { String::new() }, t));
                }
                out.push(line.to_string());
                continue;
            }
            let depth = line.chars().take_while(|c| *c == '>').count();
            let mut rest = &line[depth..];
            rest = rest.strip_prefix(' ').unwrap_or(rest);
            let soft = rest.ends_with(' ');
            match &mut open {
                Some((d, t)) if *d == depth => t.push_str(rest),
                _ => {
                    if let Some((d, t)) = open.take() {
                        out.push(format!("{}{}", if d > 0 { format!("{} ", ">".repeat(d)) } else { String::new() }, t));
                    }
                    open = Some((depth, rest.to_string()));
                }
            }
            if !soft {
                let (d, t) = open.take().unwrap();
                out.push(format!("{}{}", if d > 0 { format!("{} ", ">".repeat(d)) } else { String::new() }, t));
            }
        }
        if let Some((d, t)) = open.take() {
            out.push(format!("{}{}", if d > 0 { format!("{} ", ">".repeat(d)) } else { String::new() }, t));
        }
        out.join("\n")
    }

    #[test]
    fn sanitizer_keeps_formatting_and_drops_everything_dangerous() {
        let dirty = r#"<html><head><style>p{color:red}</style><title>t</title></head><body>
<p style="color: #c2410c; position: fixed; background-image: url(http://x/y)" onclick="evil()">Hi <b>there</b> <i>you</i> <u>u</u> <s>s</s></p>
<h2>Head</h2><ul><li>one</li><li>two</li></ul><ol start="3"><li>three</li></ol>
<blockquote type="cite">quoted</blockquote><a href="javascript:alert(1)">bad</a> <a href="https://ok.example/" target="_blank">ok</a>
<img src="https://tracker.example/p.gif"><img src="/api/mail/uploads/abc/content" alt="pic" onerror="x()"><img src="data:image/png;base64,AAAA">
<img src="data:text/html;base64,PHNjcmlwdD4="><script>alert(1)</script><iframe src="https://x"></iframe><form><input></form>
<div class="ferrite-signature evil">Sig</div><span class="x" data-x="1">s</span><!-- comment --></body></html>"#;
        let s = sanitize(dirty);
        for bad in [
            "script",
            "onclick",
            "onerror",
            "iframe",
            "<form",
            "<input",
            "javascript:",
            "tracker.example",
            "data:text",
            "position",
            "url(",
            "<style",
            "comment",
            "data-x",
            "target=",
        ] {
            assert!(!s.contains(bad), "{bad} survived: {s}");
        }
        for good in [
            "<b>there</b>",
            "<i>you</i>",
            "<u>u</u>",
            "<s>s</s>",
            "<h2>Head</h2>",
            "<li>one</li>",
            "start=\"3\"",
            "<blockquote type=\"cite\">",
            "href=\"https://ok.example/\"",
            "src=\"/api/mail/uploads/abc/content\"",
            "data:image/png;base64,AAAA",
            "style=\"color:#c2410c\"",
            "class=\"ferrite-signature\"",
        ] {
            assert!(s.contains(good), "{good} missing: {s}");
        }
        assert_eq!(s.matches("<img").count(), 2, "images without an allowed source are removed: {s}");
        assert!(!s.contains("class=\"x\""));
        assert_eq!(image_sources(&s), vec!["/api/mail/uploads/abc/content", "data:image/png;base64,AAAA"]);
    }

    #[test]
    fn images_are_rewritten_or_removed() {
        let html = "<p>a<img src=\"/api/mail/uploads/1/content\" alt=\"one\"><img src=\"/api/mail/uploads/2/content\"></p>";
        let out = rewrite_images(html, |src| src.contains("/1/").then(|| "cid:one@ferrite".to_string()));
        assert!(out.contains("src=\"cid:one@ferrite\"") && !out.contains("/2/"), "{out}");
    }

    #[test]
    fn html_renders_as_faithful_text() {
        let html = "<p>Hello <b>Bob</b>,</p><p>see <a href=\"https://example.org/q3\">the report</a> and <a href=\"https://example.org\">https://example.org</a>.</p>\
<ul><li>first</li><li>second<ul><li>nested</li></ul></li></ul><ol><li>one</li><li>two</li></ol>\
<div>line one<br>line two</div><div><br></div><div>after blank</div><hr><h1>Title</h1>\
<blockquote type=\"cite\"><p>quoted <i>text</i></p><blockquote><p>older</p></blockquote></blockquote>\
<pre>  keep   spaces\n  here</pre><img src=\"cid:x\" alt=\"Logo\"><u>under</u>\
<div class=\"ferrite-signature\"><p>Alice &amp; Co</p></div>";
        let t = html_to_text(html);
        let expected = "Hello *Bob*,\n\nsee the report <https://example.org/q3> and https://example.org.\n\n- first\n- second\n  - nested\n\n1. one\n2. two\n\nline one\nline two\n\nafter blank\n\n----------\n\nTitle\n\n> quoted /text/\n>\n>> older\n\n  keep   spaces\n  here\n\n[Logo]_under_\n-- \nAlice & Co";
        assert_eq!(t, expected);
        assert_eq!(html_to_text("<div>a</div><div>b</div>"), "a\nb");
        assert_eq!(html_to_text(""), "");
        assert_eq!(html_to_text("<a href=\"mailto:a@b.c\">a@b.c</a>"), "a@b.c");
    }

    #[test]
    fn text_becomes_html_and_back() {
        let text = "Hi <you> & me\n\nhttps://example.org/a?b=1&c=2.\n> quoted\n>> older\n> back\nend";
        let html = text_to_html(text);
        assert!(html.contains("<div>Hi &lt;you&gt; &amp; me</div><div><br></div>"), "{html}");
        assert!(html.contains("<a href=\"https://example.org/a?b=1&amp;c=2\">"), "{html}");
        assert!(
            html.contains(
                "<blockquote type=\"cite\"><div>quoted</div><blockquote type=\"cite\"><div>older</div></blockquote><div>back</div></blockquote><div>end</div>"
            ),
            "{html}"
        );
        assert_eq!(sanitize(&html), html, "text_to_html output passes the sanitizer unchanged");
        let back = html_to_text(&html);
        assert_eq!(
            back,
            "Hi <you> & me\n\nhttps://example.org/a?b=1&c=2.\n\n> quoted\n>\n>> older\n>\n> back\n\nend"
        );
        assert!(signature_block(None, "Alice\nCEO").starts_with("<div class=\"ferrite-signature\"><div>Alice</div>"));
        assert_eq!(signature_block(Some("<p></p>"), ""), "");
    }

    #[test]
    fn flowed_wraps_stuffs_and_round_trips() {
        let long = "This is a rather long line that certainly needs to be wrapped because it goes on and on beyond the limit.";
        let text = format!(
            "{long}\nFrom here on\n leading space\n> quoted line that is also quite long so that it has to be wrapped in the flowed output form\n>> deeper\ntrailing   \n-- \nAlice"
        );
        let f = flowed(&text);
        for line in f.split('\n') {
            assert!(line.chars().count() <= 78, "line too long: {line:?}");
        }
        assert!(f.contains("\n From here on\n"), "From is stuffed: {f}");
        assert!(f.contains("\n  leading space\n"), "leading space is stuffed: {f}");
        assert!(f.contains("\n>> deeper\n"));
        assert!(f.contains("\ntrailing\n"), "trailing spaces removed: {f}");
        assert!(f.ends_with("\n-- \nAlice"), "the signature separator stays: {f}");
        let first = f.lines().next().unwrap();
        assert!(first.ends_with(' '), "soft break keeps a trailing space: {first:?}");
        let decoded = unflow(&f);
        assert!(decoded.starts_with(long), "{decoded}");
        assert!(
            decoded.contains("> quoted line that is also quite long so that it has to be wrapped in the flowed output form\n>> deeper"),
            "{decoded}"
        );
        assert!(decoded.contains("\nFrom here on\n") || decoded.contains("\n From here on\n"));
    }
}
