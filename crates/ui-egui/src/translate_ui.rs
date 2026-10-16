//! File ▸ Translate…: translate the text of the open document and open the result as a new one.
//!
//! The dialog is a thin front end for `pdfcraft_translate`: it collects the language pair, the
//! backend and whether overflow is allowed, hands the document's bytes to a worker thread, and
//! opens what comes back. The worker keeps the interface alive: a hundred-page manual over a
//! network backend takes minutes, and a frozen window looks like a crash.
//!
//! What the user sees when it finishes is the evidence, not a promise: how many blocks were
//! replaced, how many were refused because the translation did not fit, how many needed another
//! font, and whether any value changed on the way. The original document stays open and untouched.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};

use crate::widgets;

/// Which service translates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// A local DeepLX server: no key, no shared rate limit.
    DeepLx,
    /// The public Google endpoint: no key, rate limited, best effort.
    Google,
    /// The official Google v2 API; the key comes from `GOOGLE_TRANSLATE_API_KEY`.
    GoogleApi,
    /// Upper-cases the text: proves the layout path without any network.
    Upper,
}

impl Backend {
    fn id(self) -> &'static str {
        match self {
            Backend::DeepLx => "deeplx",
            Backend::Google => "google",
            Backend::GoogleApi => "google-api",
            Backend::Upper => "upper",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Backend::DeepLx => "DeepLX (local server)",
            Backend::Google => "Google Translate (public endpoint)",
            Backend::GoogleApi => "Google Translate (official API, needs a key)",
            Backend::Upper => "Upper case (layout test, no network)",
        }
    }
}

/// What the dialog holds between frames. Lives on the app, because `Dialog` is `Copy`.
pub struct TranslateDraft {
    pub source: String,
    pub target: String,
    pub backend: Backend,
    pub endpoint: String,
    pub allow_overflow: bool,
    /// The last thing worth telling the user: a status, or the report of the last run.
    pub note: String,
    job: Option<Receiver<Result<Done, String>>>,
}

struct Done {
    name: String,
    bytes: Vec<u8>,
    summary: String,
}

impl Default for TranslateDraft {
    fn default() -> Self {
        Self {
            source: "en".into(),
            target: "ru".into(),
            backend: Backend::DeepLx,
            endpoint: "http://127.0.0.1:1188/translate".into(),
            allow_overflow: false,
            note: String::new(),
            job: None,
        }
    }
}

impl TranslateDraft {
    /// Is a translation running?
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// The source document changed or the dialog was reopened: forget the last run.
    pub fn reset(&mut self) {
        if self.job.is_none() {
            self.note.clear();
        }
    }

    /// Stop waiting for a run. The worker finishes on its own: it holds nothing the app needs,
    /// and the document it was reading is a copy.
    pub fn abandon(&mut self) {
        self.job = None;
        self.note = "Translation cancelled; the original document is unchanged.".into();
    }

    /// Start translating `bytes` on a worker thread.
    fn start(&mut self, name: String, bytes: Arc<Vec<u8>>) {
        if self.job.is_some() {
            return;
        }
        if self.source.trim().is_empty() || self.target.trim().is_empty() {
            self.note = "Both language codes are needed (for example en and ru).".into();
            return;
        }
        let (source, target) = (self.source.trim().to_string(), self.target.trim().to_string());
        let backend = self.backend.id().to_string();
        let endpoint = self.endpoint.trim().to_string();
        let allow_overflow = self.allow_overflow;
        let (sender, receiver) = std::sync::mpsc::channel();
        let stem = std::path::Path::new(&name).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| name.clone());
        let out_name = format!("{stem}-{target}.pdf");
        self.note = format!("Translating {name} ({source} → {target}) with {backend}…");
        self.job = Some(receiver);
        std::thread::spawn(move || {
            let result = run(&bytes, &source, &target, &backend, &endpoint, allow_overflow).map(|(bytes, summary)| Done { name: out_name, bytes, summary });
            let _ = sender.send(result);
        });
    }

    /// Collect a finished run. Returns true when the dialog should close because the result is
    /// now an open document.
    fn poll(&mut self, app: &mut crate::PdfCraftApp) -> bool {
        let Some(receiver) = self.job.as_ref() else { return false };
        match receiver.try_recv() {
            Ok(Ok(done)) => {
                self.job = None;
                self.note = done.summary;
                match app.open_bytes(&done.name, None, done.bytes) {
                    Ok(()) => true,
                    Err(e) => {
                        self.note = format!("The translation finished, but it could not be opened: {e}");
                        false
                    }
                }
            }
            Ok(Err(e)) => {
                self.job = None;
                self.note = e;
                false
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.job = None;
                self.note = "The translation stopped without an answer.".into();
                false
            }
        }
    }
}

