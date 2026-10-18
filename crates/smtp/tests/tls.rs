//! TLS against a real socket: a local receiver on loopback that speaks the session from RFC 5321
//! inside TLS — the workspace's own rustls stack on both sides, a test certificate in
//! `tests/data/` (README there regenerates it), and nothing outside the loopback interface.
//!
//! What is proved here: `starttls` upgrades the session and the message arrives byte for byte,
//! `tls` (implicit, port 465) does the same from the first byte, the certificate is checked against
//! the roots the client was given (an unknown issuer is refused and **nothing is sent**), and a
//! relay that does not offer `STARTTLS` is an error rather than a session that quietly stays in the
//! clear.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread::JoinHandle;

use pdfcraft_smtp::{Error, Security, Server, send};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// The message both tests send: the same shape `pdfcraft-eml` composes (LF line endings, base64
/// parts), so what the receiver keeps is exactly what the `.eml` holds.
const MESSAGE: &[u8] =
    b"From: ada@example.com\nTo: team@example.com\nSubject: hi\nMIME-Version: 1.0\nContent-Type: text/plain\n\nY29tcG9zZWQgYnkgaGFuZAo=\n";

fn der(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))).expect("a test certificate")
}

/// The receiver's own certificate and key, and the CA the client has to trust.
fn receiver() -> (CertificateDer<'static>, PrivateKeyDer<'static>, Vec<u8>) {
    let cert = CertificateDer::from(der("receiver.crt.der"));
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(der("receiver.key.der")));
    let root = der("ca.crt.der");
    (cert, key, root)
}

fn server_config() -> Arc<rustls::ServerConfig> {
    let (cert, key, _) = receiver();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("TLS 1.2 and 1.3")
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .expect("the test certificate and key match");
    Arc::new(config)
}

/// How the receiver behaves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Greeting in the clear, `STARTTLS` offered, then the upgrade.
    StartTls,
    /// Greeting in the clear, and `STARTTLS` is not offered at all.
    NoStartTls,
    /// Implicit TLS (RFC 8314): the handshake comes before the greeting.
    Implicit,
}

/// What the receiver saw.
#[derive(Default)]
struct Record {
    /// The command lines it was sent, in order (both before and after the upgrade).
    commands: Vec<String>,
    /// The message it kept, with the transfer's framing removed.
    payload: Vec<u8>,
    /// The session was encrypted when the message arrived.
    encrypted: bool,
}

fn receive(mode: Mode) -> (u16, JoinHandle<Record>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("the port").port();
    let handle = std::thread::spawn(move || serve(listener, mode));
    (port, handle)
}

fn serve(listener: TcpListener, mode: Mode) -> Record {
    let (tcp, _) = listener.accept().expect("one connection");
    match mode {
        Mode::Implicit => {
            let mut stream = match accept(tcp, true) {
                Ok(stream) => stream,
                // The client refused our certificate: nothing was sent, which the test asserts.
                Err(record) => return record,
            };
            // RFC 8314: the greeting still comes first — it is simply inside TLS.
            let _ = stream.write_all(b"220 receiver.test ESMTP pdfcraft-smtp-test\r\n");
            dialogue(&mut stream, true)
        }
        Mode::StartTls | Mode::NoStartTls => {
            let mut plain = tcp;
            let mut record = Record::default();
            let _ = plain.write_all(b"220 receiver.test ESMTP pdfcraft-smtp-test\r\n");
            let mut reader = BufReader::new(plain.try_clone().expect("a reading handle"));
            let Some(echo) = line(&mut reader) else { return record };
            record.commands.push(echo.clone());
            if !echo.to_ascii_uppercase().starts_with("EHLO") {
                let _ = plain.write_all(b"500 5.5.1 say hello first\r\n");
                return record;
            }
            let offers = mode == Mode::StartTls;
            let _ = match offers {
                true => plain.write_all(b"250-receiver.test\r\n250-STARTTLS\r\n250 SIZE 10485760\r\n"),
                false => plain.write_all(b"250 receiver.test\r\n250 SIZE 10485760\r\n"),
            };
            let Some(line) = line(&mut reader) else { return record };
            record.commands.push(line.clone());
            if !offers || !line.to_ascii_uppercase().starts_with("STARTTLS") {
                // Whatever came next was refused by the client, or the session stayed in the clear:
                // say so and stop, keeping what was seen.
                let _ = plain.write_all(b"502 5.5.1 STARTTLS is not offered\r\n");
                return record;
            }
            let _ = plain.write_all(b"220 2.0.0 ready to start TLS\r\n");
            let plain = reader.into_inner();
            match accept(plain, true) {
                Ok(mut stream) => {
                    let mut after = dialogue(&mut stream, true);
                    after.commands.splice(0..0, record.commands);
                    after
                }
                Err(mut refused) => {
                    refused.commands.splice(0..0, record.commands);
                    refused
                }
            }
        }
    }
}

