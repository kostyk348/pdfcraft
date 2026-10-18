//! Layout-preserving translation: replace a page's text with its translation, keeping every run's
//! formatting.
//!
//! This is the one implementation behind both `doc_translate` (the headless tool in
//! `pdfcraft-automation`) and File ▸ Translate… (the interface's dialog), as AGENTS.md §3 asks: the
//! report the dialog shows and the report the tool prints are built from the same [`Report`], so
//! they cannot disagree.
//!
//! The owner's rule is that translating changes the text and nothing else, so the unit is the
//! **visual row** of a paragraph, not the paragraph: each row keeps its own font resource (so a
//! bold heading stays bold), its own size, colour and box, and its own baseline, and rows are never
//! merged or re-flowed across each other. What the engine draws is the source layout with other
//! words in it.
//!
//! Whether a translation fits is decided by measuring the very rewrite Apply would run, and always
//! row by row: one row that is too long must not cost the paragraph the rows that do fit. Nothing is
//! ever drawn smaller unless the caller passed `shrink`, and then only the row that needs it.
//!
//! Three things can still go wrong, and all three are reported per element instead of being
//! smoothed over:
//!
//! - a row whose translation does not fit its own box at its own size: it is left exactly as it was
//!   and listed in `too_long` with its box, its text and the translation.
//! - a row that holds runs in several styles (a bold word inside a sentence). The translation map may
//!   mark a span with `<b>` or `<i>`, and the marker names a style the row really has, so the span is
//!   drawn in that run's font, size and colour; the rest of the row keeps its first run's. A row the
//!   map does not mark, and markup that cannot be honoured (broken tags, more markers than the row
//!   has runs, a marker for a style the row lacks), is drawn in the first run's style and listed as
//!   `formatting_lost` with the reason — never silently.
//! - a translation whose font cannot show a character: the engine's own notice list is passed on.
//!
//! Translations are collected for the whole run before the first edit, so a backend that cannot
//! answer leaves the document untouched. Only a [`Backend`] that reaches a socket can make this
//! contact the network, and the caller decides whether to pass one.

use std::collections::{BTreeMap, BTreeSet};

use pdfcraft_render::PageInfo;
use pdfcraft_translate as tr;

use crate::{BlockStyle, DocId, Document, Edit, RangeStyle, Session, TextBlock, TextLine};

/// Most entries one problem list holds, so a pathological document cannot answer with megabytes of
/// JSON. The count beside each list is the real one.
pub const MAX_REPORTED: usize = 100;
/// Most distinct fonts a report lists.
const MAX_FONTS: usize = 8;

/// Where a translation comes from. The interface supplies the offline map, or the map and an
/// endpoint the user named; the headless tool does the same. Nothing here reaches a socket by
/// itself.
pub trait Backend {
    /// The provider that answers, as the report names it: `map`, `deeplx`, `google`. It is a field
    /// of the report rather than a detail of the caller, so a reader of the report can see whether
    /// a run could have gone to the network at all.
    fn provider(&self) -> &'static str;

    /// The translation of `text`, or `None` when this backend has none for it.
    ///
    /// A mapping gap is not an error — a document always contains something a map does not cover,
    /// and those rows are reported as `missing` instead of failing the run.
    fn translate(&mut self, text: &str) -> std::result::Result<Option<String>, BackendError>;

    /// Outbound requests this backend has made. Always 0 for an offline backend, which is how a
    /// test can show the network was never touched.
    fn requests(&self) -> usize {
        0
    }
}

/// Why a backend could not answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendError {
    /// A map or an argument is unusable; the message says what to change.
    Invalid(String),
    /// The endpoint could not be reached, or answered something that is not a translation.
    Network(String),
}

/// Why a run stopped. `Invalid` is the caller's mistake (a page number out of range, a map that
/// cannot be read) and changed nothing; `Failed` is a document or backend failure, and also changed
/// nothing, because the whole run is collected before the first edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid(String),
    Failed(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Invalid(message) | Error::Failed(message) => f.write_str(message),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// What one run should do. Pages are 0-based.
#[derive(Clone, Debug)]
pub struct Options {
    /// The pages to translate, or every page when empty.
    pub pages: Vec<usize>,
    /// The smallest size a row may be drawn at when `shrink` allows shrinking.
    pub min_size: f64,
    /// Whether a row that does not fit may be drawn smaller. Off by default: shrinking is never
    /// silent.
    pub shrink: bool,
    /// A page font resource every translation is drawn in. Needed for a script the document's
    /// own faces lack (Cyrillic, Greek); it costs the row its inside-the-row formatting, which is
    /// reported as `formatting_lost`.
    pub font_resource: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self { pages: Vec::new(), min_size: tr::DEFAULT_MIN_SIZE, shrink: false, font_resource: None }
    }
}

