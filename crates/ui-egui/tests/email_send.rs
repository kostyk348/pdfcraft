//! File ▸ Send as email…: the dialog asks for a relay and an envelope, opens no socket until Send
//! is pressed, and then delivers **the same bytes** `doc_email`/`Save as email message…` compose.
//!
//! Everything here is a loopback socket in this file: no relay, no internet, no port outside
//! `127.0.0.1`.

mod support;

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use pdfcraft_engine::EmailOptions;
use pdfcraft_ui_egui::email_send::{Outcome, Transport};
use pdfcraft_ui_egui::{Dialog, PdfCraftApp};

/// A one-page PDF with a little text, so the message has a page to render.
fn fixture() -> Vec<u8> {
    let objs = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 300] >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 300] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_string(),
        "<< /Length 47 >>\nstream\nBT /F1 24 Tf 20 150 Td (Quarterly report) Tj ET\nendstream".to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
    out
}

fn harness() -> Harness<'static, PdfCraftApp> {
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_eframe(|_cc| {
        let mut app = PdfCraftApp::new();
        app.set_option("language", "en").unwrap();
        app.open_bytes("doc.pdf", None, fixture()).unwrap();
        app
    });
    h.run_steps(4);
    h
}

/// What the receiving relay kept.
#[derive(Default, Debug)]
struct Record {
    commands: Vec<String>,
    payload: Vec<u8>,
}

/// A receiving relay on loopback that accepts one session and keeps the message it is handed.
fn receive() -> (u16, JoinHandle<Record>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("the port").port();
    (port, std::thread::spawn(move || serve(listener)))
}

fn serve(listener: TcpListener) -> Record {
    let Ok((stream, _)) = listener.accept() else { return Record::default() };
    let mut writer = stream;
    let mut reader = BufReader::new(writer.try_clone().expect("a second handle"));
    let _ = writer.write_all(b"220 receiver.test ESMTP pdfcraft-smtp-test\r\n");
    let mut record = Record::default();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return record;
        }
        let command = line.trim_end_matches(['\r', '\n']).to_string();
        record.commands.push(command.clone());
        let upper = command.to_ascii_uppercase();
        let reply: &[u8] = if upper.starts_with("EHLO") {
            // RFC 5321 §4.2.1: a multi-line reply continues with `250-` and ends with `250 `.
            b"250-receiver.test\r\n250 SIZE 10485760\r\n"
        } else if upper.starts_with("MAIL FROM") {
            b"250 2.1.0 sender ok\r\n"
        } else if upper.starts_with("RCPT TO") {
            b"250 2.1.5 recipient ok\r\n"
        } else if upper == "DATA" {
            b"354 go ahead\r\n"
        } else if upper == "QUIT" {
            let _ = writer.write_all(b"221 2.0.0 bye\r\n");
            return record;
        } else {
            b"500 5.5.1 what?\r\n"
        };
        if writer.write_all(reply).is_err() {
            return record;
        }
        if upper == "DATA" {
            let mut body = Vec::new();
            loop {
                let mut raw = Vec::new();
                match reader.read_until(b'\n', &mut raw) {
                    Ok(0) | Err(_) => return record,
                    Ok(_) => {}
                }
                if raw == b".\r\n" {
                    break;
                }
                body.extend_from_slice(&raw);
            }
            record.payload = body;
            // The transfer's end-of-data marker is `CRLF.CRLF`: the CRLF in front of the dot is the
            // framing a relay strips, not part of the message (RFC 5321 §4.1.1.4).
            if record.payload.ends_with(b"\r\n") {
                record.payload.truncate(record.payload.len() - 2);
            }
            if writer.write_all(b"250 2.0.0 message accepted\r\n").is_err() {
                return record;
            }
        }
    }
}

