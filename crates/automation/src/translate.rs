//! `doc_translate`: replace a page's text with its translation, keeping every run's formatting.
//!
//! The run itself is [`pdfcraft_engine::translate`] — the same code File ▸ Translate… in the
//! interface calls (AGENTS.md §3), so the report here and the numbers the dialog shows are built
//! from one [`Report`]. What this module owns is the tool's side: the JSON arguments (a map file, an
//! explicit endpoint) and the JSON answer.
//!
//! An offline map answers first and an endpoint covers what it lacks; the endpoint is never
//! contacted unless `network` is passed, and the map alone makes no request at all.

use pdfcraft_engine::translate::{BackendError, Error, Options, Report};
use pdfcraft_translate as tr;
use pdfcraft_translate::Translator as _;
use serde_json::{Value, json};

use crate::{Args, Automation, Result, ToolError, failed, summary};

/// Most bytes a translation map file may hold.
const MAX_MAP_BYTES: usize = tr::MAX_MAP_BYTES;

/// One backend, or two: the offline map answers first and the endpoint covers what it lacks.
enum Backend {
    Map(tr::Map),
    Network(tr::Deeplx),
    Both(tr::Map, tr::Deeplx),
}

impl pdfcraft_engine::translate::Backend for Backend {
    fn provider(&self) -> &'static str {
        match self {
            Backend::Map(_) => "map",
            Backend::Network(_) => "deeplx",
            Backend::Both(..) => "map+deeplx",
        }
    }

    fn translate(&mut self, text: &str) -> std::result::Result<Option<String>, BackendError> {
        let answer = match self {
            Backend::Map(map) => map.translate(text),
            Backend::Network(network) => network.translate(text),
            Backend::Both(map, network) => match map.translate(text) {
                Ok(Some(translated)) => Ok(Some(translated)),
                Ok(None) => network.translate(text),
                Err(error) => Err(error),
            },
        };
        answer.map_err(|e| match e {
            tr::Error::Invalid(message) => BackendError::Invalid(message),
            tr::Error::Network(message) => BackendError::Network(message),
        })
    }

    fn requests(&self) -> usize {
        match self {
            Backend::Map(_) => 0,
            Backend::Network(network) | Backend::Both(_, network) => network.requests(),
        }
    }
}

/// The `network` argument: an endpoint, explicitly named, and the languages to ask it for.
struct Network {
    endpoint: String,
    source: String,
    target: String,
    timeout: f64,
}

impl Network {
    /// The argument as given, validated before any request could be made.
    fn parse(value: &Value) -> Result<Self> {
        let object = value.as_object().ok_or_else(|| Args::wrong("network", "an object {endpoint, target, source?, timeout?}"))?;
        for key in object.keys() {
            if !matches!(key.as_str(), "endpoint" | "target" | "source" | "timeout") {
                return Err(Args::wrong("network", "an object with only endpoint, target, source and timeout"));
            }
        }
        let a = Args(value);
        Ok(Self {
            endpoint: a.str("endpoint")?.to_owned(),
            source: a.opt_str("source")?.unwrap_or("auto").to_owned(),
            target: a.str("target")?.to_owned(),
            timeout: a.opt_num("timeout")?.unwrap_or(tr::DEFAULT_TIMEOUT_SECS),
        })
    }

    /// The HTTP translator this argument asks for. Building it contacts nothing.
    fn backend(&self) -> Result<tr::Deeplx> {
        tr::Deeplx::new(&self.endpoint, &self.source, &self.target, self.timeout).map_err(|e| ToolError::InvalidArgs(e.to_string()))
    }
}

