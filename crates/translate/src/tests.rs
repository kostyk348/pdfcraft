//! Tests for the translation backends and the fit policy. None of them needs the internet: the
//! network backend is exercised against a loopback stub.

use super::*;

#[test]
fn a_map_matches_a_paragraph_however_its_lines_broke() {
    let map = Map::parse(r#"{"Hello   world": "Hallo Welt", "Next sentence.": "Nächster Satz."}"#).expect("a valid map");
    assert_eq!(map.lookup("Hello world"), Some("Hallo Welt"));
    assert_eq!(map.lookup("  Hello\n\tworld  "), Some("Hallo Welt"));
    assert_eq!(map.lookup("Hello worlds"), None);
    assert_eq!(map.len(), 2);
    assert!(!map.is_empty());
}

#[test]
fn a_map_refuses_what_it_cannot_apply() {
    let cases: [(&str, &str); 6] = [
        ("[]", "must be a JSON object"),
        (r#"{"a": 3}"#, "is not a string"),
        (r#"{"   ": "x"}"#, "empty source string"),
        (r#"{"a": "  "}"#, "is empty"),
        (r#"{"a  b": "x", "a b": "y"}"#, "same source text with different translations"),
        ("not json", "not valid JSON"),
    ];
    for (json, expected) in cases {
        match Map::parse(json) {
            Err(Error::Invalid(message)) => assert!(message.contains(expected), "{json}: {message:?} does not name {expected:?}"),
            other => panic!("{json}: expected a refusal, got {other:?}"),
        }
    }
}

#[test]
fn a_map_accepts_one_translation_under_two_spellings() {
    let map = Map::parse(r#"{"a  b": "x", "a b": "x"}"#).expect("the same answer twice is not a conflict");
    assert_eq!(map.lookup("a b"), Some("x"));
    assert_eq!(map.len(), 1);
}

#[test]
fn a_map_refuses_an_entry_count_it_cannot_hold() {
    // One past the cap, built the cheapest way: repeated keys collapse in JSON, so distinct ones.
    let mut json = String::from("{");
    for i in 0..=MAX_MAP_ENTRIES {
        if i > 0 {
            json.push(',');
        }
        json.push_str(&format!("\"s{i}\":\"t{i}\""));
    }
    json.push('}');
    match Map::parse(&json) {
        Err(Error::Invalid(message)) => assert!(message.contains("more than"), "{message}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn candidates_step_down_from_the_source_size_to_the_floor() {
    let sizes = candidates(12.0, 5.0);
    assert_eq!(sizes.first().copied(), Some(12.0), "the source size is tried first");
    assert_eq!(sizes.last().copied(), Some(5.0), "the floor is the smallest candidate");
    assert!(sizes.len() > 3 && sizes.len() <= MAX_STEPS, "{sizes:?}");
    assert!(sizes.windows(2).all(|w| w[0] > w[1]), "largest first: {sizes:?}");
    assert!(sizes.iter().all(|s| (5.0..=12.0).contains(s)), "{sizes:?}");
}

#[test]
fn candidates_refuse_a_size_that_is_not_a_size() {
    for source in [0.0, -3.0, f64::NAN, f64::INFINITY] {
        assert!(candidates(source, 5.0).is_empty(), "{source}");
    }
    // A floor above the source cannot make the text larger than its own paragraph.
    assert_eq!(candidates(3.0, 5.0), vec![3.0]);
    assert_eq!(candidates(9.0, f64::NAN), vec![9.0], "a nonsense floor falls back to the source size");
    // A floor below what any reader could use is raised to 0.5 pt.
    let tiny = candidates(4.0, -1.0);
    assert_eq!(tiny.last().copied(), Some(0.5), "{tiny:?}");
}

#[test]
fn fit_size_picks_the_largest_candidate_that_fits() {
    // The layout is 8 pt wide at most.
    let chosen = fit_size(12.0, 5.0, |size| Ok::<bool, Error>(size <= 8.0)).expect("no failure");
    let chosen = chosen.expect("something fits");
    assert!(chosen <= 8.0 && chosen > 7.0, "{chosen} is the last candidate at or under 8 pt");
    // Candidates are measured largest first, so nothing bigger was skipped.
    assert!(!candidates(12.0, 5.0).iter().any(|s| *s > chosen && *s <= 8.0), "a larger candidate fitted too");
}

#[test]
fn fit_size_reports_when_even_the_floor_does_not_fit() {
    assert_eq!(fit_size(12.0, 5.0, |_| Ok::<bool, Error>(false)).expect("no failure"), None);
    // The floor is what gets measured last, and it is the floor the caller asked for.
    let mut measured = Vec::new();
    let _ = fit_size(12.0, 6.0, |size| {
        measured.push(size);
        Ok::<bool, Error>(false)
    })
    .expect("no failure");
    assert_eq!(measured.last().copied(), Some(6.0), "{measured:?}");
}

#[test]
fn fit_size_propagates_a_measurement_failure() {
    // A size the rewrite cannot perform at all (a face that cannot show a character) is a failure,
    // not "this size does not fit": reporting it beats quietly trying smaller sizes.
    let refused = fit_size(12.0, 5.0, |size| {
        if size <= 10.8 {
            return Err(Error::Invalid("the font cannot show U+0416".into()));
        }
        Ok(false)
    });
    match refused {
        Err(Error::Invalid(message)) => assert!(message.contains("U+0416"), "{message}"),
        other => panic!("expected the measurement failure, got {other:?}"),
    }
    // The first candidate that fits wins and nothing after it is measured.
    let mut measured = 0;
    let chosen = fit_size(12.0, 5.0, |_| {
        measured += 1;
        Ok::<bool, Error>(true)
    })
    .expect("no failure");
    assert_eq!(chosen, Some(12.0));
    assert_eq!(measured, 1);
}

/// What a stub endpoint received.
#[derive(Debug)]
struct Received {
    request_line: String,
    headers: String,
    body: String,
}

/// An endpoint on loopback that answers one request with `reply`. Returns its URL and what it
/// received, so the request itself can be asserted on.
fn stub(reply: String) -> (String, std::thread::JoinHandle<Received>) {
    use std::io::{BufRead, BufReader, Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("the port").port();
    let endpoint = format!("http://127.0.0.1:{port}/translate");
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("one connection");
        let mut reader = BufReader::new(stream.try_clone().expect("a second handle"));
        let mut request_line = String::new();
        reader.read_line(&mut request_line).expect("the request line");
        let mut headers = String::new();
        let mut length = 0_usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).expect("a header line") == 0 || line.trim_end().is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().expect("a content length");
            }
            headers.push_str(&line);
        }
        let mut body = vec![0_u8; length];
        reader.read_exact(&mut body).expect("the body");
        let _ = stream.write_all(reply.as_bytes());
        let _ = stream.flush();
        Received { request_line: request_line.trim_end().to_owned(), headers, body: String::from_utf8(body).expect("a UTF-8 request") }
    });
    (endpoint, handle)
}

/// An HTTP/1.1 answer with `body` and the status/headers a DeepLX endpoint sends.
fn answer(status: &str, body: &str) -> String {
    format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

#[test]
fn the_network_backend_sends_one_deeplx_request_and_reuses_its_answer() {
    let (endpoint, server) = stub(answer("200 OK", r#"{"code":200,"message":"","data":"Hallo Welt"}"#));
    let mut translator = Deeplx::new(&endpoint, "auto", "DE", 5.0).expect("a valid endpoint");
    assert_eq!(translator.endpoint(), endpoint);
    assert_eq!(translator.requests(), 0, "building it makes no request");
    assert_eq!(translator.translate("Hello world").expect("an answer"), Some("Hallo Welt".into()));
    assert_eq!(translator.translate("Hello world").expect("a cached answer"), Some("Hallo Welt".into()));
    assert_eq!(translator.requests(), 1, "the repeated sentence cost no second request");
    let received = server.join().expect("the stub finished");
    assert_eq!(received.request_line, "POST /translate HTTP/1.1", "{received:?}",);
    assert!(received.headers.to_ascii_lowercase().contains("content-type: application/json"), "{}", received.headers);
    let sent: serde_json::Value = serde_json::from_str(&received.body).expect("the request body is JSON");
    assert_eq!(sent["text"], "Hello world");
    assert_eq!(sent["source_lang"], "auto");
    assert_eq!(sent["target_lang"], "DE");
}

#[test]
fn the_network_backend_reports_a_refusal_with_its_message() {
    let (endpoint, server) = stub(answer("200 OK", r#"{"code":237,"message":"Quota exceeded","data":""}"#));
    let mut translator = Deeplx::new(&endpoint, "EN", "RU", 5.0).expect("a valid endpoint");
    match translator.translate("Hello") {
        Err(Error::Network(message)) => assert!(message.contains("Quota exceeded") && message.contains("237"), "{message}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    server.join().expect("the stub finished");
}

#[test]
fn the_network_backend_reports_an_answer_without_a_translation() {
    let (endpoint, server) = stub(answer("200 OK", r#"{"code":200,"data":""}"#));
    let mut translator = Deeplx::new(&endpoint, "EN", "RU", 5.0).expect("a valid endpoint");
    match translator.translate("Hello") {
        Err(Error::Network(message)) => assert!(message.contains("empty translation"), "{message}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    server.join().expect("the stub finished");

    let (endpoint, server) = stub(answer("500 Internal Server Error", "upstream fell over"));
    let mut translator = Deeplx::new(&endpoint, "EN", "RU", 5.0).expect("a valid endpoint");
    match translator.translate("Hello") {
        Err(Error::Network(message)) => assert!(message.contains("HTTP 500") && message.contains("upstream fell over"), "{message}"),
        other => panic!("expected a failure, got {other:?}"),
    }
    server.join().expect("the stub finished");
}

#[test]
fn the_network_backend_requires_one_http_endpoint_and_real_language_codes() {
    for endpoint in ["", "   ", "localhost:1188/translate", "ftp://example.test/translate", "http://", "http://exa mple.test/t"] {
        match Deeplx::new(endpoint, "EN", "RU", 5.0) {
            Err(Error::Invalid(_)) => {}
            other => panic!("{endpoint:?} should be refused, got {:?}", other.map(|_| "a translator")),
        }
    }
    for (source, target) in [("", "RU"), ("EN", ""), ("E N", "RU"), ("EN", "R U"), ("EN", "RU; DROP TABLE")] {
        let message = Deeplx::new("http://127.0.0.1:1/translate", source, target, 5.0).err().map(|e| e.to_string());
        assert!(message.is_some(), "{source:?}/{target:?} should be refused");
    }
    // The endpoint is validated but never contacted while building the translator.
    assert!(Deeplx::new("http://127.0.0.1:1/translate", "EN", "RU", 5.0).is_ok());
}