/// Fill the dialog in: a relay on this machine, and an envelope the client can send.
fn fill(h: &mut Harness<'static, PdfCraftApp>, port: u16, transport: Transport) {
    let draft = &mut h.state_mut().email.draft;
    draft.host = "127.0.0.1".to_string();
    draft.port = port.to_string();
    draft.transport = transport;
    draft.from = "ada@example.com".to_string();
    draft.to = "team@example.com".to_string();
    draft.cc = String::new();
    draft.subject = "Quarterly report".to_string();
    draft.body = "See the attached report.".to_string();
}

/// The message the dialog sends: exactly what `doc_email` writes with the same options.
fn expected(h: &Harness<'static, PdfCraftApp>) -> Vec<u8> {
    let (_, id) = h.state().active_ids().expect("an open document");
    let options = EmailOptions {
        from: "ada@example.com".to_string(),
        to: "team@example.com".to_string(),
        cc: String::new(),
        subject: "Quarterly report".to_string(),
        body: "See the attached report.".to_string(),
        dpi: 96.0,
    };
    h.state().session.email_message(id, &options).expect("the .eml")
}

#[test]
fn the_command_opens_the_dialog_and_cancelling_it_connects_to_nothing() {
    // A live listener: if the dialog opened a socket, this would see it.
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("the port").port();
    listener.set_nonblocking(true).expect("nonblocking");

    let mut h = harness();
    assert!(h.state_mut().execute("email.send"), "email.send is registered and enabled with a document open");
    assert!(matches!(h.state().dialog, Some(Dialog::EmailSend)), "{:?}", h.state().dialog);
    fill(&mut h, port, Transport::None);
    h.run_steps(2);

    h.get_by_label("Cancel").click();
    h.run_steps(2);
    assert!(h.state().dialog.is_none(), "Cancel closes the dialog");
    assert!(matches!(listener.accept(), Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock), "cancelling opened a connection");
    // Nothing was attempted, so there is no outcome and nothing was said about one.
    assert!(h.state().email.outcome.is_none());
    let toast = h.state().toast.as_ref().map(|t| t.0.clone()).unwrap_or_default();
    assert!(!toast.contains("sent") && !toast.contains("Not sent"), "{toast}");
}

#[test]
fn sending_delivers_the_eml_byte_for_byte() {
    let (port, receiver) = receive();
    let mut h = harness();
    let want = expected(&h);
    assert!(h.state_mut().execute("email.send"));
    fill(&mut h, port, Transport::None);
    h.run_steps(2);

    h.get_by_label("Send").click();
    support::wait_until(&mut h, "the session to finish", |app| app.email.outcome.is_some());
    h.run_steps(1);

    let outcome = h.state().email.outcome.clone().expect("the outcome");
    match outcome {
        Outcome::Delivered { accepted, refused, bytes, .. } => {
            assert_eq!(accepted, vec!["team@example.com".to_string()]);
            assert!(refused.is_empty(), "{refused:?}");
            assert_eq!(bytes, want.len());
        }
        Outcome::Failed(reason) => panic!("the message was not delivered: {reason}"),
    }
    let toast = h.state().toast.as_ref().map(|t| t.0.clone()).unwrap_or_default();
    assert!(toast.starts_with("Message sent to 127.0.0.1:"), "{toast}");

    let record = receiver.join().expect("the receiver finished");
    // Compare lengths and the first differing byte rather than dumping megabytes on failure.
    if record.payload != want {
        let at = record.payload.iter().zip(want.iter()).position(|(a, b)| a != b);
        panic!(
            "the receiver kept exactly the .eml bytes (same SHA-256): got {} bytes, want {} bytes, first difference at {:?}",
            record.payload.len(),
            want.len(),
            at
        );
    }
    assert!(record.commands.iter().any(|c| c == "MAIL FROM:<ada@example.com>"), "{:?}", record.commands);
    assert!(record.commands.iter().any(|c| c == "RCPT TO:<team@example.com>"), "{:?}", record.commands);
}

