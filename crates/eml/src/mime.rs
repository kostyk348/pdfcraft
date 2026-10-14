//! Content types, dispositions and multipart bodies (RFC 2045 §5, RFC 2046 §5).

use crate::header::decode_words;

/// A parsed `Content-Type` or `Content-Disposition`: a token and its parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentType {
    pub media_type: String,
    pub params: Vec<(String, String)>,
}

impl ContentType {
    /// A parameter by name (case-insensitive).
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// Whether this is `multipart/*`.
    pub fn is_multipart(&self) -> bool {
        self.media_type.starts_with("multipart/")
    }
}

/// Parse a `Content-Type` value. An empty value is `text/plain` (RFC 2045 §5.2).
pub fn content_type(value: &str) -> ContentType {
    parse(value, "text/plain")
}

/// Parse a `Content-Disposition` value. An empty value has an empty kind.
pub fn disposition(value: &str) -> ContentType {
    parse(value, "")
}

fn parse(value: &str, default_kind: &str) -> ContentType {
    let mut fields = split_semicolons(value).into_iter();
    let kind = fields.next().unwrap_or("").trim().to_ascii_lowercase();
    let media_type = if kind.is_empty() { default_kind.to_string() } else { kind };
    let mut params = Vec::new();
    for field in fields {
        if let Some(eq) = field.find('=') {
            let name = field.get(..eq).unwrap_or("").trim().to_ascii_lowercase();
            let raw = field.get(eq.saturating_add(1)..).unwrap_or("").trim();
            let unquoted = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(raw);
            if !name.is_empty() {
                params.push((name, decode_words(unquoted)));
            }
        }
    }
    ContentType { media_type, params }
}

/// Split on `;` outside a quoted string.
fn split_semicolons(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut quoted = false;
    for (i, c) in value.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ';' if !quoted => {
                parts.push(value.get(start..i).unwrap_or(""));
                start = i.saturating_add(1);
            }
            _ => {}
        }
    }
    parts.push(value.get(start..).unwrap_or(""));
    parts
}

/// Split a part into its header block and body at the first empty line (CRLF or LF).
pub fn split(data: &[u8]) -> (&[u8], &[u8]) {
    let mut line_start = 0usize;
    for (i, &b) in data.iter().enumerate() {
        if b == b'\n' {
            let line = data.get(line_start..i).unwrap_or_default();
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() {
                let mut head = data.get(..line_start).unwrap_or_default();
                while head.ends_with(b"\n") || head.ends_with(b"\r") {
                    head = head.get(..head.len().saturating_sub(1)).unwrap_or_default();
                }
                return (head, data.get(i.saturating_add(1)..).unwrap_or_default());
            }
            line_start = i.saturating_add(1);
        }
    }
    (data, data.get(data.len()..).unwrap_or_default())
}

/// Split a `multipart/*` body on `--boundary` lines. The preamble and epilogue are dropped; a
/// missing closing boundary still yields the parts found.
pub fn split_multipart(body: &[u8], boundary: &str) -> Vec<Vec<u8>> {
    let open = format!("--{boundary}");
    let close = format!("--{boundary}--");
    let mut parts: Vec<Vec<u8>> = Vec::new();
    let mut start: Option<usize> = None;
    let mut closed = false;
    let mut line_start = 0usize;
    let mut i = 0usize;
    while i <= body.len() {
        let at_end = i == body.len();
        if at_end || body.get(i) == Some(&b'\n') {
            let mut line = body.get(line_start..i).unwrap_or_default();
            line = line.strip_suffix(b"\r").unwrap_or(line);
            // A boundary line may carry trailing whitespace (RFC 2046 §5.1.1).
            let text = String::from_utf8_lossy(line).trim_end().to_string();
            if text == open || text == close {
                if let Some(s) = start {
                    parts.push(trim_part(body, s, line_start));
                }
                if text == close {
                    closed = true;
                    break;
                }
                start = Some(i.saturating_add(1).min(body.len()));
            }
            line_start = i.saturating_add(1);
        }
        i = i.saturating_add(1);
    }
    if let Some(s) = start
        && !closed
    {
        parts.push(trim_part(body, s, body.len()));
    }
    parts
}

/// The bytes of one part, without the line break that belongs to the boundary after it.
fn trim_part(body: &[u8], start: usize, end: usize) -> Vec<u8> {
    let mut e = end.min(body.len()).max(start);
    while e > start && matches!(body.get(e.saturating_sub(1)), Some(b'\n') | Some(b'\r')) {
        e = e.saturating_sub(1);
    }
    body.get(start..e).unwrap_or_default().to_vec()
}
