//! pdfcraft-smtp — delivering a message over SMTP (RFC 5321).
//!
//! A client, not a server: it connects, says hello, encrypts the session when the caller asked for
//! it, authenticates when the caller gave it credentials, hands over the message bytes and quits —
//! or, for [`verify`], stops after the login so a person can find out whether the settings work
//! without sending anything. [`PRESETS`] is the table of common providers (server, port, transport),
//! so the interface can fill the dialog from an address instead of asking for the documentation.
//! It composes nothing (that is `pdfcraft-eml`) and it holds no configuration of its own — no relay,
//! no credentials, no port: [`send`] opens a socket only when a caller passes a [`Server`], which is
//! what makes sending an explicit, opt-in step rather than something a document ever does on its
//! own.
//!
//! **Layer:** L0 (standalone, publishable). No workspace dependency. Transport security is the
//! workspace's own rustls stack, trusting the operating system's roots (native targets only: a
//! browser build has no sockets).
//!
//! Clean-room from the public RFCs only: RFC 5321 (the SMTP session and the envelope),
//! RFC 4954 (the `AUTH` extension and its `PLAIN` mechanism), RFC 3207 (`STARTTLS`) and RFC 8314
//! (implicit TLS, `smtps:`). Everything is a returned value:
//! a bad address, an unreachable relay, a certificate that does not check out, a refused recipient
//! and a relay that talks nonsense all come back as [`Error`], and no path in this crate panics.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;

/// A reply is a handful of lines; these bound a relay that keeps talking so the session always ends.
const MAX_REPLY_LINES: usize = 200;
const MAX_REPLY_BYTES: usize = 64 * 1024;
/// Commands one session may send, so a relay that keeps asking for more cannot run us in circles.
const MAX_COMMANDS: usize = 64;
/// At most this many recipient addresses in one envelope (RFC 5321 allows 100 minimum; a relay
/// that wants more will refuse, which the caller sees).
const MAX_RECIPIENTS: usize = 1_000;
/// At most this many resolved addresses tried before giving up on a host.
const MAX_ADDRESSES: usize = 8;

/// How the connection is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    /// No transport security: the session runs in the clear, for a relay on this machine or one
    /// already inside a trusted network.
    Plain,
    /// `STARTTLS` (RFC 3207): the session starts in the clear, then the same connection is upgraded
    /// after `EHLO`, and everything after it — including any password — is encrypted.
    StartTls,
    /// Implicit TLS, as `smtps://` speaks (RFC 8314): the handshake comes before the greeting.
    ImplicitTls,
}

impl Security {
    /// Read a `security` argument: `none`, `starttls` or `tls`. Naming a mode this build cannot
    /// carry out (a browser build has no sockets) is not an error here — [`send`] refuses it
    /// before opening anything.
    pub fn parse(name: &str) -> Result<Security, Error> {
        match name.trim().to_ascii_lowercase().as_str() {
            "none" => Ok(Security::Plain),
            "starttls" => Ok(Security::StartTls),
            "tls" | "smtps" | "implicit" => Ok(Security::ImplicitTls),
            other => Err(Error::Config(format!("security must be \"none\", \"starttls\" or \"tls\", not {other:?}"))),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Security::Plain => "none",
            Security::StartTls => "starttls",
            Security::ImplicitTls => "tls",
        }
    }
}

/// One mail provider's submission server: the table the interface fills its fields from.
///
/// This is the only place a provider is named. The dialog lists [`PRESETS`] by [`Preset::name`],
/// fills the server, the port and the transport from the entry, and finds the entry for an address
/// with [`preset`]; adding a provider is one entry here and nothing else. No entry holds a secret:
/// a host, a port, a transport and a page to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    /// The provider's name, as the provider and its users write it (a name, so the interface does
    /// not translate it).
    pub name: &'static str,
    /// The domains that belong to it: what follows the `@`, lowercase.
    pub domains: &'static [&'static str],
    /// The submission server.
    pub host: &'static str,
    /// The port that goes with [`Preset::security`].
    pub port: u16,
    /// The transport the provider expects on that port.
    pub security: Security,
    /// A second port the same provider accepts, for a network that blocks the first.
    pub alternative: Option<(u16, Security)>,
    /// Where the user makes an application password this provider accepts instead of the
    /// account's own; `None` for one that takes the account password. The interface shows this as
    /// a hint and opens it in the system browser; it never reads the page.
    pub app_password_page: Option<&'static str>,
}

/// The providers the interface offers, in the order it lists them.
pub const PRESETS: &[Preset] = &[
    Preset {
        name: "Gmail",
        domains: &["gmail.com", "googlemail.com"],
        host: "smtp.gmail.com",
        port: 465,
        security: Security::ImplicitTls,
        alternative: Some((587, Security::StartTls)),
        app_password_page: Some("https://myaccount.google.com/apppasswords"),
    },
    Preset {
        name: "Yandex",
        domains: &["yandex.ru", "yandex.com", "yandex.by"],
        host: "smtp.yandex.ru",
        port: 465,
        security: Security::ImplicitTls,
        alternative: None,
        app_password_page: None,
    },
    Preset {
        name: "Mail.ru",
        domains: &["mail.ru", "bk.ru", "inbox.ru", "list.ru"],
        host: "smtp.mail.ru",
        port: 465,
        security: Security::ImplicitTls,
        alternative: None,
        app_password_page: None,
    },
    Preset {
        name: "Outlook.com",
        domains: &["outlook.com", "hotmail.com"],
        host: "smtp-mail.outlook.com",
        port: 587,
        security: Security::StartTls,
        alternative: None,
        app_password_page: None,
    },
    Preset {
        name: "Live.com",
        domains: &["live.com"],
        host: "smtp-mail.outlook.com",
        port: 587,
        security: Security::StartTls,
        alternative: None,
        app_password_page: None,
    },
];

