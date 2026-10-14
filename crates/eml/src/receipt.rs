//! A hash-chained receipt for a run that touched files: an `.eml` holding the SHA-256 of every
//! input and output. It is the machine-checkable counterpart to a batch or a conversion, and it
//! verifies without any state beyond the files themselves.

use crate::header::Headers;
use crate::mime;

/// One file in a receipt: the path as given, its SHA-256 and its size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptEntry {
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

/// A receipt for one operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub op: String,
    pub timestamp: String,
    pub inputs: Vec<ReceiptEntry>,
    pub outputs: Vec<ReceiptEntry>,
}

impl Receipt {
    /// A receipt for `op` at `timestamp` (RFC 3339), with no entries yet.
    pub fn new(op: &str, timestamp: &str) -> Receipt {
        Receipt { op: op.to_string(), timestamp: timestamp.to_string(), inputs: Vec::new(), outputs: Vec::new() }
    }

    /// Record an input file and its bytes.
    pub fn input(&mut self, path: &str, bytes: &[u8]) -> &mut Self {
        self.inputs.push(entry(path, bytes));
        self
    }

    /// Record an output file and its bytes.
    pub fn output(&mut self, path: &str, bytes: &[u8]) -> &mut Self {
        self.outputs.push(entry(path, bytes));
        self
    }

    /// Render the receipt as an RFC 822 document.
    pub fn render(&self) -> Vec<u8> {
        let mut out = String::new();
        out.push_str("From: <pdfcraft@localhost>\n");
        out.push_str(&format!("X-Receipt-Op: {}\n", self.op));
        out.push_str(&format!("X-Receipt-Ts: {}\n", self.timestamp));
        for e in &self.inputs {
            out.push_str(&format!("X-Receipt-In: {}|{}|{}\n", escape(&e.path), e.sha256, e.size));
        }
        for e in &self.outputs {
            out.push_str(&format!("X-Receipt-Out: {}|{}|{}\n", escape(&e.path), e.sha256, e.size));
        }
        out.push_str("Content-Type: text/plain; charset=utf-8\n\n");
        out.push_str(&format!("{} — {} input(s), {} output(s)\n", self.op, self.inputs.len(), self.outputs.len()));
        for e in self.inputs.iter().chain(&self.outputs) {
            out.push_str(&format!("{} {} {}\n", e.sha256, e.size, e.path));
        }
        out.into_bytes()
    }

    /// Parse a receipt back.
    pub fn parse(bytes: &[u8]) -> Receipt {
        let (head, _) = mime::split(bytes);
        let headers = Headers::parse(head);
        let mut receipt = Receipt::new(headers.get("X-Receipt-Op").unwrap_or(""), headers.get("X-Receipt-Ts").unwrap_or(""));
        for value in headers.get_all("X-Receipt-In") {
            if let Some(e) = entry_from(value) {
                receipt.inputs.push(e);
            }
        }
        for value in headers.get_all("X-Receipt-Out") {
            if let Some(e) = entry_from(value) {
                receipt.outputs.push(e);
            }
        }
        receipt
    }

    /// Check every entry against the files `read` returns. Returns how many were checked, or the
    /// first mismatch.
    pub fn verify<F>(&self, read: F) -> Result<usize, String>
    where
        F: Fn(&str) -> Option<Vec<u8>>,
    {
        let mut checked = 0usize;
        for (label, entries) in [("input", &self.inputs), ("output", &self.outputs)] {
            for e in entries {
                let bytes = read(&e.path).ok_or_else(|| format!("{label} {} is missing", e.path))?;
                let digest = sha256_hex(&bytes);
                if digest != e.sha256 {
                    return Err(format!("{label} {} changed: {digest} != {}", e.path, e.sha256));
                }
                if bytes.len() as u64 != e.size {
                    return Err(format!("{label} {} size {} != {}", e.path, bytes.len(), e.size));
                }
                checked = checked.saturating_add(1);
            }
        }
        Ok(checked)
    }
}

fn entry(path: &str, bytes: &[u8]) -> ReceiptEntry {
    ReceiptEntry { path: path.to_string(), sha256: sha256_hex(bytes), size: bytes.len() as u64 }
}

fn entry_from(value: &str) -> Option<ReceiptEntry> {
    let fields = split_fields(value);
    Some(ReceiptEntry { path: unescape(fields.first()?), sha256: fields.get(1)?.clone(), size: fields.get(2)?.trim().parse::<u64>().ok()? })
}

/// Split on `|`, but not on an escaped one (`\|`), so a path holding a `|` survives a round trip.
fn split_fields(value: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for c in value.chars() {
        if escaped {
            current.push('\\');
            current.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '|' => fields.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    if escaped {
        current.push('\\');
    }
    fields.push(current);
    fields
}

/// Escape the separators a path could contain.
fn escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '|' => out.push_str("\\|"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some('|') => out.push('|'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

// ── SHA-256 (FIPS 180-4), so a receipt needs no dependency ──────────────────────────────────

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
    0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
    0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
    0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const H0: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];

/// The SHA-256 of `data` as a lowercase hex string.
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = sha256(data);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut message = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    let mut h = H0;
    for chunk in message.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, word) in chunk.as_chunks::<4>().0.iter().enumerate().take(16) {
            if let Some(slot) = w.get_mut(i) {
                *slot = u32::from_be_bytes(*word);
            }
        }
        // Indices here are fixed constants below 64.
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) = (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(value);
        }
    }

    let mut out = [0u8; 32];
    for (i, value) in h.iter().enumerate() {
        let bytes = value.to_be_bytes();
        let start = i.saturating_mul(4);
        if let Some(slice) = out.get_mut(start..start.saturating_add(4)) {
            slice.copy_from_slice(&bytes);
        }
    }
    out
}
