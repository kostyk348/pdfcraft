use std::sync::Arc;

use pdfcraft_cos::Document;
use pdfcraft_edit::text_lines;

use super::*;

/// A two-block page in Courier: prose, and a line that carries values a translation must not lose.
fn fixture() -> Document {
    let content = "BT /F1 12 Tf 72 700 Td (Hello world) Tj ET\n\
                   BT /F1 12 Tf 72 680 Td (Set 24 V on the 3.5 mm pin) Tj ET\n";
    let objs: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 600 800] /Resources 4 0 R >>".into(),
        "<< /Type /Page /Parent 2 0 R /Contents 5 0 R >>".into(),
        "<< /Font << /F1 6 0 R >> >>".into(),
        format!("<< /Length {} >>\nstream\n{content}endstream", content.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Courier >>".into(),
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
    out.extend_from_slice(
        format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes(),
    );
    Document::open(Arc::new(out)).unwrap()
}

fn page_text(doc: &Document) -> Vec<String> {
    text_lines(doc, 0).unwrap().into_iter().map(|l| l.text).collect()
}

/// A backend that makes the target text longer than the source on purpose, to exercise the refusal
/// path without needing a real translator.
struct Long;

impl Translator for Long {
    fn id(&self) -> String {
        "long (test backend)".into()
    }

    fn translate(&mut self, input: &[String]) -> Result<Vec<String>, String> {
        Ok(input.iter().map(|s| format!("{s} {}", "and this is a much longer expansion".repeat(6))).collect())
    }
}

/// A backend that answers in Russian, which Courier cannot draw.
struct Russian;

impl Translator for Russian {
    fn id(&self) -> String {
        "russian (test backend)".into()
    }

    fn translate(&mut self, input: &[String]) -> Result<Vec<String>, String> {
        Ok(input.iter().map(|_| "Привет мир".to_string()).collect())
    }
}

#[test]
fn the_plan_addresses_every_block_the_editor_can_reach() {
    let doc = fixture();
    let segments = plan(&doc, &Options::default()).unwrap();
    assert_eq!(segments.len(), 2, "two blocks on the page");
    assert!(segments[0].text.contains("Hello world"));
    assert!(segments[1].text.contains("24 V"));
    assert!(segments[0].rect[2] > segments[0].rect[0], "a block has a real rectangle");
}

#[test]
fn a_replacement_lands_in_place_and_does_not_move_the_line() {
    let mut doc = fixture();
    let before = text_lines(&doc, 0).unwrap();
    let mut report = translate_document(&mut doc, &mut UpperCase, &Options::default()).unwrap();
    let after = text_lines(&doc, 0).unwrap();

    assert_eq!(report.translated, 2, "both blocks were replaced");
    assert_eq!(report.overflow, 0);
    let text: Vec<String> = after.iter().map(|l| l.text.clone()).collect();
    assert!(text.iter().any(|t| t.contains("HELLO WORLD")), "text was replaced: {text:?}");
    // The block keeps its place: the same count of lines, and the same rectangle as before.
    assert_eq!(before.len(), after.len(), "line count is preserved");
    for (b, a) in before.iter().zip(after.iter()) {
        assert_eq!(b.rect, a.rect, "geometry is untouched");
    }
    report.outcomes.clear();
}

#[test]
fn a_translation_that_does_not_fit_is_refused_and_the_page_is_left_alone() {
    let mut doc = fixture();
    let before = page_text(&doc);
    let report = translate_document(&mut doc, &mut Long, &Options::default()).unwrap();
    assert_eq!(report.overflow, 2, "nothing fitted");
    assert_eq!(report.translated, 0);
    assert_eq!(page_text(&doc), before, "the page still shows the source text");
    // And the refusal is explained rather than silent.
    assert!(report.outcomes.iter().all(|o| o.fit == Fit::Overflow));
    assert!(report.outcomes.iter().all(|o| o.width_ratio.unwrap_or(0.0) > 1.0));
}

#[test]
fn overflow_can_be_forced_when_the_caller_asks_for_it() {
    let mut doc = fixture();
    let options = Options { allow_overflow: true, ..Default::default() };
    let report = translate_document(&mut doc, &mut Long, &options).unwrap();
    assert_eq!(report.translated, 2);
    assert!(report.outcomes.iter().all(|o| o.fit == Fit::FitsUnchecked));
}