/// The provider an address — or just the domain after its `@` — belongs to.
///
/// The domain is compared as it is written after `@`, ignoring case and a trailing root dot. A
/// domain no entry lists returns `None`: the caller fills in nothing rather than guessing a relay.
pub fn preset(value: &str) -> Option<&'static Preset> {
    let value = value.trim();
    let domain = value.rsplit_once('@').map_or(value, |(_, domain)| domain);
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    if domain.is_empty() {
        return None;
    }
    PRESETS.iter().find(|entry| entry.domains.iter().any(|known| *known == domain))
}

/// What to connect to and who to say we are. Every field is an explicit choice by the caller; the
/// defaults are a plain session to `localhost:25`, which a caller would have to name anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub host: String,
    pub port: u16,
    pub security: Security,
    /// The `AUTH PLAIN` user name, sent only together with a password.
    pub username: Option<String>,
    /// The `AUTH PLAIN` password. Never logged, never echoed back in a report.
    pub password: Option<String>,
    /// The budget for connecting, for each reply, and for writing. A relay that stalls is an error.
    pub timeout: Duration,
    /// What the client announces in `EHLO`. A relay takes a domain name; no credentials live here.
    pub client_name: String,
    /// Send with CRLF line endings and dot-stuffing, as RFC 5321 wants. **Off by default:** the
    /// message goes out exactly as it was composed, so the receiver's copy is the same bytes — the
    /// point of a hash-chained receipt. A strict relay that rejects bare LF (`smtpd_forbid_bare_newline`)
    /// wants this on.
    pub crlf: bool,
    /// Extra certificates the peer's chain may end at, DER-encoded — a relay whose own CA the system
    /// store does not carry. **Added to** the operating system's roots; verification is never
    /// switched off, so a certificate that does not check out is still an [`Error::Tls`]. Empty by
    /// default; tests use it to trust their own receiver.
    pub extra_roots: Vec<Vec<u8>>,
}

impl Default for Server {
    fn default() -> Self {
        Server {
            host: String::new(),
            port: 25,
            security: Security::Plain,
            username: None,
            password: None,
            timeout: Duration::from_secs(30),
            client_name: "pdfcraft.local".to_string(),
            crlf: false,
            extra_roots: Vec::new(),
        }
    }
}

/// What one [`send`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub host: String,
    pub port: u16,
    /// The recipients the relay took the message for.
    pub accepted: Vec<String>,
    /// The recipients it refused, with its reply. A message with no accepted recipient is not sent
    /// at all (that is an [`Error::Refused`]), so a report with entries here still delivered.
    pub refused: Vec<(String, String)>,
    /// The message bytes written after `DATA`, framing aside.
    pub bytes: usize,
    /// Lines that began with `.` and were doubled for the transfer (always 0 for a message
    /// composed by PdfCraft, whose parts are base64).
    pub stuffed_lines: usize,
    /// The relay's reply to the end of the message data.
    pub reply: String,
    /// The session in order, as `MAIL FROM:<a@b> → 250 ok`. Credentials are redacted.
    pub log: Vec<String>,
}

/// Everything that can go wrong. All of it is a value: a caller decides what to do, nothing panics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The request cannot be carried out as configured. **Nothing was sent, and no socket was
    /// opened**: this is reported before the connection, so a mistake here can never leak a message.
    Config(String),
    /// The relay could not be resolved or reached.
    Connect(String),
    /// The encrypted session could not be set up, above all a certificate that did not check out.
    /// Nothing was sent: the handshake comes before the first command.
    Tls(String),
    /// The relay sent something that is not an SMTP reply, or stopped in the middle.
    Protocol(String),
    /// The relay refused a command.
    Refused { command: String, code: u16, reply: String },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config(m) => write!(f, "{m}"),
            Error::Connect(m) => write!(f, "{m}"),
            Error::Tls(m) => write!(f, "{m}"),
            Error::Protocol(m) => write!(f, "{m}"),
            Error::Refused { command, code, reply } => write!(f, "the server refused {command}: {code} {reply}"),
        }
    }
}

impl std::error::Error for Error {}

/// Hand `message` to `server` for `recipients`, as `from`.
///
/// The session is: connect, (handshake, for implicit TLS), greeting, `EHLO` (falling back to
/// `HELO`), `STARTTLS` and a second `EHLO` when the caller asked for it, `AUTH PLAIN` when
/// credentials were given, `MAIL FROM`, one `RCPT TO` per recipient, `DATA`, the message, `QUIT`.
/// The message bytes are framed for the transfer and otherwise left alone (see [`Server::crlf`]).
///
/// A configuration mistake — an address a relay could not read, a mode this build does not carry
/// out, credentials without a user name — comes back as [`Error::Config`] **before** the socket is
/// opened; a certificate that does not check out is [`Error::Tls`] and stops the session before
/// the envelope, so nothing is ever handed to a relay unencrypted by mistake.
pub fn send(server: &Server, from: &str, recipients: &[String], message: &[u8]) -> Result<Report, Error> {
    let from = address("from", from)?;
    let recipients = envelope(recipients)?;
    let (payload, stuffed_lines) = frame(message, server.crlf)?;
    check(server)?;

    let (mut session, _capabilities) = Session::connect_authenticated(server)?;

    session.expect(&format!("MAIL FROM:<{from}>"), &[250])?;
    let mut accepted: Vec<String> = Vec::new();
    let mut refused: Vec<(String, String)> = Vec::new();
    let mut last_refusal: Option<(u16, String)> = None;
    for recipient in &recipients {
        let (code, text) = session.command(&format!("RCPT TO:<{recipient}>"))?;
        match code {
            250..=252 => accepted.push(recipient.clone()),
            // 4xx is a temporary refusal, 5xx a permanent one; either way this recipient is out.
            400..=599 => {
                last_refusal = Some((code, text.clone()));
                refused.push((recipient.clone(), text));
            }
            _ => return Err(Error::Refused { command: format!("RCPT TO:<{recipient}>"), code, reply: text }),
        }
    }
    if accepted.is_empty() {
        // RFC 5321 §3.3: with no recipient left there is nothing to send, so the transaction is
        // reset instead of delivering to nobody.
        session.ignore("RSET");
        session.ignore("QUIT");
        let (code, reply) = last_refusal.unwrap_or((550, "no recipient was accepted".to_string()));
        return Err(Error::Refused { command: "RCPT TO".to_string(), code, reply });
    }

    session.expect("DATA", &[354])?;
    session.write_body(&payload)?;
    let reply = session.reply()?;
    let (code, text) = (reply.code, reply.text());
    session.note(&format!("message {} bytes → {} {}", payload.len(), code, text));
    if code != 250 {
        session.ignore("QUIT");
        return Err(Error::Refused { command: "the message data".to_string(), code, reply: text });
    }
    session.ignore("QUIT");

    Ok(Report {
        host: server.host.trim().to_string(),
        port: server.port,
        accepted,
        refused,
        bytes: payload.len(),
        stuffed_lines,
        reply: text,
        log: session.log,
    })
}

