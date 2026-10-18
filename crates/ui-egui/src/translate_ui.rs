//! File ▸ Translate…: the interface's half of the translator.
//!
//! The run itself is [`pdfcraft_engine::translate`] — the same code `doc_translate` in
//! `pdfcraft-automation` runs (AGENTS.md §3), so the numbers here and the tool's answer are built
//! from one report and cannot disagree.
//!
//! Three providers, as the owner asked for, and they are rows of one table ([`PROVIDERS`]) rather
//! than a `match` in every corner of the dialog:
//!
//! - a **translation map**, a JSON object of source string → translation. It is the default: it
//!   needs no key, opens no socket and is exactly reproducible.
//! - a **DeepLX-compatible service**.
//! - **Google**, whose own backend (batches, 429, cache) is its own card (`t_ee9c7f34`). Its row
//!   holds its place and says so plainly instead of pretending to translate or quietly asking
//!   another provider.
//!
//! The services are **off by default**: a request leaves the computer only when the user picks a
//! service and presses Translate. The report names the provider and counts the requests, so "did
//! this go to the network" is a number, not a guess. A service run collects its translations off
//! the frame thread, so a slow endpoint cannot freeze the window.
//!
//! Where the result goes is a choice, and the safe one is the default: **a copy**, written to a new
//! file, leaving the document you have open exactly as it was — not even its edits. "Replace the
//! text in this document" applies the run to the open document instead, one undo step per
//! paragraph, as the tool does.

use std::collections::BTreeMap;
use std::sync::Arc;

use egui::{Align, Layout};
use pdfcraft_engine::translate::{self, BackendError, Options, Report};
use pdfcraft_engine::{DocId, Edit, Session};
use pdfcraft_translate as tr;
use pdfcraft_translate::Translator as _;

use crate::pickers::Ask;
use crate::theme::{self, Tokens};
use crate::{Dialog, PdfCraftApp, widgets};

/// What the dialog paints when a run refused or failed (the same red the rest of the app uses).
const ERROR: egui::Color32 = egui::Color32::from_rgb(0xD7, 0x37, 0x3F);

/// What a provider's run reads: a JSON map on this computer, or a service asked over a socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A JSON map read here. No socket exists on this path, so a run cannot reach the network.
    Map,
    /// A service. It is asked off the frame thread, and only after Translate was pressed.
    Service,
}

/// What a provider's builder sees: the user's fields, borrowed for the building.
pub struct Request<'a> {
    /// The service address the user typed (`""` when the provider asks one and none is given).
    pub endpoint: &'a str,
    /// The key the user typed, or `""`. Only a provider that needs one reads it.
    pub key: &'a str,
    /// The source language, or `""` for the service's own detection.
    pub source: &'a str,
    /// The target language, or `""` when the provider does not ask for one.
    pub target: &'a str,
}

/// One provider of translations: a row of [`PROVIDERS`], and the only place a provider is spelled
/// out. Adding one is a row here plus its builder — the dialog, the report and the tests read the
/// table instead of matching on names.
pub struct Provider {
    /// The value kept in the draft, and the name the report shows.
    pub id: &'static str,
    /// The label the picker shows: an English key, translated at render time.
    pub label: &'static str,
    pub kind: Kind,
    /// The label of the service address field; `None` when this provider has no address.
    pub address: Option<&'static str>,
    /// The address must be filled in. False for a provider whose free service needs none.
    pub address_required: bool,
    /// The provider is asked for a target language.
    pub target: bool,
    /// The label of the key field; `None` when this provider has no key.
    pub key: Option<&'static str>,
    /// What the dialog says before anything runs: an English key.
    pub hint: &'static str,
    /// Build the service's backend. `Err` is a reason for the dialog — what the provider still
    /// needs, or that this build cannot ask it. Nothing has been sent when it returns.
    pub build: Option<fn(&Request<'_>) -> Result<Box<dyn tr::Translator + Send>, String>>,
}

/// The offline map: the default, and the only provider that cannot open a socket.
pub const MAP: Provider = Provider {
    id: "map",
    label: "Translation map (JSON file)",
    kind: Kind::Map,
    address: None,
    address_required: false,
    target: false,
    key: None,
    hint: "The map answers from a JSON file on this computer; nothing is sent.",
    build: None,
};

/// A DeepLX-compatible service, named by the user.
pub const DEEPLX: Provider = Provider {
    id: "deeplx",
    label: "DeepLX-compatible service",
    kind: Kind::Service,
    address: Some("Endpoint:"),
    address_required: true,
    target: true,
    key: None,
    hint: "Nothing is sent until you press Translate, and the report counts every request.",
    build: Some(deeplx),
};

/// Google, whose own backend (its requests, batching, 429 handling and cache) is card
/// `t_ee9c7f34` and not part of this build: the row is here so the provider is one table entry to
/// fill in, and until then it refuses with a reason instead of inventing a translation.
pub const GOOGLE: Provider = Provider {
    id: "google",
    label: "Google Translate",
    kind: Kind::Service,
    address: Some("Service address:"),
    address_required: false,
    target: true,
    key: Some("API key:"),
    hint: "Leave the address empty for the free service; a google-cloud address is used with your own key. Nothing is sent until you press Translate.",
    build: Some(google),
};

/// Every provider the dialog offers, in order. The first row is the default.
pub static PROVIDERS: &[Provider] = &[MAP, DEEPLX, GOOGLE];

/// The provider a fresh dialog starts with: the offline map.
pub const DEFAULT_PROVIDER: &str = MAP.id;

/// The row an id names, or the default row when the id is unknown (a draft from an older build
/// must not leave the dialog with no provider at all).
pub fn provider(id: &str) -> &'static Provider {
    PROVIDERS.iter().find(|p| p.id == id).unwrap_or(&MAP)
}