#[test]
fn a_font_that_cannot_show_the_target_text_is_reported_not_hidden() {
    let mut doc = fixture();
    let report = translate_document(&mut doc, &mut Russian, &Options::default()).unwrap();
    assert!(report.translated > 0, "the text was written");
    assert!(report.substituted > 0, "the substitution was noticed: {:?}", report.outcomes);
    let substituted = report.outcomes.iter().filter_map(|o| o.substituted.clone()).collect::<Vec<_>>();
    assert!(!substituted.is_empty(), "the fallback face is named");
}

#[test]
fn labels_without_letters_are_not_sent_to_a_translator() {
    assert!(!is_translatable("12.5", 2));
    assert!(!is_translatable("—", 2));
    assert!(!is_translatable("41", 2));
    assert!(is_translatable("Fig.", 2));
    assert!(is_translatable("24 V", 2));
}

#[test]
fn every_value_of_the_source_must_survive_the_translation() {
    assert!(check::integrity("Set 24 V on the 3.5 mm pin", "Установите 24 В на выводе 3,5 мм").is_empty());
    assert!(check::integrity("50 % duty at 1 kHz", "скважность 50% при 1 кГц").is_empty());
    let lost = check::integrity("Set 24 V on the 3.5 mm pin", "Установите на выводе 3,5 мм");
    assert_eq!(lost.len(), 1, "{lost:?}");
    assert!(lost[0].contains("24v"), "{lost:?}");
    let invented = check::integrity("Set 24 V", "Установите 35 В");
    assert_eq!(invented.len(), 2, "{invented:?}");
}

#[test]
fn the_report_is_machine_readable() {
    let mut doc = fixture();
    let report = translate_document(&mut doc, &mut UpperCase, &Options::default()).unwrap();
    let json = report.to_json();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["translated"], 2);
    assert_eq!(parsed["overflow"], 0);
    assert!(parsed["outcomes"].as_array().unwrap().len() >= 2);
    assert_eq!(parsed["outcomes"][0]["fit"], "fits");
}

#[test]
fn the_deeplx_answer_is_read_and_its_refusal_is_explained() {
    let ok = backends::parse_deeplx(r#"{"code":200,"data":"Привет мир","id":1}"#).unwrap();
    assert_eq!(ok, "Привет мир");
    let refused = backends::parse_deeplx(r#"{"code":429,"message":"too many requests"}"#).unwrap_err();
    assert!(refused.contains("429") && refused.contains("too many requests"), "{refused}");
    assert!(backends::parse_deeplx("not json").is_err());
    assert!(backends::parse_deeplx(r#"{"code":200}"#).is_err());
}

#[test]
fn the_public_google_answer_is_joined_from_its_chunks() {
    // The shape the endpoint really returns: a ragged array of sentence chunks.
    let body = r#"[[["Установите ","Set ",null,null,10],["24 В","24 V",null,null,3]],null,"en",null,null,null,1.0,[]]"#;
    let text = backends::parse_google_free(body).unwrap();
    assert_eq!(text, "Установите 24 В");
    assert!(backends::parse_google_free("[]").is_err());
}

#[test]
fn the_official_google_answer_is_read_and_its_error_is_explained() {
    let ok = backends::parse_google_api(r#"{"data":{"translations":[{"translatedText":"Привет"}]}}"#).unwrap();
    assert_eq!(ok, "Привет");
    let refused = backends::parse_google_api(r#"{"error":{"code":403,"message":"API key not valid"}}"#).unwrap_err();
    assert!(refused.contains("403") && refused.contains("API key not valid"), "{refused}");
}

#[test]
fn a_query_value_survives_encoding() {
    assert_eq!(backends::urlencode("24 V на 3,5 мм"), "24%20V%20%D0%BD%D0%B0%203%2C5%20%D0%BC%D0%BC");
    assert_eq!(backends::urlencode("plain-text_1.0~"), "plain-text_1.0~");
}

#[test]
fn a_backend_is_chosen_by_name_without_touching_the_network() {
    let upper = backends::by_name("upper", "en", "ru", None).unwrap();
    assert_eq!(upper.id(), backends::by_name("upper", "en", "ru", None).unwrap().id());
    let unknown = backends::by_name("translatron", "en", "ru", None).unwrap_err();
    assert!(unknown.contains("deeplx"), "the error names the allowed backends: {unknown}");
}