/// What one [`verify`] found: the relay was reached and the login was taken, and **nothing was
/// handed over**.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub host: String,
    pub port: u16,
    /// Whether `AUTH PLAIN` was carried out (`false` when the caller gave no credentials).
    pub authenticated: bool,
    /// The extensions the relay listed on the (encrypted, when asked for) session.
    pub capabilities: Vec<String>,
    /// The session in order, as `EHLO … → 250 …`. Credentials are redacted.
    pub log: Vec<String>,
}

/// Check the connection without sending anything: connect, greet, say hello, encrypt the session
/// when the caller asked for it, log in when credentials were given, and quit.
///
/// The session is the one [`send`] opens — same greeting, same `EHLO`, same `STARTTLS` upgrade,
/// same `AUTH PLAIN` — and it stops there: `MAIL FROM`, `RCPT TO` and `DATA` are never sent, so a
/// relay has nothing to deliver and a user finds out about a wrong port, a certificate that does
/// not check out or a refused password before writing the message. Every failure is an [`Error`]
/// and nothing panics.
pub fn verify(server: &Server) -> Result<Checked, Error> {
    check(server)?;
    let authenticated = server.username.is_some();
    let (mut session, capabilities) = Session::connect_authenticated(server)?;
    session.ignore("QUIT");
    session.note("connection check: no message was handed over");
    Ok(Checked { host: server.host.trim().to_string(), port: server.port, authenticated, capabilities, log: session.log })
}

/// Check one envelope address (`from`, `MAIL FROM`) or recipient (`to`, `RCPT TO`).
///
/// Deliberately narrow: what an inbound relay needs (RFC 5321 §4.1.2), no display names and no
/// quoted local parts. Anything that could break out of the command line — a space, a control
/// character (a line ending above all), `<`, `>` or a comma — is refused here, before anything is
/// connected. Returns the address without surrounding whitespace.
pub fn address(what: &str, value: &str) -> Result<String, Error> {
    let value = value.trim();
    if value.is_empty() {
        return Err(Error::Config(format!("{what} must be an email address")));
    }
    if let Some(bad) = value.chars().find(|c| !is_address_char(*c)) {
        return Err(Error::Config(format!("{what} {value:?} has {bad:?} in it; an address is letters, digits and .!#$%&'*+/=?^_`{{|}}~-@")));
    }
    let parts: Vec<&str> = value.split('@').collect();
    match parts.as_slice() {
        [local, domain] if !local.is_empty() && !domain.is_empty() => Ok(value.to_string()),
        _ => Err(Error::Config(format!("{what} {value:?} must have exactly one @ with something on each side"))),
    }
}

/// The envelope's recipient list: checked, and one entry per address (a relay sees duplicates as
/// separate deliveries, which is rarely what a caller means).
fn envelope(recipients: &[String]) -> Result<Vec<String>, Error> {
    if recipients.is_empty() {
        return Err(Error::Config("at least one recipient is needed".to_string()));
    }
    if recipients.len() > MAX_RECIPIENTS {
        return Err(Error::Config(format!("at most {MAX_RECIPIENTS} recipients are sent in one message (got {})", recipients.len())));
    }
    let mut out: Vec<String> = Vec::new();
    for recipient in recipients {
        let checked = address("recipient", recipient)?;
        if !out.iter().any(|seen| seen.eq_ignore_ascii_case(&checked)) {
            out.push(checked);
        }
    }
    Ok(out)
}

/// Everything that must hold before a socket is worth opening.
fn check(server: &Server) -> Result<(), Error> {
    if server.host.trim().is_empty() {
        return Err(Error::Config("host must be the name of the SMTP server to send through".to_string()));
    }
    if server.port == 0 {
        return Err(Error::Config("port must be 1 to 65535".to_string()));
    }
    // A browser build has no sockets at all; on a native build both modes are carried out for real.
    #[cfg(target_arch = "wasm32")]
    match server.security {
        Security::Plain => {}
        Security::StartTls | Security::ImplicitTls => {
            return Err(Error::Config(format!(
                "security {:?} is not available in this build: a browser has no sockets, so PdfCraft cannot \
                 send mail from it at all — use the desktop app",
                server.security.name()
            )));
        }
    }
    match (&server.username, &server.password) {
        (Some(_), Some(_)) | (None, None) => {}
        (Some(_), None) => return Err(Error::Config("a username needs its password".to_string())),
        (None, Some(_)) => return Err(Error::Config("a password needs its username".to_string())),
    }
    if server.username.as_deref().is_some_and(|user| user.trim().is_empty()) {
        return Err(Error::Config("username must not be empty".to_string()));
    }
    if let Some(bad) = server.client_name.chars().find(|c| !c.is_ascii_graphic()) {
        return Err(Error::Config(format!("client_name has {bad:?} in it; it is one word a relay can read back")));
    }
    if server.timeout.is_zero() {
        return Err(Error::Config("timeout must be more than zero".to_string()));
    }
    Ok(())
}