/// The DeepLX row: the endpoint the user named, and the languages to ask it for. Building it
/// contacts nothing.
fn deeplx(request: &Request<'_>) -> Result<Box<dyn tr::Translator + Send>, String> {
    let backend = tr::Deeplx::new(request.endpoint, request.source, request.target, tr::DEFAULT_TIMEOUT_SECS).map_err(|error| error.to_string())?;
    Ok(Box::new(backend))
}

/// The Google row. Its requests, batching, 429 handling and cache belong to `t_ee9c7f34`; until
/// that lands this row replaces nothing and asks no one, which is what it says.
fn google(_request: &Request<'_>) -> Result<Box<dyn tr::Translator + Send>, String> {
    Err(tl!("Google Translate's backend is not in this build yet, so nothing was sent. Choose the map or a DeepLX-compatible service.").to_string())
}

/// What the dialog holds. The provider, the map, its fields and the font are the user's choice;
/// nothing is taken from an earlier session, and nothing here opens a socket on its own.
#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    /// The provider's id ([`PROVIDERS`]); the offline map until the user picks another.
    pub provider: &'static str,
    /// The path of the JSON translation map.
    pub map: String,
    /// The service address, e.g. `http://127.0.0.1:1188/translate`.
    pub endpoint: String,
    /// A key the service needs (a google-cloud address and nothing else); empty for a free service.
    pub key: String,
    /// The language code the service is asked for (`DE`, `RU`, …).
    pub target: String,
    /// The source language, or empty for the service's own detection.
    pub source_language: String,
    /// A font file loaded into the document for a target script its own faces lack (Cyrillic).
    pub font: String,
    /// Apply the run to the open document instead of writing a copy. Off by default.
    pub replace: bool,
    /// Where the copy goes. Filled in when the dialog opens, and editable so a script (and the
    /// control channel) can name it.
    pub out: String,
}

impl Default for Draft {
    fn default() -> Self {
        Self {
            provider: DEFAULT_PROVIDER,
            map: String::new(),
            endpoint: String::new(),
            key: String::new(),
            target: String::new(),
            source_language: String::new(),
            font: String::new(),
            replace: false,
            out: String::new(),
        }
    }
}

/// A map that answers from memory: what the offline source is.
struct MapBackend(tr::Map);

impl translate::Backend for MapBackend {
    fn provider(&self) -> &'static str {
        MAP.id
    }

    fn translate(&mut self, text: &str) -> Result<Option<String>, BackendError> {
        self.0.translate(text).map_err(|e| BackendError::Invalid(e.to_string()))
    }
}

/// Answers already collected from a service, so the run itself sends nothing.
struct Collected {
    translations: BTreeMap<String, Option<String>>,
    requests: usize,
    /// The row the answers came from, so the report says who was asked.
    provider: &'static str,
}

impl translate::Backend for Collected {
    fn provider(&self) -> &'static str {
        self.provider
    }

    fn translate(&mut self, text: &str) -> Result<Option<String>, BackendError> {
        Ok(self.translations.get(text).cloned().flatten())
    }

    fn requests(&self) -> usize {
        self.requests
    }
}