/// One reported row: where it is (top-left-origin points of the displayed page, the same
/// coordinates every other tool reports and accepts), what it says, and — for a problem — why it
/// was left alone.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// 1-based page number.
    pub page: usize,
    /// The displayed-page box.
    pub rect: [f32; 4],
    /// The source text, quoted to one short line.
    pub text: String,
    /// 1-based paragraph and row numbers on that page (a whole-paragraph failure names no row).
    pub paragraph: usize,
    pub row: Option<usize>,
    /// The translation that could not be placed.
    pub translation: Option<String>,
    /// The size it was drawn at (shrunk rows) or kept at (rows too long).
    pub size: Option<f64>,
    /// What the row kept, when formatting was lost: "11 pt Inter-Bold".
    pub kept: Option<String>,
    pub reason: Option<String>,
    pub error: Option<String>,
}

/// A problem list with its true size (the list itself is capped at [`MAX_REPORTED`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProblemList {
    pub count: usize,
    pub rows: Vec<Entry>,
}

/// Everything one run did, in numbers — never "looks about right".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    /// 1-based page numbers the run covered.
    pub pages: Vec<usize>,
    pub rows: usize,
    pub translated: usize,
    pub unchanged: usize,
    pub shrunk: usize,
    pub shrunk_rows: ProblemList,
    pub invisible: usize,
    pub skipped: usize,
    pub reverted: usize,
    pub missing: ProblemList,
    pub too_long: ProblemList,
    pub failed: ProblemList,
    pub formatting_lost: ProblemList,
    /// The provider that answered (`map`, `deeplx`, `google`, `map+deeplx`), so the report itself
    /// says whether the run could have reached the network.
    pub provider: &'static str,
    /// Requests the provider made. 0 means nothing left this computer.
    pub network_requests: usize,
    pub fonts: Vec<String>,
    /// Text the edit stored as typed but could only draw with `?` (its face cannot show it).
    pub warnings: Vec<String>,
}

impl Report {
    /// The report as the JSON both the tool and the interface's tests read. The keys are the
    /// `doc_translate` ones, so the dialog's numbers and the tool's are the same numbers.
    pub fn to_json(&self) -> serde_json::Value {
        let list = |list: &ProblemList| {
            serde_json::json!({
                "count": list.count,
                "paragraphs": list.rows.iter().map(entry_json).collect::<Vec<_>>(),
            })
        };
        serde_json::json!({
            "pages": self.pages,
            "rows": self.rows,
            "translated": self.translated,
            "unchanged": self.unchanged,
            "shrunk": self.shrunk,
            "shrunk_rows": list(&self.shrunk_rows),
            "invisible": self.invisible,
            "skipped": self.skipped,
            "reverted": self.reverted,
            "missing": list(&self.missing),
            "too_long": list(&self.too_long),
            "failed": list(&self.failed),
            "formatting_lost": list(&self.formatting_lost),
            "provider": self.provider,
            "network_requests": self.network_requests,
            "fonts": self.fonts,
            "warnings": self.warnings,
        })
    }
}

fn entry_json(entry: &Entry) -> serde_json::Value {
    let mut value = serde_json::json!({
        "page": entry.page,
        "rect": [entry.rect[0], entry.rect[1], entry.rect[2], entry.rect[3]],
        "text": entry.text,
        "paragraph": entry.paragraph,
    });
    if let Some(row) = entry.row {
        value["row"] = serde_json::json!(row);
    }
    for (key, text) in [("translation", &entry.translation), ("kept", &entry.kept), ("reason", &entry.reason), ("error", &entry.error)] {
        if let Some(text) = text {
            value[key] = serde_json::Value::String(text.clone());
        }
    }
    if let Some(size) = entry.size {
        value["size"] = serde_json::json!(size);
    }
    value
}

/// What the dialog can say before the run: how many of the document's rows the map covers, and how
/// many it does not. No edit, and no backend request beyond the map lookups the caller passes in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Survey {
    pub pages: usize,
    pub rows: usize,
    pub covered: usize,
    pub uncovered: usize,
}

/// The distinct source strings of `pages` (every page when empty), in document order: what a
/// network backend has to be asked about. No edit, and nothing is drawn.
pub fn sources(doc: &Document, pages: &[usize]) -> Vec<String> {
    let pages: Vec<usize> = if pages.is_empty() { (0..doc.info.pages.len()).collect() } else { pages.to_vec() };
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for &page in &pages {
        if page >= doc.info.pages.len() {
            continue;
        }
        let lines = doc.text_lines(page);
        for block in doc.text_blocks(page) {
            if block.invisible {
                continue;
            }
            for row in rows(&block, &lines) {
                let source = tr::key(&row.text);
                if !source.is_empty() {
                    seen.insert(source);
                }
            }
        }
    }
    seen.into_iter().collect()
}