fn is_address_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(c, '.' | '!' | '#' | '$' | '%' | '&' | '\'' | '*' | '+' | '/' | '=' | '?' | '^' | '_' | '`' | '{' | '|' | '}' | '~' | '-' | '@')
}

/// The bytes to write after `DATA`, and how many lines had a leading `.` doubled.
///
/// With `crlf` off the message is passed through byte for byte — the only edit is RFC 5321's
/// dot-stuffing, which never fires for a message PdfCraft composes (its parts are base64 and its
/// header lines start with a field name). A message that contains a carriage return is refused
/// instead, because on the wire the transfer's own end-of-data marker is `CRLF.CRLF`: a stray CRLF
/// would make the receiver stop in the middle of the message.
fn frame(message: &[u8], crlf: bool) -> Result<(Vec<u8>, usize), Error> {
    if !crlf && let Some(pos) = message.iter().position(|b| *b == b'\r') {
        return Err(Error::Config(format!(
            "the message has a carriage return at byte {pos}, which the transfer would read as its own end-of-data marker; compose it with LF line endings, or send it with crlf true"
        )));
    }
    let mut out = Vec::with_capacity(message.len() + 8);
    let mut stuffed = 0usize;
    let mut at_line_start = true;
    let mut i = 0usize;
    while let Some(byte) = message.get(i).copied() {
        match byte {
            b'\r' => {
                out.extend_from_slice(b"\r\n");
                at_line_start = true;
                if message.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
            }
            b'\n' => {
                if crlf {
                    out.extend_from_slice(b"\r\n");
                } else {
                    out.push(b'\n');
                }
                at_line_start = true;
            }
            _ => {
                if at_line_start && byte == b'.' {
                    out.push(b'.');
                    stuffed += 1;
                }
                out.push(byte);
                at_line_start = false;
            }
        }
        i += 1;
    }
    Ok((out, stuffed))
}

/// The dot-stuffed body plus the transfer's end-of-data marker: `CRLF.CRLF`.
fn terminate(payload: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(payload);
    if !payload.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b".\r\n");
}

/// One SMTP reply: a code and its lines (a relay may spread its answer over several).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reply {
    code: u16,
    lines: Vec<String>,
}

impl Reply {
    fn text(&self) -> String {
        self.lines.iter().map(|l| l.get(4..).unwrap_or("").trim()).collect::<Vec<_>>().join(" ")
    }

    /// The space-separated words of the whole reply, which is how `EHLO` lists its extensions.
    fn words(&self) -> Vec<String> {
        self.lines.iter().flat_map(|line| line.get(4..).unwrap_or("").split_whitespace().map(str::to_ascii_uppercase)).collect()
    }
}

/// The connection under a session: the socket itself, or the same socket carried by rustls.
enum Stream {
    Plain(TcpStream),
    #[cfg(not(target_arch = "wasm32"))]
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(tcp) => tcp.read(buf),
            #[cfg(not(target_arch = "wasm32"))]
            Stream::Tls(tls) => tls.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(tcp) => tcp.write(buf),
            #[cfg(not(target_arch = "wasm32"))]
            Stream::Tls(tls) => tls.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Plain(tcp) => tcp.flush(),
            #[cfg(not(target_arch = "wasm32"))]
            Stream::Tls(tls) => tls.flush(),
        }
    }
}

impl Stream {
    /// The socket underneath, for the one upgrade a session can make.
    fn into_tcp(self) -> Result<TcpStream, Error> {
        match self {
            Stream::Plain(tcp) => Ok(tcp),
            #[cfg(not(target_arch = "wasm32"))]
            Stream::Tls(_) => Err(Error::Protocol("the session is already encrypted".to_string())),
        }
    }
}

/// Put `stream` inside TLS and complete the handshake now, so a certificate that does not check out
/// is reported as itself instead of turning into a protocol error on the first command.
#[cfg(not(target_arch = "wasm32"))]
fn handshake(stream: Stream, server: &Server) -> Result<Stream, Error> {
    let mut tcp = stream.into_tcp()?;
    let config = client_config(&server.extra_roots)?;
    let host = server.host.trim();
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| Error::Config(format!("{host:?} is not a name or address a certificate can be checked against")))?;
    let mut session = rustls::ClientConnection::new(config, name)
        .map_err(|e| Error::Tls(format!("the encrypted session with {host}:{} could not be started: {e}", server.port)))?;
    while session.is_handshaking() {
        // The socket's read and write timeouts still apply, so a relay that stalls mid-handshake ends.
        session.complete_io(&mut tcp).map_err(|e| handshake_error(server, &e))?;
    }
    Ok(Stream::Tls(Box::new(rustls::StreamOwned::new(session, tcp))))
}