/// The whole translation, away from the interface thread.
fn run(
    bytes: &Arc<Vec<u8>>,
    source: &str,
    target: &str,
    backend: &str,
    endpoint: &str,
    allow_overflow: bool,
) -> Result<(Vec<u8>, String), String> {
    use pdfcraft_cos::{Document, SaveOptions, write_full};
    use pdfcraft_translate::{Options, backends, translate_document};

    let mut translator = backends::by_name(backend, source, target, Some(endpoint)).map_err(|e| format!("Backend: {e}"))?;
    let mut doc = Document::open(bytes.clone()).map_err(|e| format!("The document could not be read: {e}"))?;
    let options = Options { allow_overflow, ..Options::default() };
    let report = translate_document(&mut doc, translator.as_mut(), &options).map_err(|e| format!("Translation stopped: {e}"))?;
    if report.translated == 0 {
        return Err(format!(
            "Nothing was replaced: {} block(s) were refused because the translation did not fit and {} were skipped. {}",
            report.overflow,
            report.skipped,
            report.summary_line()
        ));
    }
    let out = write_full(&doc, &SaveOptions::default()).map_err(|e| format!("The result could not be written: {e}"))?;
    Ok((out.as_ref().clone(), report.summary_line()))
}

/// The dialog body: the choices that steer a run.
pub fn body(ui: &mut egui::Ui, draft: &mut TranslateDraft) {
    ui.label(egui::RichText::new("Translate document").font(crate::theme::semibold(18.0)));
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.label("From");
        ui.add_enabled(!draft.busy(), egui::TextEdit::singleline(&mut draft.source).desired_width(56.0));
        ui.label("To");
        ui.add_enabled(!draft.busy(), egui::TextEdit::singleline(&mut draft.target).desired_width(56.0));
    });
    ui.add_space(6.0);
    ui.label("Service");
    ui.add_enabled_ui(!draft.busy(), |ui| {
        egui::ComboBox::from_id_salt("translate-backend")
            .selected_text(draft.backend.label())
            .show_ui(ui, |ui| {
                for backend in [Backend::DeepLx, Backend::Google, Backend::GoogleApi, Backend::Upper] {
                    ui.selectable_value(&mut draft.backend, backend, backend.label());
                }
            });
        if draft.backend == Backend::DeepLx {
            ui.horizontal(|ui| {
                ui.label("Server");
                ui.add(egui::TextEdit::singleline(&mut draft.endpoint).desired_width(320.0));
            });
        }
    });
    ui.add_space(6.0);
    ui.add_enabled_ui(!draft.busy(), |ui| {
        ui.checkbox(&mut draft.allow_overflow, "Write translations that do not fit their block");
    });
    ui.label(
        egui::RichText::new("Off by default: a translation that grows past its box would cover whatever is drawn beside it.")
            .weak(),
    );
    if !draft.note.is_empty() {
        ui.add_space(8.0);
        ui.label(&draft.note);
    }
}

/// The dialog footer. Returns true when the dialog should close.
pub fn footer(ui: &mut egui::Ui, app: &mut crate::PdfCraftApp) -> bool {
    let mut close = false;
    // A finished run turns into an open document; a running one keeps the window alive.
    if app.translate_draft.poll(app) {
        close = true;
    }
    if app.translate_draft.busy() {
        ui.spinner();
        ui.label("Working…");
        if widgets::pill_button(ui, "Cancel", false).clicked() {
            // The worker is left to finish on its own: it holds no lock the app needs, and
            // dropping the receiver is not worth killing a thread over.
            app.translate_draft.abandon();
            close = true;
        }
        return close;
    }
    let ready = app.active_ids().is_some();
    if ui.add_enabled_ui(ready, |ui| widgets::pill_button(ui, "Translate", true)).inner.clicked() {
        if let Some((_, id)) = app.active_ids() {
            let source = app.session.get(id).map(|d| (d.name.clone(), d.bytes.clone()));
            match source {
                Some((name, bytes)) => app.translate_draft.start(name, bytes),
                None => app.translate_draft.note = "The document could not be read.".into(),
            }
        }
    }
    if widgets::pill_button(ui, "Cancel", false).clicked() {
        close = true;
    }
    close
}
