//! File ▸ Translate…: the command opens the dialog, opening it and cancelling it opens no socket,
//! and a run on the HTML fixture reports **exactly** the numbers `doc_translate` reports — compared
//! as JSON, not by eye.
//!
//! The fixture is the owner's document: a 20 pt bold heading, two body paragraphs and a bold word
//! inside one of them, created from HTML by the same `doc_create` the tools use.

mod support;

use std::path::{Path, PathBuf};

use egui_kittest::Harness;
use pdfcraft_automation::{Automation, Content};
use pdfcraft_ui_egui::translate_ui::{DEFAULT_PROVIDER, PROVIDERS};
use pdfcraft_ui_egui::{Dialog, PdfCraftApp};
use serde_json::{Value, json};

const HTML: &str = "<h1>Lost Tech Museum</h1>\
    <p>The Time Protocol is still in use.</p>\
    <p>Every entry stands on a literal quote, and <b>nothing</b> else.</p>";

const MAP: &str = "{\"Lost Tech Museum\": \"Museum der Ideen\", \
    \"The Time Protocol is still in use.\": \"Das Zeitprotokoll gilt noch.\", \
    \"Every entry stands on a literal quote, and nothing else.\": \"Jeder Eintrag fusst auf einem Zitat, nichts sonst.\"}";

fn workdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pdfcraft-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a work directory");
    dir
}

fn ok(a: &mut Automation, tool: &str, args: Value) -> Value {
    match a.call(tool, &args) {
        Ok(mut out) => match out.remove(0) {
            Content::Json(value) => value,
            other => panic!("{tool}: expected JSON, got {other:?}"),
        },
        Err(error) => panic!("{tool} {args}: {error}"),
    }
}

/// The owner's document, built from HTML by the same tool the interface gives an agent.
fn fixture(dir: &Path) -> PathBuf {
    let mut a = Automation::new();
    let doc = ok(&mut a, "doc_create", json!({ "from": "html", "html": HTML }))["doc"].as_u64().expect("a document");
    let path = dir.join("fixture.pdf");
    ok(&mut a, "doc_save", json!({ "doc": doc, "path": path.to_string_lossy() }));
    assert!(path.is_file(), "the fixture was written");
    path
}

fn harness(path: &Path) -> Harness<'static, PdfCraftApp> {
    let bytes = std::fs::read(path).expect("the fixture");
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_eframe(move |_cc| {
        let mut app = PdfCraftApp::new();
        app.set_option("language", "en").unwrap();
        app.open_bytes(&name, None, bytes.clone()).unwrap();
        app
    });
    h.run_steps(4);
    h
}

/// A draft that would reach a socket if anything ever used it: a dead endpoint on loopback. The
/// map is the source, so it must never be called.
fn draft_aimed_at_a_dead_endpoint(h: &mut Harness<'static, PdfCraftApp>, map: &Path) {
    let app = h.state_mut();
    app.translate.draft.map = map.to_string_lossy().into_owned();
    app.translate.draft.endpoint = "http://127.0.0.1:1/translate".into();
    app.translate.draft.target = "DE".into();
    app.translate_load_map();
}