/// What the peer's chain may end at: the operating system's roots, plus any the caller named.
///
/// Verification is never switched off — a relay whose certificate the system store does not carry is
/// an error, not a reason to send anyway (AGENTS.md §3: safe by default).
#[cfg(not(target_arch = "wasm32"))]
fn client_config(extra_roots: &[Vec<u8>]) -> Result<Arc<rustls::ClientConfig>, Error> {
    let mut roots = rustls::RootCertStore::empty();
    // The OS store is the trust base: no root list is bundled with the app (and none may be).
    let system = rustls_native_certs::load_native_certs();
    for cert in system.certs {
        // A root the store refuses as unreadable is simply not trusted; the rest still are.
        let _ = roots.add(cert);
    }
    for der in extra_roots {
        roots
            .add(rustls::pki_types::CertificateDer::from(der.clone()))
            .map_err(|e| Error::Config(format!("a trusted certificate given to the client could not be read: {e}")))?;
    }
    if roots.is_empty() {
        return Err(Error::Config(
            "no trusted root certificates were found on this system, so an encrypted session could not be checked; \
             use security \"none\" only for a relay you trust"
                .to_string(),
        ));
    }
    // Say which provider to use rather than taking a process-wide default: the workspace also has
    // aws-lc-rs in its graph (through another crate), and picking a default would be ambiguous.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Tls(format!("the TLS settings could not be built: {e}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// A failed handshake, in words a person can act on: above all, which certificate did not check out.
#[cfg(not(target_arch = "wasm32"))]
fn handshake_error(server: &Server, error: &std::io::Error) -> Error {
    let host = server.host.trim();
    if let Some(inner) = error.get_ref()
        && let Some(rustls_error) = inner.downcast_ref::<rustls::Error>()
    {
        return Error::Tls(format!("{host}:{} did not pass certificate verification ({}); nothing was sent", server.port, describe(rustls_error)));
    }
    Error::Tls(format!("the encrypted session with {host}:{} could not be set up: {error}", server.port))
}

#[cfg(not(target_arch = "wasm32"))]
fn describe(error: &rustls::Error) -> String {
    use rustls::CertificateError as R;
    match error {
        rustls::Error::InvalidCertificate(reason) => match reason {
            R::UnknownIssuer => "its certificate is not signed by a trusted authority".to_string(),
            R::Expired => "its certificate has expired".to_string(),
            R::NotValidYet => "its certificate is not valid yet".to_string(),
            R::NotValidForName | R::NotValidForNameContext { .. } => "its certificate is for another name".to_string(),
            R::InvalidPurpose | R::InvalidPurposeContext { .. } => "its certificate is not for a mail server".to_string(),
            R::BadSignature => "its certificate's signature does not verify".to_string(),
            R::Revoked => "its certificate has been revoked".to_string(),
            other => format!("{other}"),
        },
        other => format!("{other}"),
    }
}

/// A browser build has no TLS stack and no sockets: this is never reached, because `check` refuses
/// an encrypted mode before a connection is opened. It exists so the one `send` path compiles.
#[cfg(target_arch = "wasm32")]
fn handshake(_stream: Stream, _server: &Server) -> Result<Stream, Error> {
    Err(Error::Config("this build has no TLS, and no sockets at all".to_string()))
}

/// A live session, and what it said.
struct Session {
    /// The connection. Reading goes through the buffer; writing goes to the same stream directly — a
    /// `BufReader` keeps no output, so the two directions never interleave.
    reader: BufReader<Stream>,
    log: Vec<String>,
    /// Commands sent, so a relay that keeps asking for more cannot run us in circles.
    commands: usize,
}

impl Session {
    /// Connect, greet, say hello, encrypt when asked and log in when credentials were given.
    ///
    /// This is the part a send and a check share, so a check runs the very session a message would
    /// — the same greeting, the same `EHLO`, the same `STARTTLS` upgrade and the same `AUTH`.
    fn connect_authenticated(server: &Server) -> Result<(Session, Vec<String>), Error> {
        let mut session = Session::connect(server)?;
        session.greeting()?;
        let mut capabilities = session.hello(&server.client_name)?;

        // RFC 3207 §4.2: once the session is encrypted, what the relay said before is forgotten
        // and `EHLO` is issued again — a relay may offer different extensions on a protected
        // session.
        if server.security == Security::StartTls {
            if !capabilities.iter().any(|word| word == "STARTTLS") {
                return Err(Error::Config(format!(
                    "the server does not offer STARTTLS (it listed: {}), so this session cannot be encrypted; \
                     use security \"tls\" for a TLS port (465), or \"none\" only for a relay you trust",
                    if capabilities.is_empty() { "nothing".to_string() } else { capabilities.join(" ") }
                )));
            }
            session = session.starttls(server)?;
            capabilities = session.hello(&server.client_name)?;
            session.note("TLS established (RFC 3207 STARTTLS)");
        }

        session.authenticate(server, &capabilities)?;
        Ok((session, capabilities))
    }

    fn connect(server: &Server) -> Result<Session, Error> {
        let addrs = (server.host.trim(), server.port)
            .to_socket_addrs()
            .map_err(|e| Error::Connect(format!("{}:{} could not be resolved: {e}", server.host, server.port)))?;
        let mut last: Option<String> = None;
        let mut tcp: Option<TcpStream> = None;
        for addr in addrs.take(MAX_ADDRESSES) {
            match TcpStream::connect_timeout(&addr, server.timeout) {
                Ok(s) => {
                    tcp = Some(s);
                    break;
                }
                Err(e) => last = Some(e.to_string()),
            }
        }
        let tcp = tcp.ok_or_else(|| {
            Error::Connect(format!(
                "{}:{} could not be reached: {}",
                server.host.trim(),
                server.port,
                last.unwrap_or_else(|| "no address".to_string())
            ))
        })?;
        tcp.set_read_timeout(Some(server.timeout)).map_err(|e| Error::Connect(format!("could not set the read timeout: {e}")))?;
        tcp.set_write_timeout(Some(server.timeout)).map_err(|e| Error::Connect(format!("could not set the write timeout: {e}")))?;
        let stream = Stream::Plain(tcp);
        // Implicit TLS (RFC 8314): the handshake comes before the relay says anything.
        let stream = match server.security {
            Security::ImplicitTls => handshake(stream, server)?,
            _ => stream,
        };
        Ok(Session { reader: BufReader::new(stream), log: Vec::new(), commands: 0 })
    }

    fn reply(&mut self) -> Result<Reply, Error> {
        let mut lines: Vec<String> = Vec::new();
        let mut answer: Option<u16> = None;
        let mut bytes = 0usize;
        for _ in 0..MAX_REPLY_LINES {
            let mut line = Vec::new();
            let read = self.reader.read_until(b'\n', &mut line).map_err(|e| Error::Protocol(format!("reading a reply failed: {e}")))?;
            if read == 0 {
                return Err(Error::Protocol("the server closed the connection".to_string()));
            }
            bytes += read;
            if bytes > MAX_REPLY_BYTES {
                return Err(Error::Protocol("the server sent more reply than a reply can be".to_string()));
            }
            let text = String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']).to_string();
            let (code, more) = parse_reply_line(&text)?;
            if answer.is_none() {
                answer = Some(code);
            }
            lines.push(text);
            if !more {
                return Ok(Reply { code: answer.unwrap_or(code), lines });
            }
        }
        Err(Error::Protocol("the server never finished its reply".to_string()))
    }

    /// Send one command line and read its reply.
    fn command(&mut self, line: &str) -> Result<(u16, String), Error> {
        self.send_line(line)?;
        let reply = self.reply()?;
        self.log.push(format!("{line} → {} {}", reply.code, reply.text()));
        Ok((reply.code, reply.text()))
    }

    /// Send a command without keeping its text in the log (the `AUTH` token, which carries a password).
    fn command_quiet(&mut self, line: &str, shown: &str) -> Result<(u16, String), Error> {
        self.send_line(line)?;
        let reply = self.reply()?;
        self.log.push(format!("{shown} → {} {}", reply.code, reply.text()));
        Ok((reply.code, reply.text()))
    }

    /// Send a command whose failure is not interesting (the reset and the goodbye).
    fn ignore(&mut self, line: &str) {
        let _ = self.command(line);
    }

    /// Record something in the log that is not a command (the greeting, the delivered message).
    fn note(&mut self, line: &str) {
        self.log.push(line.to_string());
    }

    fn send_line(&mut self, line: &str) -> Result<(), Error> {
        self.commands += 1;
        if self.commands > MAX_COMMANDS {
            return Err(Error::Protocol("the session sent too many commands".to_string()));
        }
        let stream = self.reader.get_mut();
        stream.write_all(format!("{line}\r\n").as_bytes()).map_err(|e| Error::Protocol(format!("sending {line} failed: {e}")))?;
        stream.flush().map_err(|e| Error::Protocol(format!("sending {line} failed: {e}")))
    }

    fn write_body(&mut self, payload: &[u8]) -> Result<(), Error> {
        let mut framed = Vec::with_capacity(payload.len() + 8);
        terminate(payload, &mut framed);
        self.commands += 1;
        let stream = self.reader.get_mut();
        stream.write_all(&framed).and_then(|()| stream.flush()).map_err(|e| Error::Protocol(format!("sending the message failed: {e}")))
    }

    /// `STARTTLS` (RFC 3207): ask the relay for it, then carry the same socket inside TLS. Consumes
    /// the session — the encrypted one is a new one, with nothing the plaintext peer said kept.
    fn starttls(self, server: &Server) -> Result<Session, Error> {
        let mut plain = self;
        plain.expect("STARTTLS", &[220])?;
        // RFC 3207 §4: anything the relay sent after its 220 would be read as plaintext, which the
        // upgrade cannot carry, so it is a protocol error rather than silently dropped.
        let buffered = plain.reader.buffer().len();
        if buffered != 0 {
            return Err(Error::Protocol(format!(
                "the server sent {buffered} bytes after accepting STARTTLS, which cannot be carried into the encrypted session"
            )));
        }
        let stream = handshake(plain.reader.into_inner(), server)?;
        Ok(Session { reader: BufReader::new(stream), log: plain.log, commands: plain.commands })
    }

    /// Say hello and keep the greeting in the log.
    fn greeting(&mut self) -> Result<(), Error> {
        let reply = self.reply()?;
        self.log.push(format!("server greeting → {} {}", reply.code, reply.text()));
        if reply.code != 220 {
            return Err(Error::Refused { command: "the greeting".to_string(), code: reply.code, reply: reply.text() });
        }
        Ok(())
    }

    /// `EHLO`, falling back to `HELO` for a relay too old to know it. Returns the extensions the
    /// relay listed (`EHLO` only).
    fn hello(&mut self, client_name: &str) -> Result<Vec<String>, Error> {
        self.send_line(&format!("EHLO {client_name}"))?;
        let reply = self.reply()?;
        self.log.push(format!("EHLO {client_name} → {} {}", reply.code, reply.text()));
        match reply.code {
            250 => Ok(reply.words()),
            // A relay that does not implement EHLO says so with 500, 502 or 504 (RFC 5321 §4.1.1.1).
            500 | 502 | 504 => {
                let (code, text) = self.command(&format!("HELO {client_name}"))?;
                if code != 250 {
                    return Err(Error::Refused { command: format!("HELO {client_name}"), code, reply: text });
                }
                Ok(Vec::new())
            }
            code => Err(Error::Refused { command: format!("EHLO {client_name}"), code, reply: reply.text() }),
        }
    }

    /// `AUTH PLAIN` (RFC 4954) when the caller gave credentials. A relay that does not offer it is
    /// an error rather than a silent unauthenticated send: the caller asked to authenticate.
    fn authenticate(&mut self, server: &Server, capabilities: &[String]) -> Result<(), Error> {
        let (Some(user), Some(password)) = (&server.username, &server.password) else { return Ok(()) };
        let offers_auth = capabilities.iter().any(|word| word == "AUTH");
        if !offers_auth || !capabilities.iter().any(|word| word == "PLAIN") {
            return Err(Error::Config(format!(
                "the server does not offer AUTH PLAIN (it listed: {}), so the credentials cannot be used; send without them, or through a relay that offers it",
                if capabilities.is_empty() { "nothing".to_string() } else { capabilities.join(" ") }
            )));
        }
        let token = auth_plain_token(user, password);
        let (code, text) = self.command_quiet(&format!("AUTH PLAIN {token}"), "AUTH PLAIN <credentials>")?;
        let (code, text) = if code == 334 {
            // The relay asked for the token separately; send it and keep the password out of the log.
            self.command_quiet(&token, "AUTH PLAIN <credentials>")?
        } else {
            (code, text)
        };
        if code != 235 {
            return Err(Error::Refused { command: "AUTH PLAIN".to_string(), code, reply: text });
        }
        Ok(())
    }

    /// Send a command that must be accepted, with the replies that are fine.
    fn expect(&mut self, line: &str, ok: &[u16]) -> Result<(), Error> {
        let (code, text) = self.command(line)?;
        if ok.contains(&code) { Ok(()) } else { Err(Error::Refused { command: line.to_string(), code, reply: text }) }
    }
}

/// The `AUTH PLAIN` token: `NUL user NUL password`, base64 (RFC 4616).
fn auth_plain_token(user: &str, password: &str) -> String {
    let mut raw = Vec::with_capacity(user.len() + password.len() + 2);
    raw.push(0);
    raw.extend_from_slice(user.as_bytes());
    raw.push(0);
    raw.extend_from_slice(password.as_bytes());
    base64(&raw)
}

/// Standard base64 with padding. Written here because a crate that carries no dependency cannot
/// borrow one (the same reason `pdfcraft-eml` has its own SHA-256).
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let triple = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(triple >> 6) as usize & 0x3f] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[triple as usize & 0x3f] as char } else { '=' });
    }
    out
}

