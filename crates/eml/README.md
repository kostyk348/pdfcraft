# pdfcraft-eml

Reading an email (`.eml`, RFC 822 / MIME) for Create, and writing hash-chained receipts.

- **Layer:** L0 (standalone, publishable).
- **Status:** the email reader and the receipt writer. Create turns an `EmailDocument` into a PDF;
  the `eml` module itself knows nothing about PDFs.

## What it reads

Clean-room from the public RFCs only: RFC 5322 (message format), RFC 2045 and RFC 2046 (MIME parts,
base64 and quoted-printable), RFC 2047 (encoded words in a subject or a filename), RFC 2231
(a charset with a language tag). No dependency beyond the standard library.

- Header unfolding and case-insensitive lookup; `Content-Type` and `Content-Disposition` with
  quoted parameters.
- A multipart tree walked to a bounded depth, choosing one body (HTML if offered, else plain text)
  and collecting the rest as attachments.
- Bodies decoded from base64 or quoted-printable and from UTF-8, US-ASCII, ISO-8859-1 or
  Windows-1252; an unknown charset is read as UTF-8 and repaired, never refused.
- HTML bodies reduced to text: block tags become line breaks, scripts and styles are dropped, the
  common entities are decoded, and blank-line runs collapse.

## API

```rust
let email = pdfcraft_eml::Email::parse(&bytes);        // never fails
let doc: pdfcraft_eml::EmailDocument = email.to_document();
let text = doc.body_text();                            // the body as plain text
let lines = doc.attachment_lines();                    // one line per attachment
```

`Email::is_likely(name, bytes)` is the cheap detector Create uses: a `.eml` name, or a first line
that only a message starts with (`Received:`, `Return-Path:`, `MIME-Version:`, `Delivered-To:`).

## Receipts

A receipt records the SHA-256 of every file an operation read and wrote, as an `.eml`:

```rust
let mut receipt = pdfcraft_eml::Receipt::new("combine", "2026-10-13T00:00:00Z");
receipt.input("in/a.pdf", &a).output("out/combined.pdf", &out);
let bytes = receipt.render();                          // an RFC 822 document
let checked = pdfcraft_eml::Receipt::parse(&bytes).verify(|path| read(path))?;
```

`sha256_hex` is a plain FIPS 180-4 SHA-256, so a receipt needs no dependency.

## Limits and honesty

- A message is bounded: a header block of 1 MiB, 20 levels of multipart nesting, 1,000 parts, and
  1 MiB of HTML turned into text. Past a bound the reader keeps what it has and stops.
- Attachments are decoded to memory; the caller decides whether to embed them.
- `body_text()` writes an HTML body as text, with its markup removed. It is not a renderer: Create
  lays an HTML body out itself (`create.from-html`, in `crates/create`), so the headings, lists,
  quotes, tables and rules the sender wrote survive into the PDF. This crate still knows nothing
  about PDFs.
- A boundary line is matched exactly (trailing whitespace aside); a boundary that never appears
  yields the parts found so far rather than an error.
