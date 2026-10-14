//! pdfcraft-eml — reading an email (`.eml`, RFC 822 / MIME) for Create (L0). See the README.
//!
//! Everything here is clean-room from the public RFCs: RFC 5322 (message format), RFC 2045 and
//! RFC 2046 (MIME parts and transport encodings), RFC 2047 (encoded words). No dependency beyond
//! the standard library: the crate is standalone and publishable, and a `.eml` from a stranger is
//! hostile input, so every reader returns a value instead of panicking.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod compose;
mod decode;
mod header;
mod mime;
mod model;
mod receipt;
#[cfg(test)]
mod tests;

pub use compose::Message;
pub use header::{Headers, encode_words};
pub use model::{Attachment, AttachmentMeta, Body, Email, EmailDocument};
pub use receipt::{Receipt, ReceiptEntry, sha256_hex};
