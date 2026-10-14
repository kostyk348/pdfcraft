//! The email model: an address list, one chosen body (plain or HTML) and the attachments. This is
//! what Create turns into a PDF ([`EmailDocument`]).

use crate::decode;
use crate::header::{self, Headers};
use crate::mime;

/// The most a multipart message may nest before further parts are read as attachments.
const MAX_DEPTH: usize = 20;
/// The most parts one message may contain.
const MAX_PARTS: usize = 1_000;
/// The most HTML turned into text (a body larger than this is truncated, not refused).
const MAX_HTML: usize = 1 << 20;

/// The body of an email, as the sender wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    Plain(String),
    Html(String),
}

/// One attachment, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub filename: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
    pub inline: bool,
    pub content_id: Option<String>,
}

/// What an attachment is, without its bytes — what the PDF's attachment list shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentMeta {
    pub filename: String,
    pub media_type: String,
    pub size: usize,
}

/// An email read for Create: a title, the header lines to show, one body and the attachment list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailDocument {
    pub title: String,
    pub headers: Vec<(String, String)>,
    pub body: Body,
    pub attachments: Vec<AttachmentMeta>,
}

impl EmailDocument {
    /// The body as plain text: a text part as written, or an HTML part with its markup removed.
    pub fn body_text(&self) -> String {
        match &self.body {
            Body::Plain(text) => text.clone(),
            Body::Html(html) => html_to_text(html),
        }
    }

    /// One line per attachment, e.g. `report.pdf — application/pdf (1.2 MB)`.
    pub fn attachment_lines(&self) -> Vec<String> {
        self.attachments.iter().map(|a| format!("{} — {} ({})", a.filename, a.media_type, size_label(a.size))).collect()
    }
}

fn size_label(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// A parsed email.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Email {
    pub headers: Vec<(String, String)>,
    pub subject: String,
    pub from: Vec<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub date: Option<String>,
    pub body: Option<Body>,
    pub attachments: Vec<Attachment>,
}

impl Email {
    /// Whether `bytes` named `name` look like an email. A `.eml` name is enough; otherwise the
    /// first line must be a field no other format starts a file with.
    pub fn is_likely(name: &str, bytes: &[u8]) -> bool {
        if name.to_ascii_lowercase().ends_with(".eml") {
            return true;
        }
        let head = bytes.get(..bytes.len().min(4096)).unwrap_or(bytes);
        let text = String::from_utf8_lossy(head);
        let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        ["Received:", "Return-Path:", "MIME-Version:", "Delivered-To:"].iter().any(|f| first.starts_with(f))
    }

    /// Parse an RFC 822 / MIME message. Never fails: a malformed message yields what could be read.
    pub fn parse(data: &[u8]) -> Email {
        let (head, body) = mime::split(data);
        let headers = Headers::parse(head);
        let mut collected = Collected::default();
        walk(head, body, 0, &mut collected);
        Email {
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            subject: header::decode_words(headers.get("Subject").unwrap_or_default()),
            from: addresses(headers.get_all("From")),
            to: addresses(headers.get_all("To")),
            cc: addresses(headers.get_all("Cc")),
            date: headers.get("Date").map(header::decode_words),
            body: collected.take_body(),
            attachments: collected.attachments,
        }
    }

    /// The model Create renders.
    pub fn to_document(&self) -> EmailDocument {
        let mut headers = Vec::new();
        for (label, values) in [("From", &self.from), ("To", &self.to), ("Cc", &self.cc)] {
            if !values.is_empty() {
                headers.push((label.to_string(), values.join(", ")));
            }
        }
        if let Some(date) = &self.date {
            headers.push(("Date".to_string(), date.clone()));
        }
        headers.push(("Subject".to_string(), self.subject.clone()));
        EmailDocument {
            title: self.subject.clone(),
            headers,
            body: self.body.clone().unwrap_or(Body::Plain(String::new())),
            attachments: self
                .attachments
                .iter()
                // A part embedded in the body (`inline`, with a Content-ID) is not an attachment.
                .filter(|a| !a.inline)
                .map(|a| AttachmentMeta { filename: a.filename.clone(), media_type: a.media_type.clone(), size: a.bytes.len() })
                .collect(),
        }
    }
}

