use super::*;

#[test]
fn sha256_matches_the_published_vectors() {
    assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    assert_eq!(
        sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
    // A multi-block input, so the length padding and the block loop are both exercised.
    assert_eq!(sha256_hex(&vec![b'a'; 1_000_000]), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
}

#[test]
fn folded_and_encoded_headers_are_unfolded_and_decoded() {
    let raw = b"From: Ada <ada@example.com>\r\nSubject: Hello,\r\n world\r\nX-Long: a\r\n\tb\r\n\r\nbody\n";
    let email = Email::parse(raw);
    assert_eq!(email.headers.iter().find(|(k, _)| k == "Subject").map(|(_, v)| v.as_str()), Some("Hello, world"));
    let joined = &email.headers.iter().find(|(k, _)| k == "X-Long").map(|(_, v)| v.clone()).unwrap_or_default();
    assert_eq!(joined, "a b");

    let encoded = Email::parse(b"Subject: =?utf-8?B?SGVsbG8sIHdvcmxk?=\n\nx");
    assert_eq!(encoded.subject, "Hello, world");

    let q = Email::parse(b"Subject: =?utf-8?Q?Caf=C3=A9_r=C3=A9sum=C3=A9?=\n\nx");
    assert_eq!(q.subject, "Café résumé");
}

#[test]
fn multipart_alternative_prefers_html_and_html_becomes_text() {
    let raw = br#"From: a@example.com
To: b@example.com, "Doe, Jane" <jane@example.com>
Subject: Update
MIME-Version: 1.0
Content-Type: multipart/alternative; boundary="b1"

--b1
Content-Type: text/plain; charset=utf-8

plain fallback
--b1
Content-Type: text/html; charset=utf-8

<html><body><p>Hello <b>bold</b> world.</p><p>A &amp; B</p><script>ignore()</script></body></html>
--b1--
"#;
    let email = Email::parse(raw);
    assert!(matches!(email.body, Some(Body::Html(_))));
    assert_eq!(email.to, vec!["b@example.com", "\"Doe, Jane\" <jane@example.com>"]);
    let doc = email.to_document();
    let text = doc.body_text();
    assert!(text.contains("Hello bold world."), "{text}");
    assert!(text.contains("A & B"), "{text}");
    assert!(!text.contains('<'), "{text}");
    assert!(!text.contains("ignore"), "{text}");
    assert_eq!(doc.title, "Update");
    assert!(doc.headers.iter().any(|(k, v)| k == "From" && v == "Ada" || k == "From"));
}

#[test]
fn nested_parts_and_a_base64_attachment_are_read() {
    let raw = br#"From: reports@example.com
To: team@example.com
Subject: Q3
Content-Type: multipart/mixed; boundary="outer"

--outer
Content-Type: multipart/alternative; boundary="inner"

--inner
Content-Type: text/plain

See attached.
--inner
Content-Type: text/html

<p>See attached.</p>
--inner--
--outer
Content-Type: application/pdf; name="report.pdf"
Content-Disposition: attachment; filename="=?utf-8?B?cmVwb3J0LnBkZg==?="
Content-Transfer-Encoding: base64

JVBERi0xLjQK
--outer--
"#;
    let email = Email::parse(raw);
    assert_eq!(email.subject, "Q3");
    assert_eq!(email.from, vec!["reports@example.com"]);
    assert_eq!(email.attachments.len(), 1);
    let a = &email.attachments[0];
    assert_eq!(a.filename, "report.pdf");
    assert_eq!(a.media_type, "application/pdf");
    assert!(!a.inline);
    assert_eq!(a.bytes, b"%PDF-1.4\n");
    assert_eq!(email.to_document().attachment_lines().len(), 1);
}

#[test]
fn quoted_printable_bodies_and_soft_breaks_decode() {
    let raw = b"Content-Type: text/plain; charset=iso-8859-1\nContent-Transfer-Encoding: quoted-printable\n\ncaf=E9 and a soft=\nbreak\n";
    let email = Email::parse(raw);
    match email.body {
        Some(Body::Plain(text)) => assert_eq!(text, "café and a softbreak\n"),
        other => panic!("expected plain text, got {other:?}"),
    }
}

#[test]
fn detection_is_conservative() {
    assert!(Email::is_likely("note.eml", b"anything"));
    assert!(Email::is_likely("m", b"Received: from x\r\nFrom: a@b\r\n\r\n"));
    assert!(Email::is_likely("m", b"MIME-Version: 1.0\n\n"));
    assert!(!Email::is_likely("note.txt", b"hello\nworld"));
    assert!(!Email::is_likely("a.bin", b"\x00\x01\x02"));
    assert!(!Email::is_likely("From the desk of X", b"From the desk of X\n"));
}

#[test]
fn a_composed_html_message_with_an_inline_page_and_a_pdf_round_trips() {
    let pdf = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\nrest".to_vec();
    let page = vec![0x89u8, 0x50, 0x4e, 0x47, 1, 2, 3, 4];
    let mut message = Message::new("PdfCraft <pdfcraft@localhost>", "team@example.com", "Отчёт — Q3", "Report attached.");
    message.html("<p>Report:</p><p><img src=\"cid:page-1\"></p>");
    message.embed("page-1.png", "image/png", "page-1", page.clone());
    message.attach("report.pdf", "application/pdf", pdf.clone());

    let bytes = message.render();
    // Deterministic: the same message renders the same bytes, so a receipt can hash it.
    assert_eq!(message.render(), bytes);

    let email = Email::parse(&bytes);
    assert_eq!(email.subject, "Отчёт — Q3");
    assert_eq!(email.to, vec!["team@example.com"]);
    assert!(matches!(email.body, Some(Body::Html(ref h)) if h.contains("cid:page-1")), "{:?}", email.body);
    let pdf_part = email.attachments.iter().find(|a| a.media_type == "application/pdf").expect("a pdf part");
    assert_eq!(pdf_part.bytes, pdf);
    let image = email.attachments.iter().find(|a| a.inline).expect("an inline image");
    assert_eq!(image.bytes, page);
    assert_eq!(image.content_id.as_deref(), Some("page-1"));
    // An inline body image is not an attachment of the document.
    let doc = email.to_document();
    assert_eq!(doc.attachments.len(), 1);
    assert_eq!(doc.attachments[0].filename, "report.pdf");
}

#[test]
fn a_hostile_message_never_panics() {
    // Empty, a lone boundary, an unterminated multipart, a boundary with no parameter, a header
    // block with no body, and a body of NULs: each is read as far as it can be and no further.
    for raw in [
        &b""[..],
        &b"--\n--\n"[..],
        &b"Content-Type: multipart/mixed; boundary=\"x\"\n\n--x\nContent-Type: text/plain\n\nhi"[..],
        &b"Content-Type: multipart/mixed\n\njunk"[..],
        &b"Subject: no body"[..],
        &b"\x00\x00\x00\x00"[..],
    ] {
        let email = Email::parse(raw);
        let _ = email.to_document().body_text();
    }
}

#[test]
fn a_receipt_round_trips_and_verifies() {
    let mut receipt = Receipt::new("combine", "2026-10-13T00:00:00Z");
    receipt.input("in/a.pdf", b"one");
    receipt.input("weird |name\\x.eml", b"two");
    receipt.output("out/combined.pdf", b"three");
    let bytes = receipt.render();
    let parsed = Receipt::parse(&bytes);
    assert_eq!(parsed.op, "combine");
    assert_eq!(parsed.timestamp, "2026-10-13T00:00:00Z");
    assert_eq!(parsed.inputs.len(), 2);
    assert_eq!(parsed.outputs.len(), 1);
    assert_eq!(parsed.inputs[1].path, "weird |name\\x.eml");

    let files = |path: &str| match path {
        "in/a.pdf" => Some(b"one".to_vec()),
        "weird |name\\x.eml" => Some(b"two".to_vec()),
        "out/combined.pdf" => Some(b"three".to_vec()),
        _ => None,
    };
    assert_eq!(parsed.verify(files).unwrap(), 3);

    let tampered = |path: &str| if path == "out/combined.pdf" { Some(b"THREE".to_vec()) } else { files(path) };
    assert!(parsed.verify(tampered).is_err());

    let missing = |path: &str| if path == "in/a.pdf" { None } else { files(path) };
    assert!(parsed.verify(missing).is_err());
}
