//! Transport encodings and character sets: base64, quoted-printable (RFC 2045 §6.7) and the few
//! charsets a mail body realistically uses.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Decode base64, ignoring anything that is not an alphabet character (whitespace, `=` padding).
pub fn base64(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &b in input {
        let v = match b {
            b'A'..=b'Z' => b.wrapping_sub(b'A'),
            b'a'..=b'z' => b.wrapping_sub(b'a').saturating_add(26),
            b'0'..=b'9' => b.wrapping_sub(b'0').saturating_add(52),
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        acc = (acc << 6) | u32::from(v);
        bits = bits.saturating_add(6);
        if bits >= 8 {
            bits = bits.saturating_sub(8);
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    out
}

/// Encode base64 (standard alphabet, `=` padding).
pub fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3).saturating_mul(4));
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk.first().copied().unwrap_or(0));
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        for shift in [18u32, 12, 6, 0] {
            let index = ((n >> shift) & 0x3f) as usize;
            out.push(char::from(*ALPHABET.get(index).unwrap_or(&b'A')));
        }
        match chunk.len() {
            1 => {
                out.pop();
                out.pop();
                out.push_str("==");
            }
            2 => {
                out.pop();
                out.push('=');
            }
            _ => {}
        }
    }
    out
}

/// Base64 in 76-character lines, as a MIME body wants it.
pub fn base64_wrapped(input: &[u8]) -> String {
    let encoded = base64_encode(input);
    let mut out = String::with_capacity(encoded.len() + encoded.len() / 76 + 2);
    for (i, c) in encoded.chars().enumerate() {
        if i > 0 && i % 76 == 0 {
            out.push('\n');
        }
        out.push(c);
    }
    out.push('\n');
    out
}

/// Decode quoted-printable: `=XX` hex, and `=` before a line break is a soft break. A malformed
/// `=` is kept as written.
pub fn quoted_printable(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0usize;
    while i < input.len() {
        let b = match input.get(i) {
            Some(&b) => b,
            None => break,
        };
        if b != b'=' {
            out.push(b);
            i = i.saturating_add(1);
            continue;
        }
        match (input.get(i.saturating_add(1)), input.get(i.saturating_add(2))) {
            (Some(b'\n'), _) => i = i.saturating_add(2),
            (Some(b'\r'), Some(b'\n')) => i = i.saturating_add(3),
            (Some(h), Some(l)) => match (hex(*h), hex(*l)) {
                (Some(h), Some(l)) => {
                    out.push(h.wrapping_mul(16).wrapping_add(l));
                    i = i.saturating_add(3);
                }
                _ => {
                    out.push(b);
                    i = i.saturating_add(1);
                }
            },
            _ => {
                out.push(b);
                i = i.saturating_add(1);
            }
        }
    }
    out
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b.wrapping_sub(b'0')),
        b'a'..=b'f' => Some(b.wrapping_sub(b'a').saturating_add(10)),
        b'A'..=b'F' => Some(b.wrapping_sub(b'A').saturating_add(10)),
        _ => None,
    }
}

/// Decode `bytes` from a named charset. UTF-8 and US-ASCII are exact; the ISO-8859-1 family and
/// Windows-1252 share the one table below (they differ only in 0x80–0x9F, where ISO-8859-1 has
/// control characters no real mail body uses); anything unknown is read as UTF-8 and repaired
/// lossily, never refused.
pub fn charset(bytes: &[u8], charset: &str) -> String {
    let name = charset.trim().trim_matches('"').to_ascii_lowercase();
    match name.as_str() {
        "us-ascii" | "ascii" => bytes.iter().map(|&b| if b < 0x80 { b as char } else { '\u{fffd}' }).collect(),
        "iso-8859-1" | "iso8859-1" | "latin1" | "latin-1" | "cp1252" | "windows-1252" => bytes.iter().map(|&b| cp1252(b)).collect(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Windows-1252: the ASCII range, then the 0x80–0x9F punctuation, then Latin-1.
fn cp1252(b: u8) -> char {
    match b {
        0x00..=0x7f => b as char,
        0x80 => '\u{20ac}',
        0x82 => '\u{201a}',
        0x83 => '\u{0192}',
        0x84 => '\u{201e}',
        0x85 => '\u{2026}',
        0x86 => '\u{2020}',
        0x87 => '\u{2021}',
        0x88 => '\u{02c6}',
        0x89 => '\u{2030}',
        0x8a => '\u{0160}',
        0x8b => '\u{2039}',
        0x8c => '\u{0152}',
        0x8e => '\u{017d}',
        0x91 => '\u{2018}',
        0x92 => '\u{2019}',
        0x93 => '\u{201c}',
        0x94 => '\u{201d}',
        0x95 => '\u{2022}',
        0x96 => '\u{2013}',
        0x97 => '\u{2014}',
        0x98 => '\u{02dc}',
        0x99 => '\u{2122}',
        0x9a => '\u{0161}',
        0x9b => '\u{203a}',
        0x9c => '\u{0153}',
        0x9e => '\u{017e}',
        0x9f => '\u{0178}',
        0xa0 => '\u{a0}',
        0xa1..=0xff => b as char,
        // 0x81, 0x8d, 0x8f, 0x90, 0x9d are undefined in Windows-1252.
        _ => '\u{fffd}',
    }
}
