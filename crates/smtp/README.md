# pdfcraft-smtp

Delivering a message over SMTP (RFC 5321).

- **Layer:** L0 (standalone, publishable). No dependency beyond the standard library.
- **Status:** the client. It composes nothing (`pdfcraft-eml` does that) and it is never started on
  its own: `pdfcraft-automation`'s `doc_email_send` is the only tool in the table that calls it, and
  only when it is given a server and a recipient.

## Why it is a separate crate

Sending is the one step in the email path that leaves the machine, so it lives apart from everything
that does not. `pdfcraft-eml` reads a message and writes a receipt; `Session::email_message` renders
a document into `.eml` bytes in memory; `doc_email` writes those bytes to a file. None of them opens
a socket. This crate is where a socket is opened, and only for a caller that named a relay itself.

## API

```rust
let server = pdfcraft_smtp::Server {
    host: "relay.example.com".into(),
    port: 587,
    security: pdfcraft_smtp::Security::Plain,
    username: None,
    password: None,
    timeout: Duration::from_secs(30),
    client_name: "pdfcraft.local".into(),
    crlf: false,
};
let report = pdfcraft_smtp::send(&server, "pdf@localhost", &["ada@example.com".to_string()], &message)?;
report.accepted;  // the recipients the relay took the message for
report.bytes;     // the message bytes written after DATA
report.reply;     // the relay's answer to the end of the data
report.log;       // EHLO/MAIL/RCPT → code, in order, with credentials redacted
```

The session is the ordinary one: connect, greeting, `EHLO` (falling back to `HELO` for a relay that
answers `500`, `502` or `504`), `AUTH PLAIN` when credentials were given, `MAIL FROM`, one
`RCPT TO` per recipient, `DATA`, the message, `QUIT`. A relay that takes no recipient at all is
handed no message: the client resets and reports the refusal instead.

**All input is hostile.** An address a relay could not read, a message that carries a stray carriage
return, a port that is not a port and a relay that answers with nonsense all come back as a returned
`Error`; nothing in the crate panics, unwraps or expects, and the session is bounded in replies,
reply size and commands, so a relay that keeps talking cannot run it in circles.

## Byte for byte, and CRLF

By default the message goes out **exactly as it was composed**: no line-ending rewriting, and the
only edit is RFC 5321's dot-stuffing, which never fires for a message PdfCraft composes (its parts
are base64 and its header lines start with a field name) — `Report::stuffed_lines` says how many
lines it did prepend a dot to, normally none. That is what makes a hash-chained receipt worth
anything: the bytes a receiver keeps hash the same as the `.eml` `doc_email` writes.

A strict relay may reject a bare LF (`smtpd_forbid_bare_newline` in modern Postfix). `Server::crlf`
sends the RFC form instead — every line ending CRLF, dot-stuffing over the converted bytes — and
then the receiver's copy is that message, whose final CRLF belongs to the transfer's `CRLF.CRLF`
marker as RFC 5321 defines it, so it is one line shorter than what was written.

## What it cannot do, and why

- **TLS.** `Security::StartTls` and `Security::ImplicitTls` are named and refused — before anything
  is connected, with a message saying so — because the workspace's TLS stack (`rustls`,
  `rustls-native-certs`) lives in the app tier (`apps/pdfcraft`) and is not a dependency of this
  crate. A later slice that wires it in has to decide certificate verification first: until it does,
  silently sending credentials in the clear would be worse than refusing.
- **AUTH beyond `PLAIN`.** `LOGIN` and `CRAM-MD5`/`SCRAM` are not implemented; a relay that does not
  offer `AUTH PLAIN` is refused rather than sent to unauthenticated.
- **No delivery status.** A `250` after the data means the relay took responsibility for the
  message, not that it was delivered.

## Tests

`cargo test -p pdfcraft-smtp` runs 12 unit tests (framing, addresses, the `AUTH PLAIN` token,
security names, reply parsing) and 8 that speak the protocol over a real loopback socket to a
receiver in `tests/session.rs`: a byte-for-byte delivery, CRLF mode, a relay that refuses every
recipient, `AUTH PLAIN`, the `HELO` fallback, an unreachable relay, TLS refused with a live listener
proving nothing connected, and the receiver helper itself against a by-hand client. No test opens a
port outside the loopback interface and none talks to the internet.