#[test]
fn the_file_menu_command_opens_the_dialog_and_a_fresh_one_runs_nothing() {
    let dir = workdir("translate-dialog");
    let path = fixture(&dir);
    let mut h = harness(&path);
    assert!(h.state_mut().execute("translate.document"), "the command is registered and enabled");
    assert!(matches!(h.state().dialog, Some(Dialog::Translate)), "File ▸ Translate… opens its dialog");
    // Nothing has run: no report, no busy flag, and the document is untouched.
    assert!(h.state().translate.report.is_none());
    assert!(!h.state().translate.busy);
    assert_eq!(h.state().translate.draft.provider, DEFAULT_PROVIDER, "the offline map is the default");
    let (_, id) = h.state().active_ids().expect("the fixture is open");
    assert!(!h.state().session.get(id).expect("the document").dirty, "opening the dialog edits nothing");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_dialog_contacts_nothing_until_translate_is_pressed() {
    let dir = workdir("translate-cancel");
    let path = fixture(&dir);
    let map = dir.join("map.json");
    std::fs::write(&map, MAP).unwrap();
    let mut h = harness(&path);
    h.state_mut().execute("translate.document");
    draft_aimed_at_a_dead_endpoint(&mut h, &map);
    h.run_steps(4);
    // The map is loaded and the endpoint in the draft is dead: if anything had reached for it, the
    // run would have failed. Nothing has run at all yet.
    assert!(h.state().translate.report.is_none(), "opening the dialog and choosing a map runs nothing");
    assert!(h.state().translate.written.is_none());
    assert!(!h.state().translate.busy);
    let (_, id) = h.state().active_ids().expect("the fixture is open");
    assert!(!h.state().session.get(id).expect("the document").dirty, "and edits nothing");

    // Now translate, with the copy inside the work directory: the map answered every row, so the
    // dead endpoint was never asked.
    let copy = dir.join("translated.pdf");
    h.state_mut().translate.draft.out = copy.to_string_lossy().into_owned();
    h.state_mut().translate_run().expect("a map run needs no socket");
    h.run_steps(2);
    let report = h.state().translate.report.clone().expect("the run reported");
    assert_eq!(report.network_requests, 0, "the map answered, so the dead endpoint was never asked");
    assert_eq!(report.translated, 1, "the fixture is one paragraph of three rows: {report:?}");
    assert_eq!(h.state().translate.written.as_deref(), Some(copy.to_string_lossy().as_ref()), "the copy was written");
    assert!(copy.is_file(), "and it is a file");
    assert!(!h.state().session.get(id).expect("the document").dirty, "a copy run never edits the open document");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_interface_reports_the_same_numbers_as_the_headless_tool() {
    let dir = workdir("translate-equal");
    let path = fixture(&dir);
    let map = dir.join("map.json");
    std::fs::write(&map, MAP).unwrap();

    // 1. The interface: the map is the source, the result goes to a copy.
    let mut h = harness(&path);
    h.state_mut().execute("translate.document");
    draft_aimed_at_a_dead_endpoint(&mut h, &map);
    let copy = dir.join("translated.pdf");
    h.state_mut().translate.draft.out = copy.to_string_lossy().into_owned();
    h.state_mut().translate_run().expect("the map run");
    h.run_steps(4);
    let gui = h.state().translate.report.clone().expect("the interface's report");
    assert_eq!(h.state().translate.written.as_deref(), Some(copy.to_string_lossy().as_ref()), "the copy was written");
    assert!(copy.is_file(), "and it is a file");
    // The open document is still exactly what it was.
    let (_, id) = h.state().active_ids().expect("the fixture is open");
    assert!(!h.state().session.get(id).expect("the document").dirty);

    // 2. The tool, on the same bytes, with the same map.
    let mut a = Automation::new();
    let doc = ok(&mut a, "doc_open", json!({ "path": path.to_string_lossy() }))["doc"].as_u64().expect("a document");
    let mut headless = ok(&mut a, "doc_translate", json!({ "doc": doc, "translations": map.to_string_lossy() }));
    // The tool describes the document it ran on; the dialog has no document to describe.
    assert!(headless.as_object_mut().expect("an object").remove("document").is_some());

    assert_eq!(gui.to_json(), headless, "the dialog and the tool must report the same run");
    assert_eq!(gui.translated, 1, "{gui:?}");
    assert_eq!(gui.rows, 3, "three visual rows: {gui:?}");
    assert_eq!(gui.network_requests, 0, "{gui:?}");
    assert_eq!(gui.provider, "map", "the report names the provider that answered: {gui:?}");
    assert_eq!(gui.missing.count, 0, "{gui:?}");
    assert_eq!(gui.too_long.count, 0, "{gui:?}");
    // The third row is plain + bold + plain and this map's translation carries no <b>/<i> markup, so
    // the bold word cannot keep its run: the loss is counted and named, never smoothed over.
    assert_eq!(gui.formatting_lost.count, 1, "{gui:?}");
    assert_eq!(gui.formatting_lost.rows[0].row, Some(3), "{gui:?}");
    assert_eq!(gui.formatting_lost.rows[0].kept.as_deref(), Some("11 pt Helvetica"), "with what it kept: {gui:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_translation_is_refused_before_anything_happens_when_it_cannot_start() {
    let dir = workdir("translate-refusals");
    let path = fixture(&dir);
    let mut h = harness(&path);
    h.state_mut().execute("translate.document");
    // No map chosen: a reason, and no report.
    let reason = h.state_mut().translate_run().expect_err("a map run without a map is refused");
    assert!(reason.contains("map"), "{reason}");
    assert!(h.state().translate.report.is_none());
    // The endpoint is the network source: chosen by hand, and it needs an address.
    h.state_mut().translate.draft.provider = "deeplx";
    let reason = h.state_mut().translate_run().expect_err("the network source needs an endpoint");
    assert!(reason.contains("endpoint"), "{reason}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_dialog_keeps_the_map_as_its_provider_across_frames() {
    let dir = workdir("translate-frames");
    let path = fixture(&dir);
    let mut h = harness(&path);
    h.state_mut().execute("translate.document");
    for _ in 0..6 {
        h.run_steps(1);
        assert_eq!(h.state().translate.draft.provider, DEFAULT_PROVIDER, "drawing the dialog must not pick a provider");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_provider_picker_offers_the_map_deeplx_and_google_in_that_order() {
    // The picker is the table: a provider added to `PROVIDERS` appears in the dialog, and the rows
    // are what the run and the report read.
    let ids: Vec<&str> = PROVIDERS.iter().map(|p| p.id).collect();
    assert_eq!(ids, ["map", "deeplx", "google"], "the owner asked for these three, in this order");
    assert_eq!(PROVIDERS[0].id, DEFAULT_PROVIDER, "the offline map is the default");
    for row in PROVIDERS {
        assert!(!row.label.is_empty() && !row.hint.is_empty(), "{}: a picker row needs a label and a hint", row.id);
        assert_eq!(row.build.is_some(), row.id != "map", "{}: only a service needs a builder", row.id);
        assert_eq!(row.target, row.id != "map", "{}: a service is asked for a target language", row.id);
    }
    assert_eq!(PROVIDERS[0].kind, pdfcraft_ui_egui::translate_ui::Kind::Map, "the map cannot open a socket");
    assert_eq!(PROVIDERS[2].key, Some("API key:"), "Google takes the caller's own key");
    assert!(PROVIDERS[1].address_required, "a DeepLX service is named by its address");
    assert!(!PROVIDERS[2].address_required, "the free Google service needs no address at all");
}

#[test]
fn google_says_it_is_not_in_this_build_and_sends_nothing() {
    let dir = workdir("translate-google");
    let path = fixture(&dir);
    let mut h = harness(&path);
    h.state_mut().execute("translate.document");
    let copy = dir.join("translated.pdf");
    {
        let app = h.state_mut();
        // The free service: no address, no key, only the language — and it still sends nothing,
        // because this build has no Google backend yet.
        app.translate.draft.provider = "google";
        app.translate.draft.target = "de".into();
        app.translate.draft.out = copy.to_string_lossy().into_owned();
        app.translate_run().expect("the row accepts the fields it asks for");
    }
    h.run_steps(4);
    let error = h.state().translate.error.clone().expect("the row explains itself");
    assert!(error.contains("Google"), "{error}");
    assert!(h.state().translate.report.is_none(), "a provider that cannot run reports nothing");
    assert!(!h.state().translate.busy);
    assert!(!copy.is_file(), "and writes nothing");
    let (_, id) = h.state().active_ids().expect("the fixture is open");
    assert!(!h.state().session.get(id).expect("the document").dirty, "and edits nothing");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn replacing_in_the_open_document_is_an_undoable_edit_and_says_so() {
    let dir = workdir("translate-replace");
    let path = fixture(&dir);
    let map = dir.join("map.json");
    std::fs::write(&map, MAP).unwrap();
    let mut h = harness(&path);
    h.state_mut().execute("translate.document");
    draft_aimed_at_a_dead_endpoint(&mut h, &map);
    h.state_mut().translate.draft.replace = true;
    h.state_mut().translate_run().expect("the map run");
    h.run_steps(2);
    let report = h.state().translate.report.clone().expect("the report");
    assert_eq!(report.translated, 1, "{report:?}");
    let (_, id) = h.state().active_ids().expect("the fixture is open");
    assert!(h.state().session.get(id).expect("the document").dirty, "replacing edits the open document");
    let _ = std::fs::remove_dir_all(&dir);
}