/// What one walk of the message gathered.
#[derive(Default)]
struct Collected {
    parts: usize,
    bodies: Vec<(String, String)>,
    attachments: Vec<Attachment>,
}

impl Collected {
    /// The body to show: HTML if the message offered it, otherwise plain text.
    fn take_body(&self) -> Option<Body> {
        if let Some((_, html)) = self.bodies.iter().find(|(m, _)| m == "text/html") {
            return Some(Body::Html(html.clone()));
        }
        self.bodies.iter().find(|(m, _)| m == "text/plain").map(|(_, t)| Body::Plain(t.clone()))
    }
}

fn walk(head: &[u8], body: &[u8], depth: usize, out: &mut Collected) {
    if out.parts >= MAX_PARTS {
        return;
    }
    out.parts = out.parts.saturating_add(1);
    let headers = Headers::parse(head);
    let ct = mime::content_type(headers.get("Content-Type").unwrap_or("text/plain"));
    if ct.is_multipart() && depth < MAX_DEPTH {
        let boundary = match ct.param("boundary") {
            Some(b) if !b.is_empty() => b.to_string(),
            _ => return,
        };
        for part in mime::split_multipart(body, &boundary) {
            let (part_head, part_body) = mime::split(&part);
            walk(part_head, part_body, depth.saturating_add(1), out);
        }
        return;
    }
    let encoding = headers.get("Content-Transfer-Encoding").unwrap_or("7bit").trim().to_ascii_lowercase();
    let raw = decode_body(body, &encoding);
    let disp = mime::disposition(headers.get("Content-Disposition").unwrap_or(""));
    let filename = disp.param("filename").map(header::decode_words).filter(|s| !s.is_empty());
    let is_attachment = disp.media_type.eq_ignore_ascii_case("attachment") || filename.is_some();
    let inline = disp.media_type.eq_ignore_ascii_case("inline");
    let charset = ct.param("charset").unwrap_or("utf-8");
    let content_id = headers.get("Content-ID").map(|s| s.trim().trim_matches(|c| c == '<' || c == '>').to_string());
    match ct.media_type.as_str() {
        "text/plain" if !is_attachment => out.bodies.push(("text/plain".to_string(), decode::charset(&raw, charset))),
        "text/html" if !is_attachment => out.bodies.push(("text/html".to_string(), decode::charset(&raw, charset))),
        _ => out.attachments.push(Attachment {
            filename: filename.unwrap_or_else(|| default_name(&ct.media_type, content_id.as_deref())),
            media_type: ct.media_type.clone(),
            bytes: raw,
            inline,
            content_id,
        }),
    }
}

fn decode_body(body: &[u8], encoding: &str) -> Vec<u8> {
    match encoding {
        "base64" => decode::base64(body),
        "quoted-printable" => decode::quoted_printable(body),
        _ => body.to_vec(),
    }
}

fn default_name(media_type: &str, content_id: Option<&str>) -> String {
    let ext = media_type.rsplit('/').next().unwrap_or("bin").trim();
    match content_id {
        Some(id) if !id.is_empty() => format!("inline-{id}.{ext}"),
        _ => format!("attachment.{ext}"),
    }
}

/// Split an address list on commas outside a quoted string or an angle-addr, then decode each.
fn addresses(values: Vec<&str>) -> Vec<String> {
    let mut out = Vec::new();
    for value in values {
        let mut start = 0usize;
        let mut quoted = false;
        let mut angle = 0i32;
        for (i, c) in value.char_indices() {
            match c {
                '"' => quoted = !quoted,
                '<' if !quoted => angle = angle.saturating_add(1),
                '>' if !quoted => angle = angle.saturating_sub(1),
                ',' if !quoted && angle <= 0 => {
                    push_address(&mut out, value.get(start..i).unwrap_or(""));
                    start = i.saturating_add(1);
                }
                _ => {}
            }
        }
        push_address(&mut out, value.get(start..).unwrap_or(""));
    }
    out
}