/// Count the document's rows and how many of them `lookup` answers for, without touching the
/// document. `lookup` is the offline map: a preview must never send a request.
pub fn survey(doc: &Document, pages: &[usize], mut lookup: impl FnMut(&str) -> bool) -> Survey {
    let pages: Vec<usize> = if pages.is_empty() { (0..doc.info.pages.len()).collect() } else { pages.to_vec() };
    let mut out = Survey { pages: pages.len(), ..Survey::default() };
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for &page in &pages {
        if page >= doc.info.pages.len() {
            continue;
        }
        let lines = doc.text_lines(page);
        for block in doc.text_blocks(page) {
            if block.invisible {
                continue;
            }
            for row in rows(&block, &lines) {
                out.rows += 1;
                let source = tr::key(&row.text);
                if source.is_empty() || !seen.insert(source.clone()) {
                    continue;
                }
                if lookup(&source) {
                    out.covered += 1;
                } else {
                    out.uncovered += 1;
                }
            }
        }
    }
    out
}

/// One visual row: the text it shows, the runs it is drawn in, and the style the row starts with. A
/// row is the unit of translation, so its size, colour, font and box survive the rewrite.
#[derive(Clone, Debug)]
struct Row {
    text: String,
    rect: [f64; 4],
    /// Every run of the row, in drawing order: a bold word inside a sentence is a run of its own.
    runs: Vec<Run>,
    /// The style everything the map does not mark is drawn in, and the row's own style.
    first: Run,
    /// The row holds runs in more than one style, so a translation keeps the first unless the map
    /// says which run each part of it takes.
    mixed: bool,
}

/// One run of a visual row: the resource it is drawn with, and how.
#[derive(Clone, Debug)]
struct Run {
    /// The page's font resource, which is what keeps a bold word bold.
    font: String,
    base_font: String,
    size: f64,
    color: [f64; 3],
    bold: bool,
    italic: bool,
}

impl Run {
    fn of(line: &TextLine) -> Self {
        Self { font: line.font.clone(), base_font: line.base_font.clone(), size: line.size, color: line.color, bold: line.bold, italic: line.italic }
    }
}

fn union(a: [f64; 4], b: [f64; 4]) -> [f64; 4] {
    [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]
}

/// The paragraph's visual rows, in the order the editor shows them (`editor_text` splits on
/// newlines).
fn rows(block: &TextBlock, lines: &[TextLine]) -> Vec<Row> {
    let texts: Vec<&str> = block.editor_text.split('\n').collect();
    let mut out = Vec::with_capacity(block.rows.len());
    for (i, row) in block.rows.iter().enumerate() {
        let members: Vec<&TextLine> = row.iter().filter_map(|j| lines.get(*j)).collect();
        let Some(first) = members.first().copied() else { continue };
        let rect = members.iter().fold(first.rect, |acc, l| union(acc, l.rect));
        let runs: Vec<Run> = members.iter().map(|l| Run::of(l)).collect();
        let mixed = runs
            .iter()
            .any(|r| r.font != first.font || r.size != first.size || r.color != first.color || r.bold != first.bold || r.italic != first.italic);
        out.push(Row { text: texts.get(i).copied().unwrap_or_default().trim().to_owned(), rect, first: Run::of(first), runs, mixed });
    }
    out
}

/// What a `<b>` or `<i>` marker in a translation asks the source row for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Marker {
    Bold,
    Italic,
}

