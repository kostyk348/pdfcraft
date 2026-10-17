//! The client against a real socket: a local receiver on loopback that speaks the minimum of
//! RFC 5321 (`EHLO`, `MAIL FROM`, `RCPT TO`, `DATA`, `QUIT`). No internet, no relay, no port
//! outside the loopback interface — everything here is a session with this file's own thread.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;

use pdfcraft_smtp::{Error, Security, Server, send};

/// How the receiver answers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Accept everything.
    Accept,
    /// Refuse every recipient with a permanent error.
    RefuseRecipients,
    /// Advertise `AUTH PLAIN` and accept the credentials.
    WithAuth,
    /// Refuse `EHLO`, so the client has to fall back to `HELO`.
    HeloOnly,
}

/// What the receiver saw.
#[derive(Default)]
struct Record {
    /// The command lines it was sent, in order.
    commands: Vec<String>,
    /// The `AUTH` token it was given, if any.
    auth: Vec<String>,
    /// The message it kept: the transfer's framing removed.
    payload: Vec<u8>,
    /// The `DATA` line was reached.
    got_data: bool,
}

fn receive(mode: Mode) -> (u16, JoinHandle<Record>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("the port").port();
    let handle = std::thread::spawn(move || serve(listener, mode));
    (port, handle)
}

fn serve(listener: TcpListener, mode: Mode) -> Record {
    let (stream, _) = listener.accept().expect("one connection");
    let mut writer = stream;
    let mut reader = BufReader::new(writer.try_clone().expect("a second handle"));
    let _ = writer.write_all(b"220 receiver.test ESMTP pdfcraft-smtp-test\r\n");
    let mut record = Record::default();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).expect("a command line") == 0 {
            return record;
        }
        let command = line.trim_end_matches(['\r', '\n']).to_string();
        record.commands.push(command.clone());
        let upper = command.to_ascii_uppercase();
        match upper.as_str() {
            _ if upper.starts_with("EHLO") && mode == Mode::HeloOnly => {
                let _ = writer.write_all(b"500 5.5.1 command not recognised\r\n");
            }
            _ if upper.starts_with("EHLO") => {
                let _ = writer.write_all(b"250-receiver.test\r\n");
                if mode == Mode::WithAuth {
                    let _ = writer.write_all(b"250-AUTH PLAIN LOGIN\r\n");
                }
                let _ = writer.write_all(b"250 SIZE 10485760\r\n");
            }
            _ if upper.starts_with("HELO") => {
                let _ = writer.write_all(b"250 receiver.test\r\n");
            }
            _ if upper.starts_with("AUTH") => {
                record.auth.push(command.split_whitespace().nth(2).unwrap_or("").to_string());
                let _ = writer.write_all(b"235 2.7.0 accepted\r\n");
            }
            _ if upper.starts_with("MAIL FROM") => {
                let _ = writer.write_all(b"250 2.1.0 sender ok\r\n");
            }
            _ if upper.starts_with("RCPT TO") => {
                if mode == Mode::RefuseRecipients {
                    let _ = writer.write_all(b"550 5.1.1 no such user here\r\n");
                } else {
                    let _ = writer.write_all(b"250 2.1.5 recipient ok\r\n");
                }
            }
            "DATA" => {
                record.got_data = true;
                let _ = writer.write_all(b"354 end data with <CRLF>.<CRLF>\r\n");
                let mut kept: Vec<u8> = Vec::new();
                loop {
                    let mut chunk = Vec::new();
                    if reader.read_until(b'\n', &mut chunk).expect("message data") == 0 {
                        break;
                    }
                    if chunk == b".\r\n" || chunk == b".\n" {
                        break;
                    }
                    kept.extend_from_slice(&chunk);
                }
                // The transfer's end-of-data marker is `CRLF.CRLF`: the CRLF in front of the dot is
                // the framing, not part of the message.
                if kept.ends_with(b"\r\n") {
                    kept.truncate(kept.len() - 2);
                }
                record.payload = kept;
                let _ = writer.write_all(b"250 2.0.0 ok: queued as 42\r\n");
            }
            "RSET" => {
                let _ = writer.write_all(b"250 2.0.0 flushed\r\n");
            }
            "QUIT" => {
                let _ = writer.write_all(b"221 2.0.0 bye\r\n");
                return record;
            }
            _ => {
                let _ = writer.write_all(b"500 5.5.1 command not recognised\r\n");
            }
        }
    }
}