/// Finish the handshake and hand back the encrypted stream, or what was seen when it failed.
fn accept(tcp: TcpStream, implicit: bool) -> Result<rustls::StreamOwned<rustls::ServerConnection, TcpStream>, Record> {
    let _ = implicit;
    let mut session = rustls::ServerConnection::new(server_config()).expect("a server session");
    let mut tcp = tcp;
    let outcome = loop {
        match session.complete_io(&mut tcp) {
            Ok(_) if !session.is_handshaking() => break true,
            Ok(_) => {}
            Err(_) => break false,
        }
    };
    match outcome {
        true => Ok(rustls::StreamOwned::new(session, tcp)),
        false => Err(Record::default()),
    }
}

/// The rest of the session: `EHLO`, `MAIL FROM`, `RCPT TO`, `DATA`, the message, `QUIT`.
fn dialogue<S: Read + Write>(stream: &mut S, encrypted: bool) -> Record {
    let mut record = Record { encrypted, ..Default::default() };
    let mut reader = BufReader::new(stream);
    loop {
        let Some(line) = line(&mut reader) else { return record };
        record.commands.push(line.clone());
        let upper = line.to_ascii_uppercase();
        let reply: &[u8] = if upper.starts_with("EHLO") {
            b"250-receiver.test\r\n250 SIZE 10485760\r\n"
        } else if upper.starts_with("MAIL FROM") {
            b"250 2.1.0 sender ok\r\n"
        } else if upper.starts_with("RCPT TO") {
            b"250 2.1.5 recipient ok\r\n"
        } else if upper == "DATA" {
            b"354 go ahead\r\n"
        } else if upper == "QUIT" {
            let _ = reader.get_mut().write_all(b"221 2.0.0 bye\r\n");
            return record;
        } else {
            b"500 5.5.1 what?\r\n"
        };
        if reader.get_mut().write_all(reply).is_err() {
            return record;
        }
        if upper == "DATA" {
            // Everything up to the CRLF.CRLF end-of-data marker is the message.
            let mut body = Vec::new();
            loop {
                let mut raw = Vec::new();
                match reader.read_until(b'\n', &mut raw) {
                    Ok(0) | Err(_) => return record,
                    Ok(_) => {}
                }
                if raw == b".\r\n" || raw == b".\n" {
                    break;
                }
                body.extend_from_slice(&raw);
            }
            // The transfer's end-of-data marker is `CRLF.CRLF`: the CRLF in front of the dot is
            // the framing, not part of the message.
            if body.ends_with(b"\r\n") {
                body.truncate(body.len() - 2);
            }
            // Dot-stuffing is undone, as a receiver does (RFC 5321 §4.5.2).
            record.payload = unstuff(&body);
            if reader.get_mut().write_all(b"250 2.0.0 message accepted\r\n").is_err() {
                return record;
            }
        }
    }
}

fn unstuff(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut at_start = true;
    for &byte in body {
        if at_start && byte == b'.' {
            // Drop the transfer's own doubling; the rest of the line is the message.
            at_start = false;
            continue;
        }
        at_start = byte == b'\n';
        out.push(byte);
    }
    out
}

fn line<R: BufRead>(reader: &mut R) -> Option<String> {
    let mut text = String::new();
    match reader.read_line(&mut text) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(text.trim_end_matches(['\r', '\n']).to_string()),
    }
}

