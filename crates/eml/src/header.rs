//! The header block of an RFC 5322 message: unfolding, case-insensitive lookup and RFC 2047
//! encoded-words.

use crate::decode;

/// The most a header block may be before it is treated as truncated. Real headers are a few KiB;
/// the cap keeps a hostile file from sizing our allocation.
pub const MAX_HEAD_BYTES: usize = 1 << 20;

/// An ordered header block. Names are compared case-insensitively; values are unfolded (RFC 5322
/// §2.2.3) but otherwise kept as written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers {
    entries: Vec<(String, String)>,
}

impl Headers {
    /// Parse a header block (the bytes before the first empty line). A line starting with a space
    /// or tab continues the field before it; malformed lines are dropped, not guessed at.
    pub fn parse(head: &[u8]) -> Headers {
        let head = head.get(..head.len().min(MAX_HEAD_BYTES)).unwrap_or(head);
        let text = String::from_utf8_lossy(head);
        let mut entries: Vec<(String, String)> = Vec::new();
        for raw in text.split('\n') {
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            if line.is_empty() {
                continue;
            }
            if line.starts_with(' ') || line.starts_with('\t') {
                if let Some(last) = entries.last_mut() {
                    let cont = line.trim();
                    if !cont.is_empty() {
                        last.1.push(' ');
                        last.1.push_str(cont);
                    }
                }
                continue;
            }
            if let Some(idx) = line.find(':') {
                let name = line.get(..idx).unwrap_or("").trim();
                let value = line.get(idx.saturating_add(1)..).unwrap_or("").trim();
                if !name.is_empty() {
                    entries.push((name.to_string(), value.to_string()));
                }
            }
        }
        Headers { entries }
    }

    /// The first value of `name` (case-insensitive), if any.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// Every value of `name`, in order.
    pub fn get_all(&self, name: &str) -> Vec<&str> {
        self.entries.iter().filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str()).collect()
    }

    /// Every field, in order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Encode a header value for transport: as written when it is plain printable ASCII, otherwise as
/// an RFC 2047 base64 word.
pub fn encode_words(value: &str) -> String {
    if value.is_ascii() && !value.chars().any(char::is_control) {
        return value.to_string();
    }
    format!("=?utf-8?B?{}?=", crate::decode::base64_encode(value.as_bytes()))
}

/// Decode RFC 2047 `=?charset?B|Q?text?=` words in an unstructured field (a subject, a display
/// name, a filename). Text outside a word is kept; a malformed or unterminated word is left as
/// written rather than dropped.
pub fn decode_words(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes.get(i) == Some(&b'=')
            && bytes.get(i.saturating_add(1)) == Some(&b'?')
            && let Some((text, used)) = decode_word(input.get(i..).unwrap_or(""))
        {
            out.push_str(&text);
            i = i.saturating_add(used);
            continue;
        }
        match input.get(i..).and_then(|s| s.chars().next()) {
            Some(c) => {
                out.push(c);
                i = i.saturating_add(c.len_utf8());
            }
            None => break,
        }
    }
    out
}

/// Decode one encoded word at the start of `s`, returning the text and the bytes it used.
fn decode_word(s: &str) -> Option<(String, usize)> {
    let rest = s.strip_prefix("=?")?;
    let q1 = rest.find('?')?;
    // A charset may carry a language tag (`utf-8*en`, RFC 2231); only the charset decides.
    let charset = rest.get(..q1)?.split('*').next().unwrap_or("");
    let rest = rest.get(q1.checked_add(1)?..)?;
    let q2 = rest.find('?')?;
    let encoding = rest.get(..q2)?;
    let rest = rest.get(q2.checked_add(1)?..)?;
    let q3 = rest.find("?=")?;
    let encoded = rest.get(..q3)?;
    if charset.is_empty() || encoded.contains('?') {
        return None;
    }
    let data = match encoding.to_ascii_uppercase().as_str() {
        "B" => decode::base64(encoded.as_bytes()),
        "Q" => decode::quoted_printable(encoded.replace('_', " ").as_bytes()),
        _ => return None,
    };
    let used = 2usize.saturating_add(q1).saturating_add(1).saturating_add(q2).saturating_add(1).saturating_add(q3).saturating_add(2);
    Some((decode::charset(&data, charset), used))
}