/// What the worker thread brings back from a service.
struct Fetched {
    translations: BTreeMap<String, Option<String>>,
    requests: usize,
    provider: &'static str,
    error: Option<String>,
}

/// The user's fields, owned, so they can cross to the worker thread.
#[derive(Clone)]
struct Fields {
    endpoint: String,
    key: String,
    source: String,
    target: String,
}

impl Fields {
    fn borrow(&self) -> Request<'_> {
        Request { endpoint: &self.endpoint, key: &self.key, source: &self.source, target: &self.target }
    }
}

/// The dialog's state and the run it started.
#[derive(Default)]
pub struct Translate {
    pub draft: Draft,
    /// The parsed map, for the preview.
    pub map: Option<tr::Map>,
    pub map_error: Option<String>,
    /// How much of the document the map covers (no edit, no request).
    pub survey: Option<translate::Survey>,
    pub report: Option<Report>,
    /// The last reason a run refused or failed, in the interface language.
    pub error: Option<String>,
    /// Where the copy was written.
    pub written: Option<String>,
    pub busy: bool,
    running: Option<std::sync::mpsc::Receiver<Fetched>>,
    /// The copy's bytes, waiting for the save path.
    pending: Option<Arc<Vec<u8>>>,
    /// The document the dialog was opened on, so a pick made later still refers to it.
    pub doc: Option<DocId>,
}

impl PdfCraftApp {
    /// File ▸ Translate…: open the dialog. Opening it contacts nothing.
    pub(crate) fn translate_dialog(&mut self) {
        let Some((_, id)) = self.active_ids() else { return };
        self.translate.doc = Some(id);
        self.translate.error = None;
        self.translate.report = None;
        self.translate.written = None;
        // The copy's name is suggested from the document and can be changed before the run; it is
        // never taken from a setting, and it is never the document's own file.
        if self.translate.draft.out.trim().is_empty() {
            self.translate.draft.out = self.suggested_copy_name(id);
        }
        self.refresh_translate_preview();
        self.dialog = Some(Dialog::Translate);
    }

    /// `somewhere/<name> (translated).pdf`, next to the document (or in the export folder a test or
    /// the automation named).
    fn suggested_copy_name(&self, id: DocId) -> String {
        let name = self.session.get(id).map(|d| d.name.clone()).unwrap_or_else(|| "document.pdf".to_string());
        let stem = name.strip_suffix(".pdf").unwrap_or(name.as_str()).to_string();
        let file = format!("{stem} (translated).pdf");
        let dir = self
            .export_dir_override
            .clone()
            .or_else(|| {
                self.session
                    .get(id)
                    .and_then(|d| d.path.clone())
                    .and_then(|p| std::path::Path::new(&p).parent().map(|d| d.to_string_lossy().into_owned()))
            })
            .unwrap_or_default();
        if dir.is_empty() { file } else { std::path::Path::new(&dir).join(file).to_string_lossy().into_owned() }
    }

    /// Recount how much of the open document the chosen map covers. A pure lookup: no request.
    pub fn refresh_translate_preview(&mut self) {
        self.translate.survey = None;
        let Some(id) = self.translate.doc else { return };
        let (Some(map), Some(doc)) = (self.translate.map.as_ref(), self.session.get(id)) else { return };
        self.translate.survey = Some(translate::survey(doc, &[], |text| map.lookup(text).is_some()));
    }

