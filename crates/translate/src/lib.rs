//! Layout-preserving translation.
//!
//! The pipeline is deliberately boring, because the hard part already lives in `pdfcraft-edit`:
//!
//! 1. [`plan`] reads the text the page actually draws, as [`Segment`]s: the block index, its text,
//!    its rectangle, its rotation and size. Nothing is invented — a segment is what
//!    `text_blocks` reports, so a block the editor cannot address is never translated.
//! 2. A [`Translator`] turns the source strings into target strings. Batching is the translator's
//!    contract, not ours: a glossary-backed or model-backed backend wants whole paragraphs.
//! 3. Every replacement is laid out by `text_edit_layout` *before* it is written. A translation
//!    that cannot fit its own block is refused — the original stays on the page and the reason
//!    goes into the report. Reflowing a translated line over a drawing is the one failure a
//!    reader cannot undo, so it is not allowed to happen silently.
//! 4. Numbers and units the source carried must survive; [`check::integrity`] compares them and
//!    flags a mismatch instead of publishing it.
//!
//! What the report is for: a translation run is evidence. It names the backend, the blocks it
//! touched, the ones it refused and why, the font substitutions, and the integrity flags.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod backends;
pub mod check;

#[cfg(test)]
mod tests;

use pdfcraft_cos::Document;
use pdfcraft_edit::text::text_edit_layout;
use pdfcraft_edit::{BlockStyle, replace_block, text_blocks};
use serde::Serialize;
use thiserror::Error;

/// One addressable piece of text on a page, as the editor sees it.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Segment {
    /// 0-based page index.
    pub page: usize,
    /// 0-based block index on that page, valid for the whole run (the plan is taken once).
    pub block: usize,
    /// The source text, exactly as drawn.
    pub text: String,
    /// The block's rectangle in page units: `[x0, y0, x1, y1]`.
    pub rect: [f64; 4],
    /// Text rotation in degrees, as reported for the block.
    pub rotation: f64,
    /// Font size in page units.
    pub size: f64,
    /// The font the block draws with.
    pub font: String,
}

/// What happened to one segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Fit {
    /// The replacement was laid out inside its block and written to the page.
    Fits,
    /// The replacement did not fit its block and was refused.
    Overflow,
    /// The source text was not worth sending (no letters), so nothing was replaced.
    Skipped,
    /// `allow_overflow` was set: the replacement was written even though it does not fit.
    FitsUnchecked,
}

/// The per-segment record. A run without this is a claim; with it, it is evidence.
#[derive(Clone, Debug, Serialize)]
pub struct Outcome {
    pub page: usize,
    pub block: usize,
    pub source: String,
    pub target: String,
    pub source_chars: usize,
    pub target_chars: usize,
    /// How much wider (or narrower) the block would become, from the layout draft.
    pub width_ratio: Option<f64>,
    pub fit: Fit,
    /// `Some(font)` when the block's own font could not show the target text, so a fallback was
    /// used. This is the case a reader notices as "the translated line changed its typeface".
    pub substituted: Option<String>,
    /// Integrity problems between source and target (missing or invented numbers).
    pub integrity: Vec<String>,
}

impl Outcome {
    pub fn applied(&self) -> bool {
        matches!(self.fit, Fit::Fits | Fit::FitsUnchecked)
    }
}

/// What a run produced.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    pub translator: String,
    pub pages_scanned: usize,
    pub segments: usize,
    pub translated: usize,
    pub skipped: usize,
    pub overflow: usize,
    pub substituted: usize,
    pub integrity_flags: usize,
    pub outcomes: Vec<Outcome>,
}

impl Report {
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into())
    }

    /// One line a command line or a dialog can show without parsing the JSON.
    pub fn summary_line(&self) -> String {
        format!(
            "{}: {} of {} blocks translated ({} skipped), {} refused because they did not fit, {} font substitutions, {} value flags",
            self.translator,
            self.translated,
            self.segments - self.skipped,
            self.skipped,
            self.overflow,
            self.substituted,
            self.integrity_flags
        )
    }
}

/// One translation backend. Implementations decide how batching, caching and glossaries work;
/// the caller only promises that a batch comes back with one string per input.
pub trait Translator {
    /// A name for the report, e.g. `glossary+deeplx` or `local:opus-mt-en-ru`.
    fn id(&self) -> String;

    /// Translate a batch. The returned vector must have exactly one entry per input.
    fn translate(&mut self, input: &[String]) -> Result<Vec<String>, String>;
}

/// How a run behaves.
#[derive(Clone, Debug)]
pub struct Options {
    /// Pages to touch; `None` means every page.
    pub pages: Option<Vec<usize>>,
    /// Minimum number of letters a block must contain to be sent for translation. Text such as
    /// `12.5`, `—` or `Fig.` is not worth a round trip and risks turning a label into a sentence.
    pub min_letters: usize,
    /// Write replacements even when they do not fit their block. Off by default: overflow is the
    /// failure a reader cannot undo.
    pub allow_overflow: bool,
    /// How many segments go to the translator in one call.
    pub batch: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self { pages: None, min_letters: 2, allow_overflow: false, batch: 20 }
    }
}

