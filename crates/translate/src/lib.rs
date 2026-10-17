//! pdfcraft-translate — the translation side of layout-preserving translation: where a paragraph's
//! text comes from, and how large the replacement may be drawn.
//!
//! Two backends sit behind one [`Translator`]:
//!
//! - [`Map`], a JSON object of `{"source string": "translation"}` that the user (or anything that
//!   can write JSON) supplies. It opens no socket, needs no key and is exactly reproducible, so it
//!   is the default and the path the tests take.
//! - [`Deeplx`], a DeepLX-compatible HTTP endpoint. It exists only when a caller passes one: there
//!   is no default endpoint and nothing here reaches the network on its own. The web build has no
//!   sockets, so there it is not compiled at all and [`Deeplx::new`] returns an error.
//!
//! [`fit_size`] is the layout half: a translation is usually longer than its source, so it is drawn
//! at the largest size, never below a floor, that still fits the source box. Measuring a candidate
//! is the caller's job (the PDF rewrite itself, which is what the document will contain), so this
//! crate holds no document code and its tests need no PDF.
//!
//! Layer L4. It depends on no other workspace crate.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

#[cfg(not(target_arch = "wasm32"))]
mod deeplx;
#[cfg(target_arch = "wasm32")]
#[path = "deeplx_web.rs"]
mod deeplx;
#[cfg(test)]
mod tests;

use std::collections::HashMap;

pub use deeplx::Deeplx;

/// Why a translation backend could not answer.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A translation map or a backend argument is unusable; the message says what to change.
    #[error("{0}")]
    Invalid(String),
    /// The endpoint could not be reached, or answered something that is not a translation.
    #[error("{0}")]
    Network(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Where a paragraph's translation comes from.
///
/// Implementors answer for one string at a time: the caller deduplicates, so the same sentence
/// appearing three times costs one lookup (and one request).
pub trait Translator {
    /// The translation of `text`, or `None` when this backend has none for it.
    ///
    /// A mapping gap is not an error — a document always contains something the map does not cover,
    /// and the caller reports those paragraphs instead of failing the run. A backend that could not
    /// be reached, or answered nonsense, returns [`Error`].
    fn translate(&mut self, text: &str) -> Result<Option<String>>;

    /// Outbound requests this backend has made. Always 0 for an offline backend, which is how a
    /// test can show the network was never touched.
    fn requests(&self) -> usize {
        0
    }
}

/// Most entries a translation map may hold.
pub const MAX_MAP_ENTRIES: usize = 200_000;
/// Most bytes a map file may hold (read by the caller, checked here as a second line of defence).
pub const MAX_MAP_BYTES: usize = 16 << 20;
/// The per-request timeout a network backend uses unless the caller picks another, in seconds.
pub const DEFAULT_TIMEOUT_SECS: f64 = 30.0;

/// The canonical form of a paragraph's text, used as a translation key: whitespace runs collapse to
/// one space and the ends are trimmed, so a map written by hand or exported from another tool
/// matches the paragraph however its line breaks fell.
pub fn key(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// A short, single-line, human-readable rendering of `text` for an error or report entry.
pub fn quote(text: &str) -> String {
    const MAX: usize = 80;
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for (n, ch) in flat.chars().enumerate() {
        if n >= MAX {
            out.push('…');
            break;
        }
        out.push(if ch.is_control() { ' ' } else { ch });
    }
    out
}

/// An offline translation map: one JSON object of source string → translation.
///
/// Keys are compared in [`key`] form, and a map that gives one source two different translations
/// (because two differently spaced keys collapse to the same key) is refused rather than silently
/// picking one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Map {
    entries: HashMap<String, String>,
}

impl Map {
    /// Parse a map. The document's own text is untrusted input too, but a malformed map is the
    /// user's mistake and gets a message naming the offending entry.
    pub fn parse(json: &str) -> Result<Self> {
        if json.len() > MAX_MAP_BYTES {
            return Err(Error::Invalid(format!("the translation map is larger than {} MiB", MAX_MAP_BYTES >> 20)));
        }
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|e| Error::Invalid(format!("the translation map is not valid JSON ({e})")))?;
        let object = value.as_object().ok_or_else(|| {
            Error::Invalid("the translation map must be a JSON object of source string → translation, e.g. {\"Hello\": \"Привет\"}".into())
        })?;
        if object.len() > MAX_MAP_ENTRIES {
            return Err(Error::Invalid(format!("the translation map has more than {MAX_MAP_ENTRIES} entries")));
        }
        let mut entries: HashMap<String, String> = HashMap::with_capacity(object.len());
        // The first source string that produced each key, so a collision can name both.
        let mut sources: HashMap<String, &str> = HashMap::with_capacity(object.len());
        for (source, translation) in object {
            let k = key(source);
            if k.is_empty() {
                return Err(Error::Invalid(format!("the translation {} has an empty source string", quote(&k))));
            }
            let Some(translation) = translation.as_str() else {
                return Err(Error::Invalid(format!("the translation of {} is not a string", quote(&k))));
            };
            if translation.trim().is_empty() {
                return Err(Error::Invalid(format!("the translation of {} is empty; remove the entry to leave that paragraph alone", quote(&k))));
            }
            if let Some(previous) = sources.get(&k) {
                if entries.get(&k).is_none_or(|t| t != translation) {
                    return Err(Error::Invalid(format!(
                        "{} and {} are the same source text with different translations",
                        quote(previous),
                        quote(&k)
                    )));
                }
                continue;
            }
            sources.insert(k.clone(), source);
            entries.insert(k, translation.to_owned());
        }
        Ok(Self { entries })
    }

    /// The translation of `text` in [`key`] form, if the map has one.
    pub fn lookup(&self, text: &str) -> Option<&str> {
        self.entries.get(&key(text)).map(String::as_str)
    }

    /// How many distinct source strings the map covers.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Translator for Map {
    fn translate(&mut self, text: &str) -> Result<Option<String>> {
        Ok(self.lookup(text).map(str::to_owned))
    }
}