impl Marker {
    /// The run of `row` whose style this marker names. The style always comes from the source: a
    /// tag never invents one the row does not have.
    fn run_in(self, row: &Row) -> Option<usize> {
        row.runs.iter().position(|run| match self {
            Marker::Bold => run.bold,
            Marker::Italic => run.italic,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Marker::Bold => "bold",
            Marker::Italic => "italic",
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Marker::Bold => "b",
            Marker::Italic => "i",
        }
    }
}

/// The four tags a translation may carry, with the marker each opens or closes.
const TAGS: [(&str, Marker, bool); 4] =
    [("<b>", Marker::Bold, true), ("</b>", Marker::Bold, false), ("<i>", Marker::Italic, true), ("</i>", Marker::Italic, false)];

fn tag_at(text: &str) -> Option<(Marker, bool, &'static str)> {
    TAGS.into_iter().find(|(tag, _, _)| text.starts_with(tag)).map(|(tag, marker, opening)| (marker, opening, tag))
}

/// Whether a translation carries any markup at all. A string without one of the four tags is drawn
/// as it is, which is what every map written before the markup existed contains.
fn has_markup(translation: &str) -> bool {
    TAGS.iter().any(|(tag, _, _)| translation.contains(tag))
}

/// A translation with its markup taken out: what to draw, and the span each marker covers.
#[derive(Clone, Debug)]
struct Marked {
    text: String,
    spans: Vec<(std::ops::Range<usize>, Marker)>,
}

/// Split `translation` on `<b>`/`<i>`. `Ok(None)` when it carries no markup at all.
///
/// Only these four tags are understood: any other `<` is refused rather than guessed at, as is
/// nesting (a run has one style) and a closing tag that closes nothing.
fn markup(translation: &str) -> std::result::Result<Option<Marked>, String> {
    if !has_markup(translation) {
        return Ok(None);
    }
    let mut text = String::with_capacity(translation.len());
    let mut spans: Vec<(std::ops::Range<usize>, Marker)> = Vec::new();
    let mut open: Option<(Marker, usize)> = None;
    let mut rest = translation;
    while let Some(at) = rest.find('<') {
        text.push_str(&rest[..at]);
        let tail = &rest[at..];
        let Some((marker, opening, tag)) = tag_at(tail) else {
            return Err("a '<' that is not one of <b>, </b>, <i> or </i>".to_owned());
        };
        match (opening, open) {
            (true, None) => open = Some((marker, text.len())),
            (true, Some((outer, _))) => return Err(format!("{tag} opens inside an unclosed <{}> tag", outer.tag())),
            (false, Some((opened, start))) if opened == marker => {
                if start < text.len() {
                    spans.push((start..text.len(), marker));
                }
                open = None;
            }
            (false, Some((opened, _))) => return Err(format!("<{}> is closed by {tag}", opened.tag())),
            (false, None) => return Err(format!("{tag} closes nothing")),
        }
        rest = &tail[tag.len()..];
    }
    if let Some((marker, _)) = open {
        return Err(format!("the <{}> tag is never closed", marker.tag()));
    }
    text.push_str(rest);
    Ok(Some(Marked { text, spans }))
}

/// The translation with any markup tags removed, so a map whose markup cannot be honoured still
/// never draws a tag as text.
fn without_tags(translation: &str) -> String {
    if !has_markup(translation) {
        return translation.to_owned();
    }
    let mut out = String::with_capacity(translation.len());
    let mut rest = translation;
    while let Some(at) = rest.find('<') {
        out.push_str(&rest[..at]);
        match tag_at(&rest[at..]) {
            Some((_, _, tag)) => rest = &rest[at + tag.len()..],
            None => {
                out.push('<');
                rest = &rest[at + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// What one row should say after the rewrite: the text to draw and, for every marked span, the run
/// of the source row whose style it takes. `problem` is set when the map's markup could not be
/// honoured, so the loss is reported instead of being silent.
#[derive(Clone, Debug)]
struct Wanted {
    text: String,
    spans: Vec<(std::ops::Range<usize>, usize)>,
    problem: Option<String>,
}

/// Resolve a row's translation: take the markup out and replace every marker by the style of a run
/// the source row really has. A marker the row cannot satisfy is a problem, never a guess.
///
/// `forced_font` means `font_resource` draws the whole paragraph in one face, so no span can keep a
/// run's weight; markup is then reported as unusable rather than drawn as if it had worked.
fn wanted_for(row: &Row, translation: &str, forced_font: bool) -> Wanted {
    let marked = match markup(translation) {
        Ok(marked) => marked,
        Err(why) => {
            return Wanted { text: without_tags(translation), spans: Vec::new(), problem: Some(format!("the map's markup is not usable: {why}")) };
        }
    };
    let Some(marked) = marked else {
        return Wanted { text: translation.to_owned(), spans: Vec::new(), problem: None };
    };
    let problem = if forced_font {
        Some("font_resource draws the whole paragraph in one face, so the map's markup cannot be honoured".to_owned())
    } else if marked.spans.len() > row.runs.len() {
        Some(format!(
            "the map marks {} spans with markers but the row has only {} runs, so there is no source style for each of them",
            marked.spans.len(),
            row.runs.len()
        ))
    } else {
        marked.spans.iter().find_map(|(_, marker)| match marker.run_in(row) {
            Some(_) => None,
            None => Some(format!("the map marks <{}> but the row has no {} run to take that style from", marker.tag(), marker.name())),
        })
    };
    let spans = if problem.is_some() {
        Vec::new()
    } else {
        marked.spans.iter().filter_map(|(span, marker)| marker.run_in(row).map(|run| (span.clone(), run))).collect()
    };
    Wanted { text: marked.text, spans, problem }
}

/// A user-space rectangle as the displayed-page rectangle every tool reports and accepts: origin at
/// the top-left after `/Rotate`, rounded to 1/100 pt.
fn view_rect(info: &PageInfo, r: [f64; 4]) -> [f32; 4] {
    let (a, b) = (info.user_to_view(r[0] as f32, r[1] as f32), info.user_to_view(r[2] as f32, r[3] as f32));
    let round = |v: f32| (v * 100.0).round() / 100.0;
    [round(a[0].min(b[0])), round(a[1].min(b[1])), round(a[0].max(b[0])), round(a[1].max(b[1]))]
}

/// A report entry for a row of a paragraph: the page, the box, the source text and the paragraph
/// and row numbers.
fn entry(info: &PageInfo, page: usize, paragraph: usize, number: usize, rect: [f64; 4], text: &str) -> Entry {
    Entry {
        page: page + 1,
        rect: view_rect(info, rect),
        text: tr::quote(text),
        paragraph: paragraph + 1,
        row: Some(number + 1),
        translation: None,
        size: None,
        kept: None,
        reason: None,
        error: None,
    }
}

/// Whether two boxes are the same box, to the tolerance a rewrite must stay inside.
fn near(a: [f64; 4], b: [f64; 4]) -> bool {
    a.iter().zip(b.iter()).all(|(x, y)| (x - y).abs() <= 0.5)
}

/// How far beyond the paragraph's band a translation may reach, as a fraction of the row's size: a
/// face with taller metrics than the source's (Inter's descenders, say) sits a little outside it
/// without moving anything. Wrapping to another row overshoots by far more and is still caught.
const METRICS_SLACK: f64 = 0.15;

/// Whether a drawn box stays inside `block` across the page and inside `source` up it, within the
/// tolerance a rewrite is held to.
fn inside_band(drawn: [f64; 4], source: [f64; 4], block: [f64; 4], size: f64) -> bool {
    let slack = 0.5 + METRICS_SLACK * size.max(0.0);
    drawn[0] >= block[0] - 0.5 && drawn[2] <= block[2] + 0.5 && drawn[1] >= source[1] - slack && drawn[3] <= source[3] + slack
}

/// The tally a report is built from.
#[derive(Default)]
struct Tally {
    rows: usize,
    translated: usize,
    unchanged: usize,
    invisible: usize,
    skipped: usize,
    missing: usize,
    too_long: usize,
    failed: usize,
    formatting_lost: usize,
    reverted: usize,
    shrunk: usize,
}

/// What one paragraph's rewrite should contain: the draft text and the style of each of its rows.
struct Plan {
    draft: String,
    ranges: Vec<RangeStyle>,
}

impl Plan {
    /// The draft for `paragraph` with the translations in `wanted` (one entry per row; `None` keeps
    /// that row as it is) and, for the rows that need it, an explicit `sizes` entry.
    ///
    /// Each row keeps its own font resource (a heading stays bold, a caption italic), its own size,
    /// colour and box, and a row whose translation marks a span keeps that span in the style of the
    /// source run the marker named — so a bold word inside a sentence is drawn bold.
    fn of(paragraph: &[Row], wanted: &[Option<Wanted>], sizes: &[Option<f64>], font_resource: Option<&str>) -> Self {
        let mut draft = String::new();
        let mut ranges = Vec::with_capacity(paragraph.len());
        for (i, row) in paragraph.iter().enumerate() {
            if i > 0 {
                draft.push('\n');
            }
            let start = draft.len();
            let wanted = wanted.get(i).and_then(Option::as_ref);
            draft.push_str(wanted.map_or(row.text.as_str(), |w| w.text.as_str()));
            let end = draft.len();
            // A forced font replaces every run's face (it is the only way to write a script the
            // document's own fonts lack); otherwise each span keeps the run it names, which is what
            // keeps a heading bold and a caption italic.
            let forced = sizes.get(i).copied().flatten();
            let styled = |range: std::ops::Range<usize>, run: &Run| RangeStyle {
                range,
                family: None,
                resource: Some(font_resource.map_or_else(|| run.font.clone(), str::to_owned)),
                size: Some(forced.unwrap_or(run.size)),
                color: Some(run.color),
                underline: None,
                char_spacing: None,
                scale: None,
            };
            let mut at = start;
            for (span, run) in wanted.map_or(&[][..], |w| &w.spans[..]) {
                let from = start + span.start;
                let to = start + span.end;
                if from > at {
                    ranges.push(styled(at..from, &row.first));
                }
                if to > from {
                    ranges.push(styled(from..to, row.runs.get(*run).unwrap_or(&row.first)));
                }
                at = at.max(to);
            }
            if at < end {
                ranges.push(styled(at..end, &row.first));
            }
        }
        Self { draft, ranges }
    }
}

fn document(session: &Session, id: DocId) -> Result<&Document> {
    session.get(id).ok_or_else(|| Error::Failed("the document is no longer open".into()))
}

/// Translate the document's text rows in place.
///
/// Every translation is collected before the first edit, so a backend that cannot answer leaves the
/// document exactly as it was. A page's last paragraph goes first: a rewrite replaces that
/// paragraph's glyphs where they are, which cannot renumber the ones before it.
pub fn translate(session: &mut Session, id: DocId, options: &Options, backend: &mut dyn Backend) -> Result<Report> {
    // 1. Arguments, before a page is read.
    let page_count = document(session, id)?.info.pages.len();
    if page_count == 0 {
        return Err(Error::Failed("the document has no pages".into()));
    }
    let pages: Vec<usize> = if options.pages.is_empty() {
        (0..page_count).collect()
    } else {
        let mut out: Vec<usize> = Vec::with_capacity(options.pages.len());
        for &page in &options.pages {
            if page >= page_count {
                return Err(Error::Invalid(format!("page {} is out of range: the document has {page_count} pages", page + 1)));
            }
            if !out.contains(&page) {
                out.push(page);
            }
        }
        out.sort_unstable();
        out
    };
    if !(0.5..=144.0).contains(&options.min_size) {
        return Err(Error::Invalid("min_size must be a font size in points from 0.5 to 144".into()));
    }
    let shrink = options.shrink;
    let min_size = options.min_size;
    let font_resource = options.font_resource.as_deref();

    // 2. What every row says, and its translation, before the first edit: a backend that cannot
    //    answer must not leave half a document translated.
    let mut paragraphs: Vec<(usize, usize, TextBlock, Vec<Row>)> = Vec::new();
    for &page in &pages {
        let lines = document(session, id)?.text_lines(page);
        for (index, block) in document(session, id)?.text_blocks(page).into_iter().enumerate() {
            let rows = rows(&block, &lines);
            paragraphs.push((page, index, block, rows));
        }
    }
    let mut translations: BTreeMap<String, Option<String>> = BTreeMap::new();
    for (_, _, block, rows) in &paragraphs {
        if block.invisible {
            continue;
        }
        for row in rows {
            let source = tr::key(&row.text);
            if source.is_empty() || translations.contains_key(&source) {
                continue;
            }
            let translated = backend.translate(&source).map_err(|e| match e {
                BackendError::Invalid(message) => Error::Invalid(message),
                BackendError::Network(message) => Error::Failed(format!("could not translate {}: {message}", tr::quote(&source))),
            })?;
            translations.insert(source, translated);
        }
    }

    // 3. Rewrite paragraph by paragraph.
    let mut tally = Tally { rows: paragraphs.iter().map(|(_, _, _, rows)| rows.len()).sum(), ..Tally::default() };
    let mut missing: Vec<Entry> = Vec::new();
    let mut too_long: Vec<Entry> = Vec::new();
    let mut shrunk_rows: Vec<Entry> = Vec::new();
    let mut failed_rows: Vec<Entry> = Vec::new();
    let mut formatting: Vec<Entry> = Vec::new();
    let mut fonts: BTreeSet<String> = BTreeSet::new();
    for &page in &pages {
        // The page's own user-space → displayed-page conversion, so a report entry carries the same
        // coordinates `text_paragraphs` reports.
        let Some(info) = document(session, id)?.info.pages.get(page).cloned() else { continue };
        let on_page: Vec<(usize, TextBlock, Vec<Row>)> =
            paragraphs.iter().filter(|(p, _, _, _)| *p == page).map(|(_, i, b, rows)| (*i, b.clone(), rows.clone())).collect();
        let recorded: BTreeMap<usize, [f64; 4]> = on_page.iter().map(|(i, b, _)| (*i, b.rect)).collect();
        let invisible: BTreeSet<usize> = on_page.iter().filter(|(_, b, _)| b.invisible).map(|(i, _, _)| *i).collect();
        tally.invisible += invisible.len();
        for (index, block, source_rows) in on_page.iter().rev() {
            let index = *index;
            // A paragraph that moved since it was measured is left alone rather than corrupted.
            let Some(current) = document(session, id)?.text_blocks(page).get(index).cloned() else { continue };
            if !recorded.get(&index).is_some_and(|rect| near(*rect, current.rect)) || invisible.contains(&index) {
                tally.skipped += 1;
                continue;
            }
            // What this paragraph should say, row by row.
            let mut wanted: Vec<Option<Wanted>> = Vec::with_capacity(source_rows.len());
            for (i, row) in source_rows.iter().enumerate() {
                let source = tr::key(&row.text);
                if source.is_empty() {
                    wanted.push(None);
                    continue;
                }
                match translations.get(&source).and_then(|t| t.clone()) {
                    Some(translated) => {
                        // The map may mark a span with <b>/<i>; the style comes from the run the
                        // source row really has, never from the tag.
                        let row_wanted = wanted_for(row, &translated, font_resource.is_some());
                        if row_wanted.text == row.text {
                            tally.unchanged += 1;
                            wanted.push(None);
                        } else {
                            wanted.push(Some(row_wanted));
                        }
                    }
                    None => {
                        tally.missing += 1;
                        if missing.len() < MAX_REPORTED {
                            let problem = entry(&info, page, index, i, row.rect, &row.text);
                            missing.push(problem);
                        }
                        wanted.push(None);
                    }
                }
            }
            let style_for =
                |ranges: &[RangeStyle]| BlockStyle { ranges: ranges.to_vec(), resource: font_resource.map(str::to_owned), ..BlockStyle::default() };
            // The largest size in the paragraph, which is how much a face's taller metrics may
            // reach beyond the paragraph's band without counting as an overflow.
            let largest = source_rows.iter().map(|r| r.first.size).fold(0.0_f64, f64::max);
            // Does this draft stay inside the paragraph's box? Measured with the rewrite Apply
            // runs, so what measures as fitting does fit.
            let fits = |wanted: &[Option<Wanted>], sizes: &[Option<f64>]| -> Result<bool> {
                let plan = Plan::of(source_rows, wanted, sizes, font_resource);
                let layout = document(session, id)?
                    .text_edit_layout(page, index, &plan.draft, &style_for(&plan.ranges))
                    .map_err(|e| Error::Failed(format!("page {}: {e}", page + 1)))?;
                Ok(layout.draft_rect.is_none_or(|draft| inside_band(draft, block.rect, block.rect, largest)))
            };
            let mut sizes: Vec<Option<f64>> = vec![None; source_rows.len()];
            let mut accepted = wanted.clone();
            if !accepted.iter().any(Option::is_some) {
                continue;
            }
            if !fits(&accepted, &sizes)? {
                // One row that does not fit must not cost the paragraph the rows that do: measure
                // each translated row on its own and keep the ones that stay inside its box.
                let mut kept: Vec<Option<Wanted>> = vec![None; source_rows.len()];
                for (i, row) in source_rows.iter().enumerate() {
                    let Some(wanted) = accepted.get(i).cloned().flatten() else { continue };
                    let translation = wanted.text.clone();
                    let mut single: Vec<Option<Wanted>> = vec![None; source_rows.len()];
                    single[i] = Some(wanted.clone());
                    if fits(&single, &sizes)? {
                        kept[i] = Some(wanted);
                        continue;
                    }
                    // Only a row that will not fit at its own size may be drawn smaller, and only
                    // when the caller asked for it: shrinking is never silent.
                    let chosen = if shrink {
                        let mut chosen = None;
                        for candidate in tr::candidates(row.first.size, min_size) {
                            let mut attempt = sizes.clone();
                            attempt[i] = Some(candidate);
                            if fits(&single, &attempt)? {
                                chosen = Some(candidate);
                                break;
                            }
                        }
                        chosen
                    } else {
                        None
                    };
                    match chosen {
                        Some(candidate) => {
                            sizes[i] = Some(candidate);
                            kept[i] = Some(wanted);
                            tally.shrunk += 1;
                            if shrunk_rows.len() < MAX_REPORTED {
                                let mut note = entry(&info, page, index, i, row.rect, &row.text);
                                note.translation = Some(tr::quote(&translation));
                                note.size = Some((candidate * 100.0).round() / 100.0);
                                note.reason = Some(
                                    "the translation does not fit the row at its own size, so it is drawn at this one because shrink was asked for"
                                        .to_owned(),
                                );
                                shrunk_rows.push(note);
                            }
                        }
                        None => {
                            tally.too_long += 1;
                            if too_long.len() < MAX_REPORTED {
                                let mut note = entry(&info, page, index, i, row.rect, &row.text);
                                note.translation = Some(tr::quote(&translation));
                                note.size = Some((row.first.size * 100.0).round() / 100.0);
                                note.reason = Some(if shrink {
                                    "the translation does not fit the row even at min_size, so the row is left as it was".to_owned()
                                } else {
                                    "the translation does not fit the row at its own size, so the row is left as it was; allowing shrink would draw a smaller one"
                                        .to_owned()
                                });
                                too_long.push(note);
                            }
                        }
                    }
                }
                // The rows that each fit must also fit together: drop from the last until they do,
                // so what is applied is exactly what was measured.
                accepted = kept;
                while accepted.iter().any(Option::is_some) && !fits(&accepted, &sizes)? {
                    let Some(last) = (0..accepted.len()).rev().find(|i| accepted[*i].is_some()) else { break };
                    let translation = accepted[last].as_ref().map(|w| w.text.clone()).unwrap_or_default();
                    tally.too_long += 1;
                    if too_long.len() < MAX_REPORTED
                        && let Some(row) = source_rows.get(last)
                    {
                        let mut note = entry(&info, page, index, last, row.rect, &row.text);
                        note.translation = Some(tr::quote(&translation));
                        note.reason = Some("the translation leaves the paragraph's box once the other rows are placed".to_owned());
                        too_long.push(note);
                    }
                    accepted[last] = None;
                }
                if !accepted.iter().any(Option::is_some) {
                    continue;
                }
            }
            let plan = Plan::of(source_rows, &accepted, &sizes, font_resource);
            for (i, row) in source_rows.iter().enumerate().filter(|(i, _)| accepted.get(*i).is_some_and(Option::is_some)) {
                let wanted = accepted.get(i).and_then(Option::as_ref);
                // A row is only a loss when the translation could not carry its styles: the map's
                // markup names the runs, so a marked row keeps them.
                let reason = match wanted {
                    Some(wanted) if wanted.problem.is_some() => wanted.problem.clone(),
                    Some(wanted) if !wanted.spans.is_empty() => None,
                    _ if row.mixed => Some("the row mixes several styles; the translation keeps the first run's".to_owned()),
                    _ => None,
                };
                if let Some(reason) = reason {
                    tally.formatting_lost += 1;
                    if formatting.len() < MAX_REPORTED {
                        let mut problem = entry(&info, page, index, i, row.rect, &row.text);
                        problem.kept = Some(format!("{} pt {}", row.first.size, row.first.base_font));
                        problem.reason = Some(reason);
                        formatting.push(problem);
                    }
                }
            }
            session
                .apply(id, Edit::EditTextBlock { page, block: index, text: plan.draft.clone(), style: style_for(&plan.ranges) })
                .map_err(|e| Error::Failed(e.to_string()))?;
            tally.translated += 1;
            // Verify what was drawn: the same number of rows, each in its own line of the box. A
            // translation that re-wrapped the paragraph is put back as it was, and reported.
            let lines_after = document(session, id)?.text_lines(page);
            let after = document(session, id)?.text_blocks(page);
            let drawn = after.get(index).map(|b| rows(b, &lines_after)).unwrap_or_default();
            let same_shape = drawn.len() == source_rows.len()
                && drawn.iter().zip(source_rows.iter()).all(|(d, s)| inside_band(d.rect, s.rect, block.rect, s.first.size));
            if !same_shape {
                let unchanged = Plan::of(source_rows, &[], &[], font_resource);
                session
                    .apply(id, Edit::EditTextBlock { page, block: index, text: unchanged.draft, style: style_for(&unchanged.ranges) })
                    .map_err(|e| Error::Failed(e.to_string()))?;
                tally.translated = tally.translated.saturating_sub(1);
                tally.reverted += 1;
                tally.failed += 1;
                if failed_rows.len() < MAX_REPORTED {
                    let mut problem = entry(&info, page, index, 0, block.rect, &plan.draft);
                    problem.row = None;
                    problem.error = Some("the translation changes the paragraph's line breaks, so it was put back unchanged".to_owned());
                    failed_rows.push(problem);
                }
                continue;
            }
            if let Some(block) = after.get(index)
                && fonts.len() < MAX_FONTS
                && !fonts.contains(&block.base_font)
            {
                fonts.insert(block.base_font.clone());
            }
        }
    }

    // Text an edit could store as typed but only draw with `?` (its face cannot show it) is said,
    // not silent.
    let warnings: Vec<String> = document(session, id)?.edit_notices.iter().map(ToString::to_string).collect();
    Ok(Report {
        pages: pages.iter().map(|p| p + 1).collect(),
        rows: tally.rows,
        translated: tally.translated,
        unchanged: tally.unchanged,
        shrunk: tally.shrunk,
        shrunk_rows: ProblemList { count: tally.shrunk, rows: shrunk_rows },
        invisible: tally.invisible,
        skipped: tally.skipped,
        reverted: tally.reverted,
        missing: ProblemList { count: tally.missing, rows: missing },
        too_long: ProblemList { count: tally.too_long, rows: too_long },
        failed: ProblemList { count: tally.failed, rows: failed_rows },
        formatting_lost: ProblemList { count: tally.formatting_lost, rows: formatting },
        provider: backend.provider(),
        network_requests: backend.requests(),
        fonts: fonts.into_iter().collect(),
        warnings,
    })
}