    /// Choose the translation map. It is read and parsed here, so a broken map is refused before
    /// any run.
    pub fn translate_pick_map(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let dialog = rfd::AsyncFileDialog::new().set_title(tl!("Translation map (JSON file)")).add_filter("JSON", &["json"]);
            let _ = self.ask_one(Ask::File(dialog), None, |app, path| {
                app.translate.draft.map = path.to_string_lossy().into_owned();
                app.translate_load_map();
            });
        }
        #[cfg(target_arch = "wasm32")]
        self.notify_tr("Choosing a file needs the desktop app");
    }

    /// Read the map the draft names, and refuse it with a reason if it is not a map.
    pub fn translate_load_map(&mut self) {
        self.translate.map = None;
        self.translate.map_error = None;
        let path = self.translate.draft.map.trim().to_string();
        if path.is_empty() {
            return;
        }
        match std::fs::read(&path) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => match tr::Map::parse(&text) {
                    Ok(map) => self.translate.map = Some(map),
                    Err(error) => self.translate.map_error = Some(error.to_string()),
                },
                Err(_) => self.translate.map_error = Some(tl!("The translation map must be UTF-8").to_string()),
            },
            Err(error) => self.translate.map_error = Some(format!("{path}: {error}")),
        }
        self.refresh_translate_preview();
    }

    /// Choose the font the translation is drawn in, for a target script the document's own faces
    /// cannot show.
    pub fn translate_pick_font(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let dialog = rfd::AsyncFileDialog::new().set_title(tl!("Choose a font…")).add_filter("Font", &["ttf", "otf"]);
            let _ = self.ask_one(Ask::File(dialog), None, |app, path| app.translate.draft.font = path.to_string_lossy().into_owned());
        }
        #[cfg(target_arch = "wasm32")]
        self.notify_tr("Choosing a file needs the desktop app");
    }

    /// Choose where the copy goes. Opening the panel contacts nothing.
    pub fn translate_pick_out(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut dialog = rfd::AsyncFileDialog::new().set_title(tl!("Translate")).add_filter("PDF", &["pdf"]);
            if let Some(name) = std::path::Path::new(&self.translate.draft.out).file_name().map(|n| n.to_string_lossy().into_owned()) {
                dialog = dialog.set_file_name(name);
            }
            let _ = self.ask_one(Ask::Save(dialog), None, |app, path| app.translate.draft.out = path.to_string_lossy().into_owned());
        }
        #[cfg(target_arch = "wasm32")]
        self.notify_tr("Choosing a file needs the desktop app");
    }

    /// The dialog's Translate button. `Err` is a reason (already in the interface language) to stay
    /// open; nothing was edited and, unless a service row was chosen, no socket was opened.
    pub fn translate_run(&mut self) -> Result<(), String> {
        if self.translate.busy {
            return Err(tl!("A translation is already running").to_string());
        }
        let Some(id) = self.translate.doc else { return Err(tl!("Open a document first").to_string()) };
        if self.session.get(id).is_none() {
            return Err(tl!("Open a document first").to_string());
        }
        let draft = self.translate.draft.clone();
        let row = provider(draft.provider);
        // A run that cannot start leaves no report: the last one is not this one, and a stale
        // "Provider: …" line under a refusal would read as if this run had answered.
        self.translate.report = None;
        self.translate.written = None;
        self.translate.error = None;
        match row.kind {
            Kind::Map => {
                let Some(map) = self.translate.map.clone() else {
                    return Err(tl!("Choose a translation map first").to_string());
                };
                self.translate_finish(id, &draft, &mut MapBackend(map))
            }
            Kind::Service => {
                let fields = Fields {
                    endpoint: draft.endpoint.trim().to_string(),
                    key: draft.key.trim().to_string(),
                    source: draft.source_language.trim().to_string(),
                    target: draft.target.trim().to_string(),
                };
                if row.address_required && fields.endpoint.is_empty() {
                    return Err(tl!("The network service needs an endpoint").to_string());
                }
                if row.target && fields.target.is_empty() {
                    return Err(tl!("The network service needs a target language").to_string());
                }
                let Some(build) = row.build else { return Err(tl!("This provider cannot run").to_string()) };
                let strings = self.session.get(id).map(|d| translate::sources(d, &[])).unwrap_or_default();
                if strings.is_empty() {
                    return Err(tl!("Nothing to translate: the document has no text rows").to_string());
                }
                let asked = row.id;
                let (tx, rx) = std::sync::mpsc::channel();
                let ctx = self.ctx.clone();
                std::thread::spawn(move || {
                    let fetched = fetch(asked, build, &fields, &strings);
                    let _ = tx.send(fetched);
                    if let Some(ctx) = ctx {
                        ctx.request_repaint();
                    }
                });
                self.translate.running = Some(rx);
                self.translate.busy = true;
                Ok(())
            }
        }
    }

    /// Run the shared translation and keep its report. Writes the result as a copy unless the user
    /// asked for the open document to be changed.
    fn translate_finish(&mut self, id: DocId, draft: &Draft, backend: &mut dyn translate::Backend) -> Result<(), String> {
        let base = Options { pages: Vec::new(), min_size: tr::DEFAULT_MIN_SIZE, shrink: false, font_resource: None };
        let report = if draft.replace {
            let font = import_font(&mut self.session, id, &draft.font)?;
            let options = Options { font_resource: font, ..base };
            translate::translate(&mut self.session, id, &options, backend).map_err(|e| e.to_string())?
        } else {
            // A copy: the open document is not touched at all, not even in memory. The copy is the
            // same bytes, so it is the same run and the same report.
            let (name, bytes, password) = {
                let doc = self.session.get(id).ok_or_else(|| tl!("Open a document first").to_string())?;
                (doc.name.clone(), doc.bytes.clone(), doc.password.clone())
            };
            let mut copy = Session::new();
            let copy_id = copy.open(name, None, bytes, password.as_deref()).map_err(|e| e.to_string())?;
            let font = import_font(&mut copy, copy_id, &draft.font)?;
            let options = Options { font_resource: font, ..base };
            let report = translate::translate(&mut copy, copy_id, &options, backend).map_err(|e| e.to_string())?;
            let out = copy.save_full_bytes(copy_id).map_err(|e| e.to_string())?;
            self.translate.pending = Some(out);
            report
        };
        self.translate.report = Some(report);
        if draft.replace {
            return Ok(());
        }
        self.translate_save_copy()
    }

    /// Write the copy to the file the dialog names. It is never the document's own file, and the
    /// document itself stays untouched.
    fn translate_save_copy(&mut self) -> Result<(), String> {
        let Some(bytes) = self.translate.pending.take() else { return Ok(()) };
        let out = self.translate.draft.out.trim().to_string();
        if out.is_empty() {
            return Err(tl!("The copy needs a file name").to_string());
        }
        let same = self
            .translate
            .doc
            .and_then(|id| self.session.get(id).and_then(|d| d.path.clone()))
            .is_some_and(|p| std::path::Path::new(&p) == std::path::Path::new(&out));
        if same {
            return Err(tl!("The copy needs a file of its own: this document would be overwritten").to_string());
        }
        #[cfg(not(target_arch = "wasm32"))]
        match std::fs::write(&out, bytes.as_slice()) {
            Ok(()) => self.translate.written = Some(out),
            Err(error) => return Err(format!("{out}: {error}")),
        }
        #[cfg(target_arch = "wasm32")]
        {
            let name = std::path::Path::new(&out).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(out.clone());
            if let Err(error) = crate::editing::download(&name, &bytes) {
                self.notify_error(error);
            }
        }
        Ok(())
    }

    /// Pick up a finished service run (each frame). A run that failed says why and leaves the
    /// document exactly as it was: nothing is applied before every translation is in hand.
    pub(crate) fn poll_translate(&mut self) {
        let fetched = {
            let Some(rx) = self.translate.running.take() else { return };
            match rx.try_recv() {
                Ok(fetched) => fetched,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    self.translate.running = Some(rx);
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.translate.busy = false;
                    self.translate.error = Some(tl!("the endpoint stopped unexpectedly").to_string());
                    return;
                }
            }
        };
        self.translate.busy = false;
        if let Some(error) = fetched.error {
            self.translate.error = Some(error);
            return;
        }
        let Some(id) = self.translate.doc else { return };
        let draft = self.translate.draft.clone();
        let mut backend = Collected { translations: fetched.translations, requests: fetched.requests, provider: fetched.provider };
        if let Err(error) = self.translate_finish(id, &draft, &mut backend) {
            self.translate.error = Some(error);
        }
    }
}