#[test]
fn a_certificate_that_does_not_check_out_is_reported_and_nothing_is_delivered() {
    // The relay here speaks plain SMTP: a TLS client cannot make anything of its greeting, which is
    // the same thing a user sees when a real relay presents a certificate that does not check out.
    let (port, receiver) = receive();
    let mut h = harness();
    assert!(h.state_mut().execute("email.send"));
    fill(&mut h, port, Transport::Tls);
    h.run_steps(2);

    h.get_by_label("Send").click();
    support::wait_until(&mut h, "the session to give up", |app| app.email.outcome.is_some());
    h.run_steps(1);

    match h.state().email.outcome.clone().expect("the outcome") {
        Outcome::Failed(reason) => {
            assert!(reason.contains("encrypted session") || reason.contains("certificate"), "{reason}");
            assert!(reason.contains("nothing was sent") || reason.contains("could not be set up"), "{reason}");
        }
        Outcome::Delivered { .. } => panic!("a plain relay must not receive a TLS message"),
    }
    let toast = h.state().toast.as_ref().map(|t| t.0.clone()).unwrap_or_default();
    assert!(toast.starts_with("Not sent:"), "the failure is shown, not swallowed: {toast}");
    let record = receiver.join().expect("the receiver finished");
    assert!(record.payload.is_empty(), "the message was not handed over: {:?}", record.commands);
}

#[test]
fn an_unusable_envelope_keeps_the_dialog_open_and_opens_no_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("the port").port();
    listener.set_nonblocking(true).expect("nonblocking");
    let mut h = harness();
    assert!(h.state_mut().execute("email.send"));
    fill(&mut h, port, Transport::None);
    h.state_mut().email.draft.to = "not an address".to_string();
    h.run_steps(2);

    h.get_by_label("Send").click();
    h.run_steps(2);
    assert!(matches!(h.state().dialog, Some(Dialog::EmailSend)), "the dialog stays open to be corrected");
    assert!(h.state().email.outcome.is_none(), "nothing was attempted");
    let toast = h.state().toast.as_ref().map(|t| t.0.clone()).unwrap_or_default();
    assert!(toast.contains("recipient"), "the reason is shown: {toast}");
    assert!(matches!(listener.accept(), Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock), "a bad address opened a connection");
}

#[test]
fn a_password_is_never_put_in_the_diagnostic_form_of_the_draft() {
    let draft = pdfcraft_ui_egui::email_send::Draft {
        username: "ada".to_string(),
        password: "hunter2".to_string(),
        ..Default::default()
    };
    let shown = format!("{draft:?}");
    assert!(!shown.contains("hunter2"), "{shown}");
    assert!(shown.contains("<hidden>"), "{shown}");
}

/// The relay is spoken to only from the dialog's Send button: a client for it still works, which
/// proves the receiver above is a real SMTP peer rather than something that agrees with itself.
#[test]
fn the_receiver_helper_understands_a_by_hand_client() {
    let (port, receiver) = receive();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let mut reader = BufReader::new(stream.try_clone().expect("a second handle"));
    let mut greeting = String::new();
    reader.read_line(&mut greeting).expect("greeting");
    assert!(greeting.starts_with("220 "), "{greeting}");
    stream.write_all(b"EHLO byhand\r\nMAIL FROM:<a@b>\r\nRCPT TO:<c@d>\r\nDATA\r\n").expect("envelope");
    // Two lines for the `EHLO` reply (RFC 5321 §4.2.1), then one each for `MAIL`, `RCPT`, `DATA`.
    for _ in 0..5 {
        let mut line = String::new();
        reader.read_line(&mut line).expect("reply");
        assert!(line.starts_with("250") || line.starts_with("354"), "{line}");
    }
    stream.write_all(b"by hand\r\n.\r\nQUIT\r\n").expect("data");
    let record = receiver.join().expect("the receiver thread");
    // The CRLF in front of the `.` is the framing, so the relay keeps the line as sent.
    assert_eq!(record.payload, b"by hand");
}