/// The font size a translation is drawn at by default: the largest candidate that still fits the
/// source box, and never smaller than [`DEFAULT_MIN_SIZE`].
pub const DEFAULT_MIN_SIZE: f64 = 5.0;
/// Each candidate is this much smaller than the last.
const SIZE_STEP: f64 = 0.1;
/// Most candidates [`candidates`] will produce, whatever the sizes say.
const MAX_STEPS: usize = 64;

/// Font sizes to try for a translated paragraph, largest first: the source size, then 10% smaller
/// each step, never below `min` (itself never below 0.5 pt, and never above the source size).
///
/// An unreadable source size (zero, negative, NaN, infinite) yields no candidates, and the caller
/// reports the paragraph instead of drawing it at nonsense size.
pub fn candidates(source: f64, min: f64) -> Vec<f64> {
    let mut out = Vec::new();
    if !(source.is_finite() && source > 0.0) {
        return out;
    }
    let floor = if min.is_finite() { min.clamp(0.5, source) } else { source };
    let mut size = source;
    for _ in 0..MAX_STEPS {
        // Rounded so the report and the PDF read alike; the caller measures this exact value.
        let rounded = (size * 1000.0).round() / 1000.0;
        if out.last() != Some(&rounded) {
            out.push(rounded);
        }
        if size <= floor + 1e-9 {
            break;
        }
        let next = (size * (1.0 - SIZE_STEP)).max(floor);
        if next >= size {
            break;
        }
        size = next;
    }
    out
}

/// The largest candidate size whose measured layout fits, or `None` when even the smallest one does
/// not: `fits` reports whether the paragraph drawn at that size stays inside its source rectangle,
/// and may fail (a font that cannot show a character fails at every size).
pub fn fit_size<E>(source: f64, min: f64, mut fits: impl FnMut(f64) -> std::result::Result<bool, E>) -> std::result::Result<Option<f64>, E> {
    for size in candidates(source, min) {
        if fits(size)? {
            return Ok(Some(size));
        }
    }
    Ok(None)
}