/// Ask the chosen service for every string, off the frame thread. A provider that cannot be built
/// (Google, until its own card lands) says so here, before a single request.
fn fetch(
    provider: &'static str,
    build: fn(&Request<'_>) -> Result<Box<dyn tr::Translator + Send>, String>,
    fields: &Fields,
    strings: &[String],
) -> Fetched {
    let mut backend = match build(&fields.borrow()) {
        Ok(backend) => backend,
        Err(error) => return Fetched { translations: BTreeMap::new(), requests: 0, provider, error: Some(error) },
    };
    let mut translations = BTreeMap::new();
    for text in strings {
        match backend.translate(text) {
            Ok(translation) => {
                translations.insert(text.clone(), translation);
            }
            Err(error) => {
                return Fetched { translations: BTreeMap::new(), requests: backend.requests(), provider, error: Some(error.to_string()) };
            }
        }
    }
    Fetched { translations, requests: backend.requests(), provider, error: None }
}

/// Load `path` into the document as a text font and return the resource to draw with. `None` when
/// the user chose no font; the document is then left exactly as it was.
fn import_font(session: &mut Session, id: DocId, path: &str) -> Result<Option<String>, String> {
    let path = path.trim();
    if path.is_empty() {
        return Ok(None);
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let pages = session.get(id).map(|d| d.info.pages.len()).unwrap_or(0);
    for page in 0..pages {
        let before: Vec<String> = session.get(id).map(|d| d.text_editor_fonts(page).into_iter().map(|f| f.resource).collect()).unwrap_or_default();
        session.apply(id, Edit::RegisterTextFont { page, bytes: Arc::new(bytes.clone()) }).map_err(|e| e.to_string())?;
        let added = session
            .get(id)
            .and_then(|d| d.text_editor_fonts(page).into_iter().find(|f| !before.contains(&f.resource) && !f.resource.starts_with("PCApproved")));
        if let Some(font) = added {
            return Ok(Some(font.resource));
        }
    }
    Ok(None)
}

/// The dialog's fields. Returns `true` when it should close.
pub(crate) fn body(ui: &mut egui::Ui, app: &mut PdfCraftApp, t: &Tokens) -> bool {
    ui.label(egui::RichText::new(tl!("Translate")).font(theme::semibold(18.0)));
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(tl!("Replace the document's text with a translation, keeping each row's font, size, colour and place."))
            .size(12.0)
            .color(t.text_muted),
    );
    ui.add_space(10.0);

    // Where the translations come from: the map is the default and a service is off until the user
    // picks it. Choosing one is the explicit act that allows a request, and the rows come from the
    // table, so a new provider appears here without touching this drawer.
    let row = provider(app.translate.draft.provider);
    ui.horizontal_wrapped(|ui| {
        for offered in PROVIDERS {
            ui.radio_value(&mut app.translate.draft.provider, offered.id, tl!(offered.label));
        }
    });
    ui.label(egui::RichText::new(tl!(row.hint)).size(12.0).color(t.text_muted));
    ui.add_space(6.0);
    match row.kind {
        Kind::Map => {
            let mut reload = false;
            ui.horizontal(|ui| {
                let label = ui.label(tl!("Map:"));
                if ui
                    .add(egui::TextEdit::singleline(&mut app.translate.draft.map).desired_width(380.0).hint_text(tl!("No file chosen")))
                    .labelled_by(label.id)
                    .changed()
                {
                    reload = true;
                }
                if ui.button(tl!("Choose…")).clicked() {
                    app.translate_pick_map();
                }
            });
            if reload {
                app.translate_load_map();
            }
            if let Some(problem) = &app.translate.map_error {
                ui.label(egui::RichText::new(problem.clone()).color(ERROR));
            }
            if let Some(preview) = app.translate.survey {
                let numbers = [preview.covered.to_string(), preview.rows.to_string()];
                ui.label(
                    egui::RichText::new(crate::i18n::fmt(
                        tl!("The map covers {covered} of {rows} rows."),
                        &[("covered", &numbers[0]), ("rows", &numbers[1])],
                    ))
                    .size(12.0)
                    .color(t.text_muted),
                );
            }
        }
        Kind::Service => {
            egui::Grid::new("translate-service").num_columns(2).spacing([12.0, 8.0]).min_col_width(110.0).show(ui, |ui| {
                let field = |ui: &mut egui::Ui, label: &str, value: &mut String, width: f32| {
                    let l = ui.label(tl!(label));
                    ui.add(egui::TextEdit::singleline(value).desired_width(width)).labelled_by(l.id);
                    ui.end_row();
                };
                if let Some(label) = row.address {
                    field(ui, label, &mut app.translate.draft.endpoint, 320.0);
                }
                if row.target {
                    field(ui, "Source language:", &mut app.translate.draft.source_language, 100.0);
                    field(ui, "Target language:", &mut app.translate.draft.target, 100.0);
                }
                if let Some(label) = row.key {
                    field(ui, label, &mut app.translate.draft.key, 320.0);
                }
            });
        }
    }
    ui.add_space(8.0);

    ui.horizontal(|ui| {
        ui.label(tl!("Font:"));
        let shown = if app.translate.draft.font.is_empty() { tl!("None").to_string() } else { app.translate.draft.font.clone() };
        ui.label(egui::RichText::new(shown).color(t.text_muted));
        if ui.button(tl!("Choose a font…")).clicked() {
            app.translate_pick_font();
        }
    });
    ui.label(
        egui::RichText::new(tl!(
            "A script the document's own faces cannot draw (Cyrillic, Greek) needs a font; without one those rows fail instead of being drawn."
        ))
        .size(12.0)
        .color(t.text_muted),
    );
    ui.add_space(8.0);

    ui.add(widgets::checkbox(&mut app.translate.draft.replace, tl!("Replace the text in this document")));
    if !app.translate.draft.replace {
        ui.label(
            egui::RichText::new(tl!("Otherwise the translation is written to a new file and this document is left alone."))
                .size(12.0)
                .color(t.text_muted),
        );
        ui.horizontal(|ui| {
            let label = ui.label(tl!("Copy to:"));
            ui.add(egui::TextEdit::singleline(&mut app.translate.draft.out).desired_width(380.0)).labelled_by(label.id);
            if ui.button(tl!("Choose…")).clicked() {
                app.translate_pick_out();
            }
        });
    }
    ui.add_space(10.0);

    if app.translate.busy {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(tl!("Translating…"));
        });
    }
    if let Some(error) = &app.translate.error {
        ui.label(egui::RichText::new(error.clone()).color(ERROR));
    }
    if let Some(written) = &app.translate.written {
        let path = written.clone();
        ui.label(egui::RichText::new(crate::i18n::fmt(tl!("Written to {path}"), &[("path", &path)])).color(t.text_muted));
    }
    if let Some(report) = &app.translate.report {
        report_body(ui, report, t);
    }
    ui.add_space(10.0);

    let mut close = false;
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        let translate = widgets::pill_button(ui, tl!("Translate"), true);
        if translate.clicked() && !app.translate.busy {
            match app.translate_run() {
                Ok(()) => {}
                Err(reason) => app.translate.error = Some(reason),
            }
        }
        if widgets::pill_button(ui, tl!("Cancel"), false).clicked() {
            close = true;
        }
    });
    close
}