/// `250-text` continues, `250 text` ends, and the first three bytes are the code.
fn parse_reply_line(line: &str) -> Result<(u16, bool), Error> {
    let bytes = line.as_bytes();
    if bytes.len() < 3 {
        return Err(Error::Protocol(format!("{line:?} is not an SMTP reply")));
    }
    let code = std::str::from_utf8(&bytes[..3])
        .ok()
        .and_then(|code| code.parse::<u16>().ok())
        .filter(|code| (100..=599).contains(code))
        .ok_or_else(|| Error::Protocol(format!("{line:?} is not an SMTP reply")))?;
    let more = bytes.get(3) == Some(&b'-');
    Ok((code, more))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_composed_message_goes_out_byte_for_byte() {
        // What pdfcraft-eml composes: LF line endings, base64 parts, no line starting with a dot.
        let message = b"From: a@b\nTo: c@d\nSubject: hi\nMIME-Version: 1.0\nContent-Type: text/plain\n\nYWJj\n";
        let (payload, stuffed) = frame(message, false).expect("framing");
        assert_eq!(payload, message);
        assert_eq!(stuffed, 0);
    }

    #[test]
    fn a_carriage_return_is_refused_before_anything_is_sent() {
        let err = frame(b"Subject: hi\r\nFrom: a@b\n", false).expect_err("CRLF in a verbatim message");
        assert!(matches!(err, Error::Config(_)), "{err:?}");
        assert!(err.to_string().contains("carriage return"), "{err}");
    }

    #[test]
    fn a_leading_dot_is_doubled_so_the_receiver_keeps_it() {
        let (payload, stuffed) = frame(b"a\n.hidden\nb\n", false).expect("framing");
        assert_eq!(payload, b"a\n..hidden\nb\n");
        assert_eq!(stuffed, 1);
    }

    #[test]
    fn crlf_mode_converts_line_endings_and_still_stuffs() {
        let (payload, stuffed) = frame(b"a\nb\r\n.c\n", true).expect("framing");
        assert_eq!(payload, b"a\r\nb\r\n..c\r\n");
        assert_eq!(stuffed, 1);
    }

    #[test]
    fn the_end_of_data_marker_is_crlf_dot_crlf() {
        let mut framed = Vec::new();
        terminate(b"body\n", &mut framed);
        assert_eq!(framed, b"body\n\r\n.\r\n");
        let mut framed = Vec::new();
        terminate(b"body\r\n", &mut framed);
        assert_eq!(framed, b"body\r\n.\r\n");
    }

    #[test]
    fn security_is_read_by_name_and_the_unknown_is_refused() {
        assert_eq!(Security::parse("none").expect("none"), Security::Plain);
        assert_eq!(Security::parse("STARTTLS").expect("starttls"), Security::StartTls);
        assert_eq!(Security::parse("smtps").expect("smtps"), Security::ImplicitTls);
        assert!(matches!(Security::parse("maybe"), Err(Error::Config(_))));
    }

    #[test]
    fn an_address_a_relay_could_not_read_is_refused() {
        assert_eq!(address("from", " ada@example.com ").expect("padded"), "ada@example.com");
        for bad in ["", "ada", "a b@c", "a@b\nRCPT TO:<evil@x>", "a@b@c", "a@", "@b", "<a@b>", "a,b@c"] {
            assert!(matches!(address("from", bad), Err(Error::Config(_))), "{bad:?} should be refused");
        }
    }

    #[test]
    fn the_envelope_keeps_one_entry_per_address() {
        let list = envelope(&["a@b".to_string(), "A@B".to_string(), " c@d ".to_string()]).expect("envelope");
        assert_eq!(list, vec!["a@b".to_string(), "c@d".to_string()]);
        assert!(matches!(envelope(&[]), Err(Error::Config(_))));
    }

    #[test]
    fn a_tls_request_is_carried_out_rather_than_refused_before_the_socket() {
        // Nothing listens on 127.0.0.1:1, so reaching the connect step — a `Connect` error, not a
        // `Config` one — is the proof that the encrypted modes are no longer refused up front.
        for security in [Security::StartTls, Security::ImplicitTls] {
            let server = Server { host: "127.0.0.1".into(), port: 1, security, ..Default::default() };
            let err = send(&server, "a@b", &["c@d".to_string()], b"Subject: hi\n\nbody\n").expect_err("unreachable");
            assert!(matches!(err, Error::Connect(_)), "{security:?}: {err:?}");
        }
    }

    #[test]
    fn credentials_without_a_user_name_are_refused_before_a_socket_is_opened() {
        let server = Server { host: "127.0.0.1".into(), port: 1, password: Some("secret".into()), ..Default::default() };
        let err = send(&server, "a@b", &["c@d".to_string()], b"Subject: hi\n\nbody\n").expect_err("password alone");
        assert!(matches!(err, Error::Config(_)), "{err:?}");
    }

    #[test]
    fn the_preset_map_finds_the_provider_of_an_address() {
        let gmail = preset("user@gmail.com").expect("gmail is in the map");
        assert_eq!(gmail.name, "Gmail");
        assert_eq!((gmail.host, gmail.port, gmail.security), ("smtp.gmail.com", 465, Security::ImplicitTls));
        assert_eq!(gmail.alternative, Some((587, Security::StartTls)));
        assert_eq!(gmail.app_password_page, Some("https://myaccount.google.com/apppasswords"));
        // The other spelling of the same provider, a bare domain, another case and a root dot.
        assert_eq!(preset("USER@GoogleMail.COM").map(|p| p.name), Some("Gmail"));
        assert_eq!(preset(" gmail.com ").map(|p| p.name), Some("Gmail"));
        assert_eq!(preset("gmail.com.").map(|p| p.name), Some("Gmail"));
        for (domain, host, port, security) in [
            ("yandex.ru", "smtp.yandex.ru", 465, Security::ImplicitTls),
            ("yandex.com", "smtp.yandex.ru", 465, Security::ImplicitTls),
            ("yandex.by", "smtp.yandex.ru", 465, Security::ImplicitTls),
            ("mail.ru", "smtp.mail.ru", 465, Security::ImplicitTls),
            ("bk.ru", "smtp.mail.ru", 465, Security::ImplicitTls),
            ("inbox.ru", "smtp.mail.ru", 465, Security::ImplicitTls),
            ("list.ru", "smtp.mail.ru", 465, Security::ImplicitTls),
            ("outlook.com", "smtp-mail.outlook.com", 587, Security::StartTls),
            ("hotmail.com", "smtp-mail.outlook.com", 587, Security::StartTls),
            ("live.com", "smtp-mail.outlook.com", 587, Security::StartTls),
        ] {
            let entry = preset(domain).unwrap_or_else(|| panic!("{domain} is not in the map"));
            assert_eq!((entry.host, entry.port, entry.security), (host, port, security), "{domain}");
        }
    }

    #[test]
    fn a_domain_the_map_does_not_know_fills_in_nothing() {
        for value in ["", " ", "@", "user@", "example.org", "user@example.org", "gmail.com.example", "user@mail.ru.example"] {
            assert!(preset(value).is_none(), "{value:?} should not be a known provider");
        }
    }

    #[test]
    fn every_preset_names_a_relay_a_session_could_use() {
        for entry in PRESETS {
            assert!(!entry.name.trim().is_empty() && !entry.domains.is_empty(), "{entry:?}");
            // What the entry fills in has to pass the same checks a session makes before its socket.
            let server = Server { host: entry.host.to_string(), port: entry.port, security: entry.security, ..Default::default() };
            assert!(check(&server).is_ok(), "{entry:?}");
            for domain in entry.domains {
                assert_eq!(domain.to_ascii_lowercase(), *domain, "{domain} is listed in another case");
                assert!(preset(&format!("someone@{domain}")).is_some(), "{domain}");
            }
        }
    }

    #[test]
    fn a_check_refuses_what_it_could_not_carry_out_before_the_socket() {
        // No host at all, and a password with no user name: both are configuration mistakes a
        // check reports as such (a `Config` error) rather than by opening anything.
        assert!(matches!(verify(&Server::default()), Err(Error::Config(_))));
        let server = Server { host: "127.0.0.1".into(), port: 1, password: Some("secret".into()), ..Default::default() };
        assert!(matches!(verify(&server), Err(Error::Config(_))));
    }

    #[test]
    fn the_auth_token_is_nul_separated_and_base64() {
        assert_eq!(auth_plain_token("user", "pass"), "AHVzZXIAcGFzcw==");
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
        assert_eq!(base64(b"abcd"), "YWJjZA==");
    }

    #[test]
    fn a_reply_line_says_whether_it_continues() {
        assert_eq!(parse_reply_line("220 ready").expect("greeting"), (220, false));
        assert_eq!(parse_reply_line("250-SIZE 10240000").expect("extension"), (250, true));
        assert!(matches!(parse_reply_line("hi"), Err(Error::Protocol(_))));
        assert!(matches!(parse_reply_line("999 nope"), Err(Error::Protocol(_))));
    }
}