/// A client for the local receiver: `host` is `localhost`, so the test certificate's name checks out.
fn client(security: Security, port: u16, root: Option<Vec<u8>>) -> Server {
    Server {
        host: "localhost".to_string(),
        port,
        security,
        extra_roots: root.into_iter().collect(),
        timeout: std::time::Duration::from_secs(5),
        ..Default::default()
    }
}

#[test]
fn starttls_upgrades_the_session_and_the_message_arrives_byte_for_byte() {
    let (cert, _, root) = receiver();
    assert!(!cert.is_empty(), "the test certificate is there");
    let (port, receiver) = receive(Mode::StartTls);
    let report = send(&client(Security::StartTls, port, Some(root)), "ada@example.com", &["team@example.com".to_string()], MESSAGE)
        .expect("a session that is encrypted before anything is sent");
    assert_eq!(report.accepted, vec!["team@example.com".to_string()]);
    assert!(report.log.iter().any(|step| step.contains("TLS established")), "{:?}", report.log);

    let record = receiver.join().expect("the receiver finished");
    assert!(record.encrypted, "the message was handed over inside TLS");
    assert_eq!(record.payload, MESSAGE, "the receiver kept the message byte for byte");
    assert_eq!(record.commands.first().map(String::as_str), Some("EHLO pdfcraft.local"), "{:?}", record.commands);
    assert!(record.commands.iter().any(|c| c == "STARTTLS"), "{:?}", record.commands);
    // RFC 3207 §4.2: hello is said again on the encrypted session, before the envelope.
    let hello = record.commands.iter().filter(|c| c.starts_with("EHLO")).count();
    assert_eq!(hello, 2, "{:?}", record.commands);
    let mail = record.commands.iter().position(|c| c.starts_with("MAIL FROM")).expect("an envelope");
    let starttls = record.commands.iter().position(|c| c == "STARTTLS").expect("the upgrade");
    assert!(starttls < mail, "nothing is sent before the session is encrypted: {:?}", record.commands);
}

#[test]
fn implicit_tls_delivers_the_message_from_the_first_byte() {
    let (_, _, root) = receiver();
    let (port, receiver) = receive(Mode::Implicit);
    let report = send(&client(Security::ImplicitTls, port, Some(root)), "ada@example.com", &["team@example.com".to_string()], MESSAGE)
        .expect("a session on a TLS port");
    assert_eq!(report.accepted, vec!["team@example.com".to_string()]);
    let record = receiver.join().expect("the receiver finished");
    assert!(record.encrypted);
    assert_eq!(record.payload, MESSAGE);
    assert!(!record.commands.iter().any(|c| c == "STARTTLS"), "there is nothing to upgrade: {:?}", record.commands);
}

#[test]
fn a_certificate_the_client_cannot_vouch_for_is_refused_and_nothing_is_sent() {
    let (port, receiver) = receive(Mode::StartTls);
    // No extra roots: only the operating system's, which do not contain the test CA.
    let error = send(&client(Security::StartTls, port, None), "ada@example.com", &["team@example.com".to_string()], MESSAGE)
        .expect_err("an unverifiable certificate must stop the session");
    assert!(matches!(error, Error::Tls(_)), "{error:?}");
    let text = error.to_string();
    assert!(text.contains("certificate"), "{text}");
    assert!(text.contains("nothing was sent"), "{text}");

    let record = receiver.join().expect("the receiver finished");
    assert!(record.payload.is_empty(), "the message never reached the receiver");
    assert!(!record.commands.iter().any(|c| c.starts_with("MAIL FROM")), "{:?}", record.commands);
}

#[test]
fn a_relay_without_starttls_is_an_error_rather_than_a_cleartext_session() {
    let (_, _, root) = receiver();
    let (port, receiver) = receive(Mode::NoStartTls);
    let error = send(&client(Security::StartTls, port, Some(root)), "ada@example.com", &["team@example.com".to_string()], MESSAGE)
        .expect_err("a relay with no STARTTLS cannot be used encrypted");
    assert!(matches!(error, Error::Config(_)), "{error:?}");
    assert!(error.to_string().contains("STARTTLS"), "{error}");

    let record = receiver.join().expect("the receiver finished");
    assert!(record.payload.is_empty(), "the message was not sent in the clear");
    assert!(!record.commands.iter().any(|c| c.starts_with("MAIL FROM")), "{:?}", record.commands);
}