fn push_address(out: &mut Vec<String>, value: &str) {
    let text = header::decode_words(value).trim().to_string();
    if !text.is_empty() {
        out.push(text);
    }
}

/// HTML turned into readable text: block tags become line breaks, other tags vanish, the common
/// entities are decoded, and runs of blank lines collapse. Scripts and styles are dropped whole.
fn html_to_text(html: &str) -> String {
    let html = html.get(..html.len().min(MAX_HTML)).unwrap_or(html);
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut i = 0usize;
    while i < html.len() {
        let c = match html.get(i..).and_then(|s| s.chars().next()) {
            Some(c) => c,
            None => break,
        };
        if c == '<' {
            let end = match lower.get(i..).and_then(|s| s.find('>')) {
                Some(p) => i.saturating_add(p),
                None => {
                    i = html.len();
                    continue;
                }
            };
            let tag = lower.get(i..end).unwrap_or("");
            let name = tag.trim_start_matches('<').trim_start_matches('/').split(|ch: char| !ch.is_ascii_alphanumeric()).next().unwrap_or("");
            if name == "script" || name == "style" {
                let close = format!("</{name}");
                i = lower.get(end..).and_then(|s| s.find(&close)).map(|p| end.saturating_add(p)).unwrap_or(html.len());
                continue;
            }
            let block = matches!(
                name,
                "br" | "p"
                    | "div"
                    | "tr"
                    | "li"
                    | "hr"
                    | "pre"
                    | "blockquote"
                    | "table"
                    | "section"
                    | "article"
                    | "header"
                    | "footer"
                    | "ul"
                    | "ol"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
            );
            out.push(if block { '\n' } else { ' ' });
            i = end.saturating_add(1);
            continue;
        }
        if c == '&'
            && let Some((ch, used)) = entity(html.get(i..).unwrap_or(""))
        {
            out.push(ch);
            i = i.saturating_add(used);
            continue;
        }
        out.push(c);
        i = i.saturating_add(c.len_utf8());
    }
    collapse(&out)
}

/// Decode one HTML entity at the start of `s`.
fn entity(s: &str) -> Option<(char, usize)> {
    let rest = s.strip_prefix('&')?;
    let semi = rest.get(..rest.len().min(12))?.find(';')?;
    let name = rest.get(..semi)?;
    let ch = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" | "#39" => '\'',
        "nbsp" => '\u{a0}',
        "mdash" => '\u{2014}',
        "ndash" => '\u{2013}',
        "hellip" => '\u{2026}',
        "lsquo" => '\u{2018}',
        "rsquo" => '\u{2019}',
        "ldquo" => '\u{201c}',
        "rdquo" => '\u{201d}',
        other => {
            let digits = other.strip_prefix('#')?;
            let code = match digits.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => digits.parse::<u32>().ok()?,
            };
            char::from_u32(code)?
        }
    };
    Some((ch, semi.saturating_add(2)))
}

/// Line endings to `\n`, blank-line runs to one, and runs of spaces or tabs to one.
fn collapse(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(normalized.len());
    let mut blanks = 0usize;
    for line in normalized.split('\n') {
        let mut cleaned = String::with_capacity(line.len());
        let mut space = false;
        for c in line.trim_end().chars() {
            if c == ' ' || c == '\t' {
                if !space {
                    cleaned.push(' ');
                    space = true;
                }
            } else {
                cleaned.push(c);
                space = false;
            }
        }
        let cleaned = cleaned.trim();
        if cleaned.is_empty() {
            blanks = blanks.saturating_add(1);
            if blanks > 1 {
                continue;
            }
        } else {
            blanks = 0;
        }
        out.push_str(cleaned);
        out.push('\n');
    }
    out.trim().to_string()
}