#[derive(Debug, Error)]
pub enum TranslateError {
    #[error("{0}")]
    Edit(#[from] pdfcraft_edit::EditError),
    /// A backend returned the wrong number of strings, which would misalign the page.
    #[error("translator {translator} returned {got} strings for {expected} inputs")]
    BadBatch { translator: String, expected: usize, got: usize },
    #[error("{0}")]
    Backend(String),
}

/// Letters that make a block worth translating. Digits, punctuation and lone symbols are labels,
/// not prose.
pub fn is_translatable(text: &str, min_letters: usize) -> bool {
    text.chars().filter(|c| c.is_alphabetic()).count() >= min_letters
}

/// Read the translation plan: every block the editor can address, in page order.
pub fn plan(doc: &Document, options: &Options) -> Result<Vec<Segment>, TranslateError> {
    let pages = 0..pdfcraft_model::pages(doc).len();
    let mut out = Vec::new();
    for page in pages {
        if let Some(selected) = &options.pages {
            if !selected.contains(&page) {
                continue;
            }
        }
        let blocks = match text_blocks(doc, page) {
            Ok(blocks) => blocks,
            // A page without text is not an error: a translated manual has figure pages.
            Err(_) => continue,
        };
        for (block, b) in blocks.iter().enumerate() {
            out.push(Segment {
                page,
                block,
                text: b.editor_text.clone(),
                rect: b.rect,
                rotation: b.rotation,
                size: b.size,
                font: b.base_font.clone(),
            });
        }
    }
    Ok(out)
}

/// Translate a document in place and return the evidence.
pub fn translate_document(
    doc: &mut Document,
    translator: &mut dyn Translator,
    options: &Options,
) -> Result<Report, TranslateError> {
    let plan = plan(doc, options)?;
    let mut report = Report { translator: translator.id(), segments: plan.len(), ..Default::default() };
    report.pages_scanned = pdfcraft_model::pages(doc).len();

    let work: Vec<Segment> = plan
        .into_iter()
        .filter(|s| is_translatable(&s.text, options.min_letters))
        .collect();
    report.skipped = report.segments - work.len();

    for chunk in work.chunks(options.batch.max(1)) {
        let inputs: Vec<String> = chunk.iter().map(|s| s.text.clone()).collect();
        let outputs = translator.translate(&inputs).map_err(TranslateError::Backend)?;
        if outputs.len() != inputs.len() {
            return Err(TranslateError::BadBatch {
                translator: report.translator.clone(),
                expected: inputs.len(),
                got: outputs.len(),
            });
        }
        for (segment, target) in chunk.iter().zip(outputs) {
            let target = target.trim().to_string();
            let layout = text_edit_layout(doc, segment.page, segment.block, &target, &BlockStyle::default())?;
            let width_ratio = layout.draft_rect.map(|d| {
                let source_width = (segment.rect[2] - segment.rect[0]).abs();
                let draft_width = (d[2] - d[0]).abs();
                if source_width > 0.0 { draft_width / source_width } else { 1.0 }
            });
            let integrity = check::integrity(&segment.text, &target);
            let mut outcome = Outcome {
                page: segment.page,
                block: segment.block,
                source: segment.text.clone(),
                target: target.clone(),
                source_chars: segment.text.chars().count(),
                target_chars: target.chars().count(),
                width_ratio,
                fit: Fit::Fits,
                substituted: None,
                integrity: integrity.clone(),
            };
            if !integrity.is_empty() {
                report.integrity_flags += integrity.len();
            }
            if layout.overflow && !options.allow_overflow {
                outcome.fit = Fit::Overflow;
                report.overflow += 1;
                report.outcomes.push(outcome);
                continue;
            }
            let edit = replace_block(doc, segment.page, segment.block, &target)?;
            outcome.substituted = edit.substituted;
            if outcome.substituted.is_some() {
                report.substituted += 1;
            }
            outcome.fit = if layout.overflow { Fit::FitsUnchecked } else { Fit::Fits };
            report.translated += 1;
            report.outcomes.push(outcome);
        }
    }
    Ok(report)
}

/// A translator that proves the pipeline without a network: it upper-cases the source.
///
/// It exists so the layout surgery can be tested on its own, and so `--translator upper` is a
/// smoke test a user can run on a document before pointing a real backend at it.
#[derive(Default)]
pub struct UpperCase;

impl Translator for UpperCase {
    fn id(&self) -> String {
        "upper-case (test backend, no translation)".into()
    }

    fn translate(&mut self, input: &[String]) -> Result<Vec<String>, String> {
        Ok(input.iter().map(|s| s.to_uppercase()).collect())
    }
}
