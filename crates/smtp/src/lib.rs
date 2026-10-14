//! pdfcraft-smtp — delivering a message over SMTP (RFC 5321).
//!
//! A client, not a server: it connects, says hello, authenticates when the caller gave it
//! credentials, hands over the message bytes and quits. It composes nothing (that is
//! `pdfcraft-eml`) and it holds no configuration of its own — no relay, no credentials, no port:
//! [`send`] opens a socket only when a caller passes a [`Server`], which is what makes sending an
//! explicit, opt-in step rather than something a document ever does on its own.
//!
//! **Layer:** L0 (standalone, publishable). No dependency beyond the standard library.
//!
//! Clean-room from the public RFCs only: RFC 5321 (the SMTP session and the envelope),
//! RFC 4954 (the `AUTH` extension and its `PLAIN` mechanism). Everything is a returned value:
//! a bad address, an unreachable relay, a refused recipient and a relay that talks nonsense all
//! come back as [`Error`], and no path in this crate panics.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

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
    /// No transport security: the session runs in the clear. The only one this build carries out.
    Plain,
    /// `STARTTLS` (RFC 3207).
    StartTls,
    /// Implicit TLS, as `smtps://` speaks (RFC 8314).
    ImplicitTls,
}

impl Security {
    /// Read a `security` argument: `none`, `starttls` or `tls`. Naming a mode this build cannot
    /// carry out is not an error here — [`send`] refuses it before opening anything.
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
            Error::Protocol(m) => write!(f, "{m}"),
            Error::Refused { command, code, reply } => write!(f, "the server refused {command}: {code} {reply}"),
        }
    }
}

impl std::error::Error for Error {}

/// Hand `message` to `server` for `recipients`, as `from`.
///
/// The session is: connect, greeting, `EHLO` (falling back to `HELO`), `AUTH PLAIN` when
/// credentials were given, `MAIL FROM`, one `RCPT TO` per recipient, `DATA`, the message, `QUIT`.
/// The message bytes are framed for the transfer and otherwise left alone (see [`Server::crlf`]).
///
/// A configuration mistake — an address a relay could not read, a mode this build does not carry
/// out, credentials without a user name — comes back as [`Error::Config`] **before** the socket is
/// opened.
pub fn send(server: &Server, from: &str, recipients: &[String], message: &[u8]) -> Result<Report, Error> {
    let from = address("from", from)?;
    let recipients = envelope(recipients)?;
    let (payload, stuffed_lines) = frame(message, server.crlf)?;
    check(server)?;

    let mut session = Session::connect(server)?;
    session.greeting()?;
    let capabilities = session.hello(&server.client_name)?;
    session.authenticate(server, &capabilities)?;

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
    match server.security {
        Security::Plain => {}
        Security::StartTls | Security::ImplicitTls => {
            return Err(Error::Config(format!(
                "security {:?} is not available in this build: the workspace's TLS stack (rustls, rustls-native-certs) lives in the app tier and is not wired into pdfcraft-smtp yet, \
                 so this client would otherwise send a password in the clear without saying so — pass security \"none\" for a local or already-trusted relay",
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

/// A live session, and what it said.
struct Session {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    log: Vec<String>,
    /// Commands sent, so a relay that keeps asking for more cannot run us in circles.
    commands: usize,
}

impl Session {
    fn connect(server: &Server) -> Result<Session, Error> {
        let addrs = (server.host.trim(), server.port)
            .to_socket_addrs()
            .map_err(|e| Error::Connect(format!("{}:{} could not be resolved: {e}", server.host, server.port)))?;
        let mut last: Option<String> = None;
        let mut stream: Option<TcpStream> = None;
        for addr in addrs.take(MAX_ADDRESSES) {
            match TcpStream::connect_timeout(&addr, server.timeout) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) => last = Some(e.to_string()),
            }
        }
        let stream = stream.ok_or_else(|| {
            Error::Connect(format!(
                "{}:{} could not be reached: {}",
                server.host.trim(),
                server.port,
                last.unwrap_or_else(|| "no address".to_string())
            ))
        })?;
        stream.set_read_timeout(Some(server.timeout)).map_err(|e| Error::Connect(format!("could not set the read timeout: {e}")))?;
        stream.set_write_timeout(Some(server.timeout)).map_err(|e| Error::Connect(format!("could not set the write timeout: {e}")))?;
        let writer = stream.try_clone().map_err(|e| Error::Connect(format!("could not use the connection: {e}")))?;
        Ok(Session { reader: BufReader::new(stream), writer, log: Vec::new(), commands: 0 })
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
        self.writer.write_all(format!("{line}\r\n").as_bytes()).map_err(|e| Error::Protocol(format!("sending {line} failed: {e}")))?;
        self.writer.flush().map_err(|e| Error::Protocol(format!("sending {line} failed: {e}")))
    }

    fn write_body(&mut self, payload: &[u8]) -> Result<(), Error> {
        let mut framed = Vec::with_capacity(payload.len() + 8);
        terminate(payload, &mut framed);
        self.commands += 1;
        self.writer.write_all(&framed).and_then(|()| self.writer.flush()).map_err(|e| Error::Protocol(format!("sending the message failed: {e}")))
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
    fn tls_is_refused_before_a_socket_is_opened() {
        // `127.0.0.1:1` would refuse the connection if we ever got that far, so an Err that is
        // Config rather than Connect is the proof that nothing was opened.
        let server = Server { host: "127.0.0.1".into(), port: 1, security: Security::StartTls, ..Default::default() };
        let err = send(&server, "a@b", &["c@d".to_string()], b"Subject: hi\n\nbody\n").expect_err("starttls");
        assert!(matches!(err, Error::Config(_)), "{err:?}");
        assert!(err.to_string().contains("starttls"), "{err}");
    }

    #[test]
    fn credentials_without_a_user_name_are_refused_before_a_socket_is_opened() {
        let server = Server { host: "127.0.0.1".into(), port: 1, password: Some("secret".into()), ..Default::default() };
        let err = send(&server, "a@b", &["c@d".to_string()], b"Subject: hi\n\nbody\n").expect_err("password alone");
        assert!(matches!(err, Error::Config(_)), "{err:?}");
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
