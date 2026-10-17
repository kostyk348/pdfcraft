//! `doc_translate`: replace a page's text with its translation, keeping every run's formatting.
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
//! - a row that holds runs in several styles (a bold word inside a sentence). One translation can
//!   be drawn in only one of them, so the row keeps its first run's style and is listed as
//!   `formatting_lost`.
//! - a translation whose font cannot show a character: the engine's own notice list is passed on.
//!
//! Translations are collected for the whole run before the first edit, so a backend that cannot
//! answer leaves the document untouched. Only the `network` argument can make this reach a socket.

use std::collections::{BTreeMap, BTreeSet};

use pdfcraft_engine::{BlockStyle, Edit, RangeStyle, TextBlock, TextLine};
use pdfcraft_render::PageInfo;
use pdfcraft_translate as tr;
use pdfcraft_translate::Translator as _;
use serde_json::{Value, json};

use crate::{Args, Automation, Result, ToolError, failed, summary};

/// Most entries one problem list holds, so a pathological document cannot answer with megabytes of
/// JSON. The count beside each list is the real one.
const MAX_REPORTED: usize = 100;
/// Most distinct fonts a report lists.
const MAX_FONTS: usize = 8;
/// Most bytes a translation map file may hold.
const MAX_MAP_BYTES: usize = tr::MAX_MAP_BYTES;

/// One backend, or two: the offline map answers first and the endpoint covers what it lacks.
enum Backend {
    Map(tr::Map),
    Network(tr::Deeplx),
    Both(tr::Map, tr::Deeplx),
}

impl tr::Translator for Backend {
    fn translate(&mut self, text: &str) -> tr::Result<Option<String>> {
        match self {
            Backend::Map(map) => map.translate(text),
            Backend::Network(network) => network.translate(text),
            Backend::Both(map, network) => Ok(match map.translate(text)? {
                Some(translated) => Some(translated),
                None => network.translate(text)?,
            }),
        }
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

/// One visual row: the text it shows and the style it is drawn in. A row is the unit of
/// translation, so its size, colour, font and box survive the rewrite.
#[derive(Clone, Debug)]
struct Row {
    text: String,
    rect: [f64; 4],
    /// Size in points, as the row is drawn.
    size: f64,
    color: [f64; 3],
    /// The page's font resource this row uses, which is what keeps a heading bold.
    font: String,
    base_font: String,
    /// The row holds runs in more than one style, so a translation can keep only the first.
    mixed: bool,
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
        let mixed = members
            .iter()
            .any(|l| l.font != first.font || l.size != first.size || l.color != first.color || l.bold != first.bold || l.italic != first.italic);
        out.push(Row {
            text: texts.get(i).copied().unwrap_or_default().trim().to_owned(),
            rect,
            size: first.size,
            color: first.color,
            font: first.font.clone(),
            base_font: first.base_font.clone(),
            mixed,
        });
    }
    out
}

/// A report entry: where a row is (top-left-origin points of the displayed page, the same
/// coordinates every other tool reports and accepts), what it says, and — for a problem — why it
/// was left alone.
fn entry(info: &PageInfo, page: usize, rect: [f64; 4], text: &str) -> Value {
    let r = crate::comments::rect_to_view(info, [rect[0] as f32, rect[1] as f32, rect[2] as f32, rect[3] as f32]);
    json!({
        "page": page + 1,
        "rect": [r[0], r[1], r[2], r[3]],
        "text": tr::quote(text),
    })
}

/// A report entry for one row of a paragraph: the paragraph and row number, the box, the source
/// text and the translation that could not be placed.
fn row_entry(info: &PageInfo, page: usize, paragraph: usize, number: usize, row: &Row, translation: &str) -> Value {
    let mut value = entry(&info, page, row.rect, &row.text);
    value["paragraph"] = json!(paragraph + 1);
    value["row"] = json!(number + 1);
    value["translation"] = json!(tr::quote(translation));
    value
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

/// A problem list with its true size.
fn list(entries: Vec<Value>, count: usize) -> Value {
    json!({ "count": count, "paragraphs": entries })
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
    /// colour and box, so what the engine draws is the source layout with other words in it.
    fn of(paragraph: &[Row], wanted: &[Option<String>], sizes: &[Option<f64>], font_resource: Option<&str>) -> Self {
        let mut draft = String::new();
        let mut ranges = Vec::with_capacity(paragraph.len());
        for (i, row) in paragraph.iter().enumerate() {
            if i > 0 {
                draft.push('\n');
            }
            let start = draft.len();
            let replacement = wanted.get(i).and_then(|t| t.as_deref());
            draft.push_str(replacement.unwrap_or(&row.text));
            ranges.push(RangeStyle {
                range: start..draft.len(),
                family: None,
                // A forced font replaces every row's face (it is the only way to write a script the
                // document's own fonts lack); otherwise each row keeps its own resource, which is
                // what keeps a heading bold and a caption italic.
                resource: Some(font_resource.map_or_else(|| row.font.clone(), str::to_owned)),
                size: Some(sizes.get(i).copied().flatten().unwrap_or(row.size)),
                color: Some(row.color),
                underline: None,
                char_spacing: None,
                scale: None,
            });
        }
        Self { draft, ranges }
    }
}

pub(crate) fn doc_translate(auto: &mut Automation, a: &Args) -> Result<Value> {
    // 1. Arguments and backends, before a page is read.
    let page_count = auto.doc(a)?.info.pages.len();
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

    // 2. What every row says, and its translation, before the first edit: a backend that cannot
    //    answer must not leave half a document translated.
    let mut paragraphs: Vec<(usize, usize, TextBlock, Vec<Row>)> = Vec::new();
    for &page in &pages {
        let lines = auto.doc(a)?.text_lines(page);
        for (index, block) in auto.doc(a)?.text_blocks(page).into_iter().enumerate() {
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
                tr::Error::Invalid(message) => ToolError::InvalidArgs(message),
                tr::Error::Network(message) => failed(format!("could not translate {}: {message}", tr::quote(&source))),
            })?;
            translations.insert(source, translated);
        }
    }

