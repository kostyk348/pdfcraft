//! Composing an email: an RFC 5322 / MIME message with a plain-text body, an HTML body that may
//! embed its images inline (`cid:`), and file attachments — so a PDF can be written as an `.eml`
//! and handed to a mail client or a sender. Composition is pure: it writes no file, opens no
//! socket and fetches nothing.
//!
//! The structure is the standard one for "HTML with inline images plus an attachment":
//!
//! ```text
//! multipart/mixed
//!   multipart/alternative
//!     text/plain
//!     multipart/related
//!       text/html            (references cid:…)
//!       image/png            (Content-ID, Content-Disposition: inline)
//!   application/pdf           (attachment)
//! ```

use crate::decode::base64_wrapped;
use crate::header::encode_words;
use crate::model::Attachment;
use crate::receipt::sha256_hex;

/// An email to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub date: Option<String>,
    /// The plain-text body.
    pub body: String,
    /// The HTML body, when the message has one. It may reference inline parts as `cid:<id>`.
    pub html: Option<String>,
    /// Parts embedded in the HTML body (`Content-Disposition: inline`, a `Content-ID`).
    pub inline: Vec<Attachment>,
    /// Parts attached to the message.
    pub attachments: Vec<Attachment>,
}

impl Message {
    pub fn new(from: &str, to: &str, subject: &str, body: &str) -> Message {
        Message {
            from: from.to_string(),
            to: if to.is_empty() { Vec::new() } else { vec![to.to_string()] },
            cc: Vec::new(),
            subject: subject.to_string(),
            date: None,
            body: body.to_string(),
            html: None,
            inline: Vec::new(),
            attachments: Vec::new(),
        }
    }

    pub fn to(&mut self, address: &str) -> &mut Self {
        if !address.is_empty() {
            self.to.push(address.to_string());
        }
        self
    }

    pub fn cc(&mut self, address: &str) -> &mut Self {
        if !address.is_empty() {
            self.cc.push(address.to_string());
        }
        self
    }

    /// Set the `Date` header (RFC 5322 date, as the caller formats it).
    pub fn date(&mut self, date: &str) -> &mut Self {
        self.date = Some(date.to_string());
        self
    }

    /// Give the message an HTML body, shown instead of the plain text by clients that render HTML.
    pub fn html(&mut self, html: &str) -> &mut Self {
        self.html = Some(html.to_string());
        self
    }

    /// Embed a part in the HTML body. `cid` is what the HTML references (`<img src="cid:page-1">`
    /// for `cid = "page-1"`).
    pub fn embed(&mut self, filename: &str, media_type: &str, cid: &str, bytes: Vec<u8>) -> &mut Self {
        self.inline.push(Attachment {
            filename: filename.to_string(),
            media_type: media_type.to_string(),
            bytes,
            inline: true,
            content_id: Some(cid.to_string()),
        });
        self
    }

    /// Attach a file.
    pub fn attach(&mut self, filename: &str, media_type: &str, bytes: Vec<u8>) -> &mut Self {
        self.attachments.push(Attachment {
            filename: filename.to_string(),
            media_type: media_type.to_string(),
            bytes,
            inline: false,
            content_id: None,
        });
        self
    }

    /// The MIME boundaries. Derived from the message, so the same message renders the same bytes
    /// (a receipt can hash it) and they change whenever any part does.
    fn boundaries(&self) -> (String, String, String) {
        let mut seed = String::from(&self.subject);
        seed.push_str(&self.body);
        if let Some(html) = &self.html {
            seed.push_str(html);
        }
        for part in self.inline.iter().chain(&self.attachments) {
            seed.push_str(&part.filename);
            seed.push_str(&part.media_type);
            seed.push_str(&part.bytes.len().to_string());
        }
        let digest = sha256_hex(seed.as_bytes());
        let outer = format!("=_pdfcraft_{}=", digest.get(..24).unwrap_or(digest.as_str()));
        (format!("{outer}.alt"), format!("{outer}.rel"), outer)
    }