/// Read the offline map, capped before it is read so a hostile file cannot exhaust memory.
fn read_map(auto: &Automation, path: &str) -> Result<tr::Map> {
    let file = auto.resolve(path, false)?;
    let size = std::fs::metadata(&file).map(|m| m.len()).map_err(|e| failed(format!("{}: {e}", file.display())))?;
    if size > MAX_MAP_BYTES as u64 {
        return Err(ToolError::InvalidArgs(format!("{}: the translation map is larger than {} MiB", file.display(), MAX_MAP_BYTES >> 20)));
    }
    let bytes = std::fs::read(&file).map_err(|e| failed(format!("{}: {e}", file.display())))?;
    let text = String::from_utf8(bytes).map_err(|_| ToolError::InvalidArgs(format!("{}: the translation map must be UTF-8", file.display())))?;
    tr::Map::parse(&text).map_err(|e| ToolError::InvalidArgs(format!("{}: {e}", file.display())))
}

/// A reason the shared run refused, in the tool's own error language.
fn tool_error(error: Error) -> ToolError {
    match error {
        Error::Invalid(message) => ToolError::InvalidArgs(message),
        Error::Failed(message) => ToolError::Failed(message),
    }
}

pub(crate) fn doc_translate(auto: &mut Automation, a: &Args) -> Result<Value> {
    // 1. Arguments and backends, before a page is read.
    let doc = auto.doc(a)?;
    let page_count = doc.info.pages.len();
    let id = doc.id;
    if page_count == 0 {
        return Err(failed("the document has no pages"));
    }
    let pages: Vec<usize> = match a.opt_ints("pages")? {
        Some(given) => {
            let mut out: Vec<usize> = Vec::with_capacity(given.len());
            for page in given {
                let page = usize::try_from(page)
                    .ok()
                    .filter(|p| (1..=page_count).contains(p))
                    .ok_or_else(|| ToolError::InvalidArgs(format!("page {page} is out of range: the document has {page_count} pages")))?;
                if !out.contains(&(page - 1)) {
                    out.push(page - 1);
                }
            }
            if out.is_empty() {
                return Err(Args::wrong("pages", "a non-empty array of page numbers"));
            }
            out.sort_unstable();
            out
        }
        None => (0..page_count).collect(),
    };
    let min_size = a.opt_num("min_size")?.unwrap_or(tr::DEFAULT_MIN_SIZE);
    if !(0.5..=144.0).contains(&min_size) {
        return Err(Args::wrong("min_size", "a font size in points from 0.5 to 144"));
    }
    let shrink = a.opt_bool("shrink")?.unwrap_or(false);
    let font_resource = a.opt_str("font_resource")?.map(str::to_owned);
    let map = match a.opt_str("translations")? {
        Some(path) => Some(read_map(auto, path)?),
        None => None,
    };
    let network = a.get("network").map(Network::parse).transpose()?;
    let mut backend = match (map, network) {
        (None, None) => {
            return Err(ToolError::InvalidArgs(
                "pass translations (a JSON map of source string → translation) or network (an explicit DeepLX-compatible endpoint)".into(),
            ));
        }
        (Some(map), None) => Backend::Map(map),
        (None, Some(network)) => Backend::Network(network.backend()?),
        (Some(map), Some(network)) => Backend::Both(map, network.backend()?),
    };

    // 2. The run itself, shared with File ▸ Translate….
    let options = Options { pages, min_size, shrink, font_resource };
    let report: Report = pdfcraft_engine::translate::translate(&mut auto.session, id, &options, &mut backend).map_err(tool_error)?;

    // 3. Optionally write the result as its own file: a full rewrite, pushed atomically, so the
    //    source document is never touched and a failure leaves no half-written PDF.
    let written = match a.opt_str("out")? {
        Some(path) => {
            let bytes = auto.session.save_full_bytes(id).map_err(failed)?;
            let target = auto.write_output(path, &bytes)?;
            Some(json!({ "path": target.to_string_lossy(), "bytes": bytes.len() }))
        }
        None => None,
    };

    let mut value = report.to_json();
    // The report the dialog shows has no open document to describe; the tool's answer does.
    value["document"] = summary(auto.doc(a)?);
    if let Some(written) = written {
        value["out"] = written;
    }
    // Warnings are part of the report already; the tool's answer keeps them under the same key.
    Ok(value)
}