    // 3. Rewrite paragraph by paragraph. Within a page the last paragraph goes first: a rewrite
    //    replaces that paragraph's glyphs where they are, which cannot renumber the ones before it.
    let mut tally = Tally { rows: paragraphs.iter().map(|(_, _, _, rows)| rows.len()).sum(), ..Tally::default() };
    let mut missing = Vec::new();
    let mut too_long = Vec::new();
    let mut shrunk_rows = Vec::new();
    let mut failed_rows = Vec::new();
    let mut formatting = Vec::new();
    let mut fonts: BTreeSet<String> = BTreeSet::new();
    for &page in &pages {
        // The page's own user-space → displayed-page conversion, so a report entry carries the same
        // coordinates `text_paragraphs` reports.
        let Some(info) = auto.doc(a)?.info.pages.get(page).cloned() else { continue };
        let on_page: Vec<(usize, TextBlock, Vec<Row>)> =
            paragraphs.iter().filter(|(p, _, _, _)| *p == page).map(|(_, i, b, rows)| (*i, b.clone(), rows.clone())).collect();
        let recorded: BTreeMap<usize, [f64; 4]> = on_page.iter().map(|(i, b, _)| (*i, b.rect)).collect();
        let invisible: BTreeSet<usize> = on_page.iter().filter(|(_, b, _)| b.invisible).map(|(i, _, _)| *i).collect();
        tally.invisible += invisible.len();
        for (index, block, source_rows) in on_page.iter().rev() {
            let index = *index;
            // A paragraph that moved since it was measured is left alone rather than corrupted.
            let Some(current) = auto.doc(a)?.text_blocks(page).get(index).cloned() else { continue };
            if !recorded.get(&index).is_some_and(|rect| near(*rect, current.rect)) || invisible.contains(&index) {
                tally.skipped += 1;
                continue;
            }
            // What this paragraph should say, row by row.
            let mut wanted: Vec<Option<String>> = Vec::with_capacity(source_rows.len());
            for (i, row) in source_rows.iter().enumerate() {
                let source = tr::key(&row.text);
                if source.is_empty() {
                    wanted.push(None);
                    continue;
                }
                match translations.get(&source).and_then(|t| t.clone()) {
                    Some(translated) if translated != row.text => wanted.push(Some(translated)),
                    Some(_) => {
                        tally.unchanged += 1;
                        wanted.push(None);
                    }
                    None => {
                        tally.missing += 1;
                        if missing.len() < MAX_REPORTED {
                            let mut problem = entry(&info, page, row.rect, &row.text);
                            problem["paragraph"] = json!(index + 1);
                            problem["row"] = json!(i + 1);
                            missing.push(problem);
                        }
                        wanted.push(None);
                    }
                }
            }
            let style_for = |ranges: &[RangeStyle]| BlockStyle { ranges: ranges.to_vec(), resource: font_resource.clone(), ..BlockStyle::default() };
            // The largest size in the paragraph, which is how much a face's taller metrics may
            // reach beyond the paragraph's band without counting as an overflow.
            let largest = source_rows.iter().map(|r| r.size).fold(0.0_f64, f64::max);
            // Does this draft stay inside the paragraph's box? Measured with the rewrite Apply
            // runs, so what measures as fitting does fit.
            let fits = |wanted: &[Option<String>], sizes: &[Option<f64>]| -> Result<bool> {
                let plan = Plan::of(source_rows, wanted, sizes, font_resource.as_deref());
                let layout = auto
                    .doc(a)
                    .map_err(|e| failed(e.to_string()))?
                    .text_edit_layout(page, index, &plan.draft, &style_for(&plan.ranges))
                    .map_err(|e| failed(format!("page {}: {e}", page + 1)))?;
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
                let mut kept: Vec<Option<String>> = vec![None; source_rows.len()];
                for (i, row) in source_rows.iter().enumerate() {
                    let Some(translation) = accepted.get(i).cloned().flatten() else { continue };
                    let mut single: Vec<Option<String>> = vec![None; source_rows.len()];
                    single[i] = Some(translation.clone());
                    if fits(&single, &sizes)? {
                        kept[i] = Some(translation);
                        continue;
                    }
                    // Only a row that will not fit at its own size may be drawn smaller, and only
                    // when the caller asked for it: shrinking is never silent.
                    let chosen = if shrink {
                        let mut chosen = None;
                        for candidate in tr::candidates(row.size, min_size) {
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
                            kept[i] = Some(translation.clone());
                            tally.shrunk += 1;
                            if shrunk_rows.len() < MAX_REPORTED {
                                let mut note = row_entry(&info, page, index, i, row, &translation);
                                note["size"] = json!((candidate * 100.0).round() / 100.0);
                                note["reason"] = json!(
                                    "the translation does not fit the row at its own size, so it is drawn at this one because shrink was asked for"
                                );
                                shrunk_rows.push(note);
                            }
                        }
                        None => {
                            tally.too_long += 1;
                            if too_long.len() < MAX_REPORTED {
                                let mut note = row_entry(&info, page, index, i, row, &translation);
                                note["size"] = json!((row.size * 100.0).round() / 100.0);
                                note["reason"] = if shrink {
                                    json!("the translation does not fit the row even at min_size, so the row is left as it was")
                                } else {
                                    json!(
                                        "the translation does not fit the row at its own size, so the row is left as it was; pass shrink: true to allow a smaller one"
                                    )
                                };
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
                    let translation = accepted[last].clone().unwrap_or_default();
                    tally.too_long += 1;
                    if too_long.len() < MAX_REPORTED
                        && let Some(row) = source_rows.get(last)
                    {
                        let mut note = row_entry(&info, page, index, last, row, &translation);
                        note["reason"] = json!("the translation leaves the paragraph's box once the other rows are placed");
                        too_long.push(note);
                    }
                    accepted[last] = None;
                }
                if !accepted.iter().any(Option::is_some) {
                    continue;
                }
            }
            let plan = Plan::of(source_rows, &accepted, &sizes, font_resource.as_deref());
            for (i, row) in source_rows.iter().enumerate().filter(|(i, _)| accepted.get(*i).is_some_and(Option::is_some)) {
                if row.mixed {
                    tally.formatting_lost += 1;
                    if formatting.len() < MAX_REPORTED {
                        let mut problem = entry(&info, page, row.rect, &row.text);
                        problem["paragraph"] = json!(index + 1);
                        problem["row"] = json!(i + 1);
                        problem["kept"] = json!(format!("{} pt {}", row.size, row.base_font));
                        problem["reason"] = json!("the row mixes several styles; the translation keeps the first run's");
                        formatting.push(problem);
                    }
                }
            }
            auto.apply(a, Edit::EditTextBlock { page, block: index, text: plan.draft.clone(), style: style_for(&plan.ranges) })?;
            tally.translated += 1;
            // Verify what was drawn: the same number of rows, each in its own line of the box. A
            // translation that re-wrapped the paragraph is put back as it was, and reported.
            let lines_after = auto.doc(a)?.text_lines(page);
            let after = auto.doc(a)?.text_blocks(page);
            let drawn = after.get(index).map(|b| rows(b, &lines_after)).unwrap_or_default();
            let same_shape = drawn.len() == source_rows.len()
                && drawn.iter().zip(source_rows.iter()).all(|(d, s)| inside_band(d.rect, s.rect, block.rect, s.size));
            if !same_shape {
                let unchanged = Plan::of(source_rows, &[], &[], font_resource.as_deref());
                auto.apply(a, Edit::EditTextBlock { page, block: index, text: unchanged.draft, style: style_for(&unchanged.ranges) })?;
                tally.translated = tally.translated.saturating_sub(1);
                tally.reverted += 1;
                tally.failed += 1;
                if failed_rows.len() < MAX_REPORTED {
                    let mut problem = entry(&info, page, block.rect, &plan.draft);
                    problem["paragraph"] = json!(index + 1);
                    problem["error"] = json!("the translation changes the paragraph's line breaks, so it was put back unchanged");
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

    // 4. Optionally write the result as its own file: a full rewrite, pushed atomically, so the
    //    source document is never touched and a failure leaves no half-written PDF.
    let written = match a.opt_str("out")? {
        Some(path) => {
            let bytes = auto.session.save_full_bytes(auto.doc(a)?.id).map_err(failed)?;
            let target = auto.write_output(path, &bytes)?;
            Some(json!({ "path": target.to_string_lossy(), "bytes": bytes.len() }))
        }
        None => None,
    };

    let mut report = json!({
        "pages": pages.iter().map(|p| p + 1).collect::<Vec<_>>(),
        "rows": tally.rows,
        "translated": tally.translated,
        "unchanged": tally.unchanged,
        "shrunk": tally.shrunk,
        "shrunk_rows": list(shrunk_rows, tally.shrunk),
        "invisible": tally.invisible,
        "skipped": tally.skipped,
        "reverted": tally.reverted,
        "missing": list(missing, tally.missing),
        "too_long": list(too_long, tally.too_long),
        "failed": list(failed_rows, tally.failed),
        "formatting_lost": list(formatting, tally.formatting_lost),
        "network_requests": backend.requests(),
        "fonts": fonts.iter().collect::<Vec<_>>(),
        "document": summary(auto.doc(a)?),
    });
    if let Some(written) = written {
        report["out"] = written;
    }
    // Text an edit could store as typed but only draw with `?` (its face cannot show it) is said,
    // not silent.
    let notices: Vec<String> = auto.doc(a)?.edit_notices.iter().map(ToString::to_string).collect();
    if !notices.is_empty() {
        report["warnings"] = json!(notices);
    }
    Ok(report)
}