    /// Render the message as an RFC 822 / MIME document.
    pub fn render(&self) -> Vec<u8> {
        let (alt, rel, outer) = self.boundaries();
        let mut out = String::new();
        out.push_str(&format!("From: {}\n", self.from));
        for address in &self.to {
            out.push_str(&format!("To: {address}\n"));
        }
        for address in &self.cc {
            out.push_str(&format!("Cc: {address}\n"));
        }
        out.push_str(&format!("Subject: {}\n", encode_words(&self.subject)));
        if let Some(date) = &self.date {
            out.push_str(&format!("Date: {date}\n"));
        }
        out.push_str("MIME-Version: 1.0\n");
        out.push_str(&format!("Content-Type: multipart/mixed; boundary=\"{outer}\"\n\n"));

        match (&self.html, self.inline.is_empty()) {
            (Some(html), true) => {
                out.push_str(&format!("--{outer}\nContent-Type: multipart/alternative; boundary=\"{alt}\"\n\n"));
                self.text_part(&mut out, &alt);
                leaf(&mut out, &alt, "text/html; charset=utf-8", None, html.as_bytes());
                out.push_str(&format!("--{alt}--\n"));
            }
            (Some(html), false) => {
                out.push_str(&format!("--{outer}\nContent-Type: multipart/alternative; boundary=\"{alt}\"\n\n"));
                self.text_part(&mut out, &alt);
                out.push_str(&format!("--{alt}\nContent-Type: multipart/related; boundary=\"{rel}\"\n\n"));
                leaf(&mut out, &rel, "text/html; charset=utf-8", None, html.as_bytes());
                for part in &self.inline {
                    inline_part(&mut out, &rel, part);
                }
                out.push_str(&format!("--{rel}--\n--{alt}--\n"));
            }
            (None, _) => {
                leaf(&mut out, &outer, "text/plain; charset=utf-8", None, self.body.as_bytes());
            }
        }

        for part in &self.attachments {
            let name = encode_words(&part.filename);
            out.push_str(&format!("--{outer}\n"));
            out.push_str(&format!("Content-Type: {}; name=\"{name}\"\n", part.media_type));
            out.push_str(&format!("Content-Disposition: attachment; filename=\"{name}\"\n"));
            out.push_str("Content-Transfer-Encoding: base64\n\n");
            out.push_str(&base64_wrapped(&part.bytes));
        }
        out.push_str(&format!("--{outer}--\n"));
        out.into_bytes()
    }

    fn text_part(&self, out: &mut String, boundary: &str) {
        leaf(out, boundary, "text/plain; charset=utf-8", None, self.body.as_bytes());
    }
}

/// Write one base64 leaf part under `boundary`.
fn leaf(out: &mut String, boundary: &str, content_type: &str, extra_headers: Option<&str>, bytes: &[u8]) {
    out.push_str(&format!("--{boundary}\nContent-Type: {content_type}\n"));
    if let Some(extra) = extra_headers {
        out.push_str(extra);
    }
    out.push_str("Content-Transfer-Encoding: base64\n\n");
    out.push_str(&base64_wrapped(bytes));
}

/// Write an inline part: `Content-ID` plus `Content-Disposition: inline`, so the HTML can point at it.
fn inline_part(out: &mut String, boundary: &str, part: &Attachment) {
    let name = encode_words(&part.filename);
    let cid = part.content_id.as_deref().unwrap_or("part");
    out.push_str(&format!("--{boundary}\n"));
    out.push_str(&format!("Content-Type: {}; name=\"{name}\"\n", part.media_type));
    out.push_str(&format!("Content-ID: <{cid}>\n"));
    out.push_str(&format!("Content-Disposition: inline; filename=\"{name}\"\n"));
    out.push_str("Content-Transfer-Encoding: base64\n\n");
    out.push_str(&base64_wrapped(&part.bytes));
}