/// The run's numbers and the rows it left alone, with their coordinates — never "looks about
/// right".
fn report_body(ui: &mut egui::Ui, report: &Report, t: &Tokens) {
    ui.add_space(8.0);
    ui.separator();
    // Who answered, and how many requests that cost: a number, so "this never touched the network"
    // is visible rather than assumed.
    let requests = report.network_requests.to_string();
    ui.label(crate::i18n::fmt(tl!("Provider: {provider} · requests: {requests}"), &[("provider", report.provider), ("requests", &requests)]));
    if report.network_requests == 0 {
        ui.label(egui::RichText::new(tl!("No request left this computer.")).size(11.0).color(t.text_muted));
    }
    let numbers = [report.rows.to_string(), report.translated.to_string(), report.shrunk.to_string(), report.missing.count.to_string()];
    ui.label(crate::i18n::fmt(
        tl!("{rows} rows: {translated} translated, {shrunk} shrunk, {missing} missing"),
        &[("rows", &numbers[0]), ("translated", &numbers[1]), ("shrunk", &numbers[2]), ("missing", &numbers[3])],
    ));
    for (title, list, what) in [
        (tl!("Too long:"), &report.too_long, "size"),
        (tl!("Missing:"), &report.missing, ""),
        (tl!("Formatting lost:"), &report.formatting_lost, "kept"),
        (tl!("Failed"), &report.failed, "error"),
    ] {
        if list.count == 0 {
            continue;
        }
        ui.label(egui::RichText::new(format!("{title} {}", list.count)).strong());
        for row in list.rows.iter().take(20) {
            ui.label(egui::RichText::new(describe(row, what)).size(11.0).color(t.text_muted));
        }
    }
}

/// One reported row as a single line: page and box, the source text, and what it says about the
/// translation.
fn describe(row: &translate::Entry, what: &str) -> String {
    let mut line =
        format!("{} p.{} [{:.0}, {:.0}, {:.0}, {:.0}] “{}”", row.paragraph, row.page, row.rect[0], row.rect[1], row.rect[2], row.rect[3], row.text);
    match what {
        "size" => {
            if let Some(size) = row.size {
                line.push_str(&format!(" → {size} pt"));
            }
        }
        "kept" => {
            if let Some(kept) = &row.kept {
                line.push_str(&format!(" — {kept}"));
            }
        }
        _ => {}
    }
    if let Some(reason) = &row.reason {
        line.push_str(&format!(" — {reason}"));
    }
    if let Some(error) = &row.error {
        line.push_str(&format!(" — {error}"));
    }
    line
}