/// A message the way `pdfcraft-eml` composes one: LF endings, base64 leaves, no leading dots.
fn message() -> Vec<u8> {
    let mut out = b"From: pdfcraft@localhost\nTo: team@example.com\nSubject: Two pages\nMIME-Version: 1.0\nContent-Type: multipart/mixed; boundary=\"=_pdfcraft_test\"\n\n".to_vec();
    out.extend_from_slice(
        b"--=_pdfcraft_test\nContent-Type: text/plain; charset=utf-8\nContent-Transfer-Encoding: base64\n\nUGFnZSAx\n--=_pdfcraft_test--\n",
    );
    out
}

fn server(port: u16) -> Server {
    Server { host: "127.0.0.1".to_string(), port, timeout: std::time::Duration::from_secs(5), ..Default::default() }
}

#[test]
fn a_message_arrives_byte_for_byte() {
    let (port, receiver) = receive(Mode::Accept);
    let bytes = message();
    let report = send(&server(port), "pdfcraft@localhost", &["team@example.com".to_string()], &bytes).expect("the message is sent");
    let record = receiver.join().expect("the receiver thread");

    assert_eq!(record.payload, bytes, "the receiver's copy differs from the composed message");
    assert_eq!(report.bytes, bytes.len());
    assert_eq!(report.stuffed_lines, 0);
    assert_eq!(report.accepted, vec!["team@example.com".to_string()]);
    assert!(report.refused.is_empty());
    assert!(report.reply.contains("queued"), "{}", report.reply);
    assert_eq!(
        record.commands,
        vec![
            "EHLO pdfcraft.local".to_string(),
            "MAIL FROM:<pdfcraft@localhost>".to_string(),
            "RCPT TO:<team@example.com>".to_string(),
            "DATA".to_string(),
            "QUIT".to_string(),
        ]
    );
    assert!(report.log.iter().any(|line| line.contains("MAIL FROM:<pdfcraft@localhost> → 250")), "{:?}", report.log);
}

#[test]
fn crlf_mode_reaches_a_relay_that_wants_crlf() {
    let (port, receiver) = receive(Mode::Accept);
    let bytes = message();
    let report =
        send(&Server { crlf: true, ..server(port) }, "pdfcraft@localhost", &["team@example.com".to_string()], &bytes).expect("the message is sent");
    let record = receiver.join().expect("the receiver thread");

    // Every line ending is CRLF, and the final CRLF of the payload is the transfer's own marker, as
    // RFC 5321 defines it — so what the receiver keeps is the message with CRLF endings.
    let expected = String::from_utf8(bytes[..bytes.len() - 1].to_vec()).expect("the message is text").replace('\n', "\r\n").into_bytes();
    assert!(record.payload.iter().enumerate().all(|(i, b)| *b != b'\n' || (i > 0 && record.payload[i - 1] == b'\r')), "a line ending was left bare");
    assert_eq!(record.payload, expected);
    assert_eq!(report.bytes, expected.len() + 2);
}

#[test]
fn a_relay_that_refuses_every_recipient_is_never_handed_the_message() {
    let (port, receiver) = receive(Mode::RefuseRecipients);
    let err = send(&server(port), "pdfcraft@localhost", &["team@example.com".to_string()], &message()).expect_err("a refusal");
    let record = receiver.join().expect("the receiver thread");

    assert!(matches!(err, Error::Refused { .. }), "{err:?}");
    assert!(err.to_string().contains("550"), "{err}");
    assert!(!record.got_data, "the message was sent to a relay that took no recipient");
    assert!(record.payload.is_empty());
    assert_eq!(record.commands.last(), Some(&"QUIT".to_string()), "{:?}", record.commands);
    assert!(record.commands.contains(&"RSET".to_string()), "{:?}", record.commands);
}

#[test]
fn credentials_go_out_as_auth_plain_when_the_relay_offers_it() {
    let (port, receiver) = receive(Mode::WithAuth);
    let server = Server { username: Some("ada".to_string()), password: Some("hunter2".to_string()), ..server(port) };
    let report = send(&server, "pdfcraft@localhost", &["team@example.com".to_string()], &message()).expect("the message is sent");
    let record = receiver.join().expect("the receiver thread");

    assert_eq!(record.auth, vec!["AGFkYQBodW50ZXIy"], "the token is NUL user NUL password, base64");
    assert!(!report.log.iter().any(|line| line.contains("hunter2")), "the password reached the log");
    assert!(report.log.iter().any(|line| line.contains("AUTH PLAIN <credentials>")), "{:?}", report.log);
}

#[test]
fn a_relay_too_old_for_ehlo_still_gets_the_message() {
    let (port, receiver) = receive(Mode::HeloOnly);
    let bytes = message();
    let report = send(&server(port), "pdfcraft@localhost", &["team@example.com".to_string()], &bytes).expect("the message is sent");
    let record = receiver.join().expect("the receiver thread");

    assert_eq!(record.commands.first(), Some(&"EHLO pdfcraft.local".to_string()));
    assert!(record.commands.iter().any(|c| c.starts_with("HELO pdfcraft.local")), "{:?}", record.commands);
    assert_eq!(record.payload, bytes);
    assert_eq!(report.bytes, bytes.len());
}

#[test]
fn a_relay_that_is_not_there_is_an_error_not_a_panic() {
    // Bind a port and drop it again: nothing is listening on it now.
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("the port").port();
    drop(listener);
    let err = send(
        &Server { timeout: std::time::Duration::from_millis(500), ..server(port) },
        "pdfcraft@localhost",
        &["team@example.com".to_string()],
        &message(),
    )
    .expect_err("nothing is listening");
    assert!(matches!(err, Error::Connect(_)), "{err:?}");
    assert!(err.to_string().contains(&port.to_string()), "{err}");
}

#[test]
fn an_encrypted_mode_reaches_the_socket_instead_of_being_refused() {
    // The port is a live listener: a connection here is the proof that `starttls`/`tls` are carried
    // out rather than refused up front. The listener says nothing, so the handshake times out and
    // the error is a transport one (never `Config`), and no message is handed over.
    for security in [Security::ImplicitTls, Security::StartTls] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let port = listener.local_addr().expect("the port").port();
        let err = send(
            &Server { timeout: std::time::Duration::from_millis(500), security, ..server(port) },
            "pdfcraft@localhost",
            &["team@example.com".to_string()],
            &message(),
        )
        .expect_err("a relay that says nothing cannot carry an encrypted session");
        assert!(!matches!(err, Error::Config(_)), "{security:?}: {err:?}");
        match listener.accept() {
            Ok(_) => {}
            Err(e) => panic!("{security:?} opened no connection: {e}"),
        }
    }
}

/// The receiver helper itself: a client that speaks the protocol by hand reaches it, so a passing
/// test above means the client and the receiver agree about the wire, not merely that both exist.
#[test]
fn the_receiver_helper_understands_a_by_hand_client() {
    let (port, receiver) = receive(Mode::Accept);
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let mut reader = BufReader::new(stream.try_clone().expect("a second handle"));
    let mut greeting = String::new();
    reader.read_line(&mut greeting).expect("greeting");
    assert!(greeting.starts_with("220 "), "{greeting}");
    stream.write_all(b"EHLO byhand\r\n").expect("ehlo");
    let mut reply = String::new();
    reader.read_line(&mut reply).expect("ehlo reply");
    assert_eq!(reply, "250-receiver.test\r\n");
    stream.write_all(b"MAIL FROM:<a@b>\r\nRCPT TO:<c@d>\r\nDATA\r\n").expect("envelope");
    for _ in 0..3 {
        let mut line = String::new();
        reader.read_line(&mut line).expect("reply");
        assert!(line.starts_with("250 ") || line.starts_with("354 "), "{line}");
    }
    stream.write_all(b"by hand\r\n.\r\nQUIT\r\n").expect("data");
    let record = receiver.join().expect("the receiver thread");
    assert_eq!(record.payload, b"by hand");
}
