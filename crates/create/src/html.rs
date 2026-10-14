//! An HTML body laid out as a real PDF: `create.from-html`.
//!
//! An email (or a `.html` file) carries structure, and reducing it to text loses that structure.
//! This module reads a **bounded subset** of HTML into blocks and lays those blocks out as a PDF:
//! headings, paragraphs, line breaks, lists, block quotes, preformatted text, rules and simple
//! tables — wrapped with the standard-14 metrics in `pdfcraft-fonts`, spaced and paginated here.
//! It is the same machinery `from_text` uses (the standard fonts, WinAnsiEncoding, a content
//! stream per page), not a second rendering engine and not a third-party library.
//!
//! What the subset is, exactly — anything else is ignored rather than guessed at:
//!
//! * `h1`–`h3` are set in Helvetica-Bold at 20 / 16 / 13.5 pt; `h4`–`h6` at body size, bold;
//! * `p`, `div` and the other block containers are paragraphs; `br` breaks a line;
//! * `ul`/`ol`/`li` become bullets and numbers, indented per level, with a hanging indent;
//! * `blockquote` is indented with a rule down its left side;
//! * `pre` is Courier, indented, with its whitespace and blank lines kept and long lines wrapped;
//! * `hr` is a rule across the text width;
//! * `table`/`tr`/`td`/`th` become a grid: columns sized to their content, cells wrapped, a
//!   hairline under every row, a header row in bold, and no row split across a page;
//! * `b`, `strong`, `i`, `em` and `code` (and the other monospace tags) change the face. There is no
//!   bold-italic face: bold wins where they nest;
//! * `a` contributes its text only — the link target is not written;
//! * the common named entities and every numeric reference are decoded; an unknown name is left as
//!   written;
//! * input is bounded: 1 MiB of HTML, 20 000 blocks, 64 KiB of `pre`, and nesting past six levels
//!   stops indenting. Past a bound the document keeps what it has.
//!
//! Ignored by design (CSS is not a layout engine, and this module is not one either): `<style>`,
//! `<script>` and `<head>` *and their contents*, every attribute (so no colours, no font sizes,
//! no alignment), images — including an email's `cid:` parts, whose `alt` text is not written
//! either — nested tables (their rows flatten into the outer grid), and any tag it does not know.
//! Text is written in WinAnsiEncoding: a character Helvetica can't draw becomes `[U+XXXX]` and the
//! first line says so, exactly as [`crate::from_text_escaped`] does for reports.

use pdfcraft_cos::{Dict, Document, Object};
use pdfcraft_fonts::{Std14, literal, win_ansi};

use crate::{CreateError, ESCAPED_NOTE, LETTER, add_page, set_title};

/// The most HTML read; a longer document is cut at a character boundary, never refused.
const MAX_HTML: usize = 1 << 20;
/// The most blocks one document may have.
const MAX_BLOCKS: usize = 20_000;
/// The most `pre` text one block may hold.
const MAX_PRE: usize = 64 * 1024;
/// The most list levels and quote levels that indent.
const MAX_NESTING: usize = 6;
/// The most markup nesting the reader tracks (deeper containers are ignored, text still flows).
const MAX_OPEN: usize = 64;

const MARGIN: f64 = 72.0;
const BODY_SIZE: f64 = 11.0;
const TABLE_SIZE: f64 = 9.5;
const PRE_SIZE: f64 = 9.0;
const PARA_SPACE: f64 = 6.0;
/// `h1`, `h2`, `h3`.
const HEADING_SIZES: [f64; 3] = [20.0, 16.0, 13.5];
const LIST_INDENT: f64 = 18.0;
const QUOTE_INDENT: f64 = 20.0;
const PRE_INDENT: f64 = 12.0;
/// Padding inside a table cell, on both sides.
const CELL_PAD: f64 = 4.0;
const COLUMN_GAP: f64 = 6.0;
/// A column is never wider than this, however long its text is.
const MAX_COLUMN: f64 = 180.0;
const MIN_COLUMN: f64 = 18.0;
/// Half a point of slack in every fit test. The metrics are exact and floats are not, so a word
/// that measures 28.5095 pt in a 28.5095 pt column must still be one line.
const SLACK: f64 = 0.5;

/// The width a line of `size` occupies: `size * 1.35`, and never less than the glyphs.
fn leading(size: f64) -> f64 {
    (size * 1.35).max(10.0)
}

/// The face a run is set in. Four of the standard fourteen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Style {
    Regular,
    Bold,
    Italic,
    Mono,
}

impl Style {
    fn face(self) -> Std14 {
        match self {
            Style::Regular => Std14::Helvetica,
            Style::Bold => Std14::HelveticaBold,
            Style::Italic => Std14::HelveticaOblique,
            Style::Mono => Std14::Courier,
        }
    }

    /// The key this face has in a page's `/Font` dictionary.
    fn key(self) -> &'static str {
        match self {
            Style::Regular => "F1",
            Style::Bold => "F2",
            Style::Italic => "F3",
            Style::Mono => "F4",
        }
    }

    /// A new style combined with `self`, in the order the tags were opened.
    fn within(self, outer: Style) -> Style {
        match (outer, self) {
            // No bold-italic face exists among the standard fourteen: bold wins.
            (Style::Bold, _) | (_, Style::Bold) => Style::Bold,
            (Style::Mono, _) | (_, Style::Mono) => Style::Mono,
            (Style::Italic, Style::Italic) => Style::Italic,
            _ => Style::Regular,
        }
    }
}

/// A piece of text in one style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Run {
    pub text: String,
    pub style: Style,
}

impl Run {
    /// A run in one style.
    pub(crate) fn new(text: impl Into<String>, style: Style) -> Self {
        Run { text: text.into(), style }
    }
}

/// One cell of a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cell {
    pub runs: Vec<Run>,
    /// A `th` cell: set bold, and its row gets a stronger rule.
    pub header: bool,
}

/// One block of the document, in the order it appears in the HTML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Block {
    /// `h1`–`h6`; a level above 3 is set at body size.
    Heading {
        level: u8,
        runs: Vec<Run>,
    },
    Para {
        runs: Vec<Run>,
    },
    /// A list item with the marker the list gave it (`•`, `1.`, …).
    Item {
        marker: String,
        depth: usize,
        runs: Vec<Run>,
    },
    Quote {
        depth: usize,
        runs: Vec<Run>,
    },
    Pre {
        text: String,
    },
    Rule,
    Table {
        rows: Vec<Vec<Cell>>,
    },
}

// ── the reader ──────────────────────────────────────────────────────────────────────────────

/// The blocks an HTML body holds, in document order.
pub(crate) fn blocks(html: &str) -> Vec<Block> {
    let mut reader = Reader::default();
    reader.read(truncate(html));
    reader.finish()
}

/// [`blocks`] as a document: US Letter, laid out with the standard-14 metrics.
pub(crate) fn document(title: &str, blocks: &[Block]) -> Result<Document, CreateError> {
    let mut escaped = 0usize;
    let escaped_blocks: Vec<Block> = blocks.iter().map(|b| escape_block(b, &mut escaped)).collect();
    let mut all = Vec::new();
    if escaped > 0 {
        all.push(Block::Para { runs: vec![Run::new(ESCAPED_NOTE, Style::Italic)] });
    }
    all.extend(escaped_blocks);
    let mut layout = Layout::new(LETTER);
    for block in &all {
        layout.block(block);
    }
    let mut doc = Document::new_empty();
    layout.render(&mut doc)?;
    set_title(&mut doc, title);
    Ok(doc)
}

/// A new document from an HTML body. See the module docs for the subset that is laid out.
pub fn from_html(title: &str, html: &str) -> Result<Document, CreateError> {
    document(title, &blocks(html))
}

/// A new document from an HTML file: its bytes decoded like every other text source (a byte order
/// mark picks the encoding, then UTF-8, then the Windows code page they most likely are).
pub fn from_html_bytes(name: &str, bytes: &[u8]) -> Result<Document, CreateError> {
    let title = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    from_html(title, &crate::decode_text(bytes))
}

fn truncate(html: &str) -> &str {
    if html.len() <= MAX_HTML {
        return html;
    }
    let mut end = MAX_HTML;
    while end > 0 && !html.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    html.get(..end).unwrap_or("")
}

/// Whether a `.html` file is HTML: its name, or its first markup.
pub fn is_likely(name: &str, bytes: &[u8]) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".html") || lower.ends_with(".htm") {
        return true;
    }
    let head = bytes.get(..bytes.len().min(1024)).unwrap_or(bytes);
    let text = String::from_utf8_lossy(head).trim_start().to_ascii_lowercase();
    text.starts_with("<!doctype html") || text.starts_with("<html")
}

#[derive(Default)]
struct Reader {
    blocks: Vec<Block>,
    runs: Vec<Run>,
    /// The styles opened so far; the last one is what text is written in.
    styles: Vec<Style>,
    /// The heading open now (`<h1>` … `<h6>`), until its text is flushed.
    heading: Option<u8>,
    /// `(ordered, next number)` per open list.
    lists: Vec<(bool, usize)>,
    /// Lists past [`MAX_OPEN`]: counted, so the closing tags still balance.
    skipped_lists: usize,
    quote: usize,
    /// The text of the open `<pre>`, kept as written.
    pre: Option<String>,
    /// How many `<script>`, `<style>` or `<head>` elements are open (their text is dropped).
    skip: usize,
    /// The table being read, its open row and its open cell.
    table: Option<Vec<Vec<Cell>>>,
    row: Option<Vec<Cell>>,
    cell: Option<Cell>,
    /// The marker and depth of the item open now (`<li>`).
    marker: Option<String>,
    item_depth: usize,
    /// Whether the last character seen was whitespace.
    space: bool,
}

impl Reader {
    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or(Style::Regular)
    }

    fn push_style(&mut self, style: Style) {
        if self.styles.len() >= MAX_OPEN {
            self.styles.push(self.style());
            return;
        }
        let combined = style.within(self.style());
        self.styles.push(combined);
    }

    fn pop_style(&mut self) {
        self.styles.pop();
    }

    fn read(&mut self, html: &str) {
        let mut i = 0usize;
        let mut text_start = 0usize;
        while i < html.len() {
            if html.as_bytes().get(i) != Some(&b'<') {
                i = i.saturating_add(1);
                continue;
            }
            if let Some(before) = html.get(text_start..i) {
                self.text(before);
            }
            if html.get(i..).is_some_and(|rest| rest.starts_with("<!--")) {
                // A comment: dropped whole, terminated or not.
                i = match html.get(i.saturating_add(4)..).and_then(|rest| rest.find("-->")) {
                    Some(at) => i.saturating_add(4).saturating_add(at).saturating_add(3),
                    None => html.len(),
                };
                text_start = i;
                continue;
            }
            let Some(end) = tag_end(html, i) else {
                // No `>`: the rest is text, not markup.
                if let Some(rest) = html.get(i..) {
                    self.text(rest);
                }
                break;
            };
            if let Some(inner) = html.get(i.saturating_add(1)..end) {
                self.tag(inner);
            }
            i = end.saturating_add(1);
            text_start = i;
        }
        if let Some(rest) = html.get(text_start..) {
            self.text(rest);
        }
    }

    /// The index of the `>` that ends the tag starting at `start`, skipping quoted attributes.
    fn finish(&mut self) -> Vec<Block> {
        self.flush_cell();
        self.flush_row();
        self.flush_current();
        if let Some(text) = self.pre.take() {
            self.push(Block::Pre { text });
        }
        std::mem::take(&mut self.blocks)
    }

    fn tag(&mut self, inner: &str) {
        let inner = inner.trim();
        let closing = inner.starts_with('/');
        let body = if closing { inner.get(1..).unwrap_or("").trim_start() } else { inner };
        let name: String = body.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
        let name = name.to_ascii_lowercase();
        if name.is_empty() {
            return;
        }
        // A stray `>` inside a value has already been handled, so `/` at the end means self-closing.
        let void = body.trim_end().ends_with('/') || is_void(&name);
        self.element(&name, closing, void);
    }

    fn element(&mut self, name: &str, closing: bool, _void: bool) {
        match (name, closing) {
            // Their contents are not text: dropped whole (and the input stays bounded).
            ("script" | "style" | "head", false) => {
                self.flush_current();
                self.skip = self.skip.saturating_add(1);
            }
            ("script" | "style" | "head", true) => self.skip = self.skip.saturating_sub(1),
            ("br", false) => self.break_line(),
            ("hr", false) => {
                self.flush_current();
                self.push(Block::Rule);
            }
            ("h1", false) => self.start_heading(1),
            ("h2", false) => self.start_heading(2),
            ("h3", false) => self.start_heading(3),
            ("h4", false) => self.start_heading(4),
            ("h5", false) => self.start_heading(5),
            ("h6", false) => self.start_heading(6),
            ("h1" | "h2" | "h3" | "h4" | "h5" | "h6", true) => self.flush_current(),
            ("ul", false) => self.start_list(false),
            ("ol", false) => self.start_list(true),
            ("ul" | "ol", true) => self.end_list(),
            ("li", false) => self.start_item(),
            ("li", true) => {
                self.flush_current();
                self.marker = None;
            }
            ("blockquote", false) => {
                self.flush_current();
                self.quote = self.quote.saturating_add(1).min(MAX_OPEN);
            }
            ("blockquote", true) => {
                self.flush_current();
                self.quote = self.quote.saturating_sub(1);
            }
            ("pre", false) => {
                self.flush_current();
                if self.pre.is_none() {
                    self.pre = Some(String::new());
                }
            }
            ("pre", true) => {
                if let Some(text) = self.pre.take() {
                    self.push(Block::Pre { text: text.trim_matches(['\n', '\r']).to_string() });
                }
            }
            ("table", false) => {
                self.flush_current();
                if self.table.is_none() {
                    self.table = Some(Vec::new());
                }
            }
            ("table", true) => {
                self.flush_row();
                if let Some(rows) = self.table.take()
                    && !rows.is_empty()
                {
                    self.push(Block::Table { rows });
                }
            }
            ("tr", false) => {
                self.flush_row();
                self.row = Some(Vec::new());
            }
            ("tr", true) => self.flush_row(),
            ("td" | "th", false) => {
                self.flush_current();
                self.flush_cell();
                self.cell = Some(Cell { runs: Vec::new(), header: name == "th" });
            }
            ("td" | "th", true) => {
                self.flush_current();
                self.flush_cell();
            }
            ("b" | "strong", false) => self.push_style(Style::Bold),
            ("b" | "strong", true) => self.pop_style(),
            ("i" | "em", false) => self.push_style(Style::Italic),
            ("i" | "em", true) => self.pop_style(),
            ("code" | "kbd" | "samp" | "tt" | "var", false) => self.push_style(Style::Mono),
            ("code" | "kbd" | "samp" | "tt" | "var", true) => self.pop_style(),
            (_, false) if is_block(name) => self.flush_current(),
            (_, true) if is_block(name) => self.flush_current(),
            // Everything else (a, span, img, font, unknown tags) changes nothing: text flows.
            _ => {}
        }
    }

    fn start_heading(&mut self, level: u8) {
        self.flush_current();
        self.heading = Some(level);
    }

    fn start_list(&mut self, ordered: bool) {
        self.flush_current();
        if self.lists.len() >= MAX_OPEN {
            self.skipped_lists = self.skipped_lists.saturating_add(1);
        } else {
            self.lists.push((ordered, 1));
        }
    }

    fn end_list(&mut self) {
        self.flush_current();
        if self.skipped_lists > 0 {
            self.skipped_lists = self.skipped_lists.saturating_sub(1);
        } else {
            self.lists.pop();
        }
    }

    fn start_item(&mut self) {
        self.flush_current();
        let ordered = self.lists.last_mut().map(|(ordered, next)| {
            if *ordered {
                let number = *next;
                *next = next.saturating_add(1);
                Some(number)
            } else {
                None
            }
        });
        self.marker = Some(match ordered {
            Some(Some(number)) => format!("{number}."),
            _ => "•".to_string(),
        });
        self.item_depth = self.lists.len().saturating_sub(1);
    }

    fn break_line(&mut self) {
        if self.skip > 0 {
            return;
        }
        if self.pre.is_some() {
            if let Some(pre) = self.pre.as_mut() {
                pre.push('\n');
            }
            return;
        }
        if self.runs.last().is_some_and(|r| !r.text.ends_with('\n')) {
            self.runs.push(Run::new("\n", self.style()));
        }
    }

    /// Text that is not markup: entities decoded, whitespace collapsed (kept as written in `pre`).
    fn text(&mut self, text: &str) {
        if self.skip > 0 || text.is_empty() {
            return;
        }
        if self.pre.is_some() {
            let decoded = if text.contains('&') { unescape(text) } else { text.to_string() };
            if let Some(pre) = self.pre.as_mut()
                && pre.len() < MAX_PRE
            {
                pre.push_str(&decoded);
            }
            return;
        }
        let style = self.style();
        for c in unescape(text).chars() {
            if c.is_whitespace() {
                self.space = true;
                continue;
            }
            if self.space {
                self.push_char(' ', style);
                self.space = false;
            }
            self.push_char(c, style);
        }
    }

    fn push_char(&mut self, c: char, style: Style) {
        match self.runs.last_mut() {
            Some(run) if run.style == style => run.text.push(c),
            _ => self.runs.push(Run::new(c.to_string(), style)),
        }
    }

    /// The text collected so far as the block it belongs to; nothing when it holds no text.
    fn flush_current(&mut self) {
        let runs = self.take_runs();
        if runs.is_empty() {
            // An empty heading or list item leaves nothing behind, but must not colour the next
            // block either: `<h1></h1><p>text</p>` is a paragraph, not a heading.
            self.heading = None;
            self.marker = None;
            return;
        }
        if let Some(cell) = self.cell.as_mut() {
            cell.runs.extend(runs);
            return;
        }
        let block = if let Some(level) = self.heading.take() {
            Block::Heading { level, runs }
        } else if let Some(marker) = self.marker.take() {
            Block::Item { marker, depth: self.item_depth, runs }
        } else if self.quote > 0 {
            Block::Quote { depth: self.quote.min(MAX_NESTING), runs }
        } else {
            Block::Para { runs }
        };
        self.push(block);
    }

    fn take_runs(&mut self) -> Vec<Run> {
        self.space = false;
        let mut runs = std::mem::take(&mut self.runs);
        if let Some(first) = runs.first_mut() {
            first.text = first.text.trim_start().to_string();
        }
        if let Some(last) = runs.last_mut() {
            last.text = last.text.trim_end().to_string();
        }
        runs.retain(|r| !r.text.is_empty());
        runs
    }

    fn flush_cell(&mut self) {
        let Some(cell) = self.cell.take() else { return };
        let mut cell = cell;
        if cell.runs.iter().all(|r| r.text.trim().is_empty()) {
            cell.runs.clear();
        }
        if let Some(row) = self.row.as_mut() {
            row.push(cell);
        }
    }

    fn flush_row(&mut self) {
        self.flush_cell();
        let Some(row) = self.row.take() else { return };
        if row.is_empty() {
            return;
        }
        if let Some(table) = self.table.as_mut() {
            table.push(row);
        }
    }

    fn push(&mut self, block: Block) {
        if self.blocks.len() < MAX_BLOCKS {
            self.blocks.push(block);
        }
    }
}

/// The index of the `>` that closes the tag starting at `start`, skipping quoted attribute values.
fn tag_end(html: &str, start: usize) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (offset, c) in html.get(start.saturating_add(1)..)?.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (None, '"' | '\'') => quote = Some(c),
            (None, '>') => return Some(start.saturating_add(1).saturating_add(offset)),
            _ => {}
        }
    }
    None
}

/// The elements that hold no text and never have a closing tag.
fn is_void(name: &str) -> bool {
    matches!(name, "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input" | "link" | "meta" | "param" | "source" | "track" | "wbr")
}

/// The elements that are a paragraph of their own (their text is flushed on either tag).
fn is_block(name: &str) -> bool {
    matches!(
        name,
        "p" | "div"
            | "section"
            | "article"
            | "aside"
            | "header"
            | "footer"
            | "main"
            | "nav"
            | "figure"
            | "figcaption"
            | "address"
            | "center"
            | "dl"
            | "dt"
            | "dd"
            | "form"
            | "fieldset"
            | "legend"
            | "caption"
            | "body"
            | "html"
            | "tbody"
            | "thead"
            | "tfoot"
    )
}

/// Decode the entities in `text`; an unknown name is left as written.
fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        if let Some(before) = rest.get(..at) {
            out.push_str(before);
        }
        let tail = rest.get(at..).unwrap_or("");
        match entity(tail) {
            Some((c, used)) => {
                out.push(c);
                rest = tail.get(used..).unwrap_or("");
            }
            None => {
                out.push('&');
                rest = tail.get(1..).unwrap_or("");
            }
        }
    }
    out.push_str(rest);
    out
}

/// One entity at the start of `s`: the character and how many bytes it takes.
fn entity(s: &str) -> Option<(char, usize)> {
    let body = s.strip_prefix('&')?;
    // An entity name is short; the cap keeps the scan bounded.
    let window = body.get(..body.len().min(16))?;
    let end = window.find(';')?;
    let name = body.get(..end)?;
    let used = end.saturating_add(2);
    if let Some(number) = name.strip_prefix('#') {
        let code = match number.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => number.parse::<u32>().ok()?,
        };
        let c = char::from_u32(code)?;
        // A control character would be drawn as a box; keep it out of the text.
        if c.is_control() && !matches!(c, '\n' | '\t') {
            return None;
        }
        return Some((c, used));
    }
    let c = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "hellip" => '…',
        "mdash" => '—',
        "ndash" => '–',
        "bull" => '•',
        "middot" => '·',
        "laquo" => '«',
        "raquo" => '»',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "deg" => '°',
        "plusmn" => '±',
        "times" => '×',
        "divide" => '÷',
        "frac12" => '½',
        "frac14" => '¼',
        "frac34" => '¾',
        "sect" => '§',
        "para" => '¶',
        "dagger" => '†',
        "Dagger" => '‡',
        "euro" => '€',
        "pound" => '£',
        "yen" => '¥',
        "cent" => '¢',
        "brvbar" => '¦',
        "uml" => '¨',
        "szlig" => 'ß',
        "agrave" => 'à',
        "aacute" => 'á',
        "auml" => 'ä',
        "ouml" => 'ö',
        "uuml" => 'ü',
        "eacute" => 'é',
        "egrave" => 'è',
        "ccedil" => 'ç',
        "ntilde" => 'ñ',
        "oslash" => 'ø',
        "aring" => 'å',
        "ne" => '≠',
        "le" => '≤',
        "ge" => '≥',
        _ => return None,
    };
    Some((c, used))
}

// ── escaping ────────────────────────────────────────────────────────────────────────────────

/// Every character the standard fonts can't draw becomes `[U+XXXX]`, as in [`crate::from_text_escaped`].
fn escape(text: &str, escaped: &mut usize) -> String {
    if text.is_ascii() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\n' | '\t') || pdfcraft_fonts::first_undrawable(c.encode_utf8(&mut [0; 4])).is_none() {
            out.push(c);
        } else {
            *escaped = escaped.saturating_add(1);
            out.push_str(&format!("[U+{:04X}]", u32::from(c)));
        }
    }
    out
}

fn escape_runs(runs: &[Run], escaped: &mut usize) -> Vec<Run> {
    runs.iter().map(|r| Run { text: escape(&r.text, escaped), style: r.style }).collect()
}

fn escape_block(block: &Block, escaped: &mut usize) -> Block {
    match block {
        Block::Heading { level, runs } => Block::Heading { level: *level, runs: escape_runs(runs, escaped) },
        Block::Para { runs } => Block::Para { runs: escape_runs(runs, escaped) },
        Block::Item { marker, depth, runs } => Block::Item { marker: marker.clone(), depth: *depth, runs: escape_runs(runs, escaped) },
        Block::Quote { depth, runs } => Block::Quote { depth: *depth, runs: escape_runs(runs, escaped) },
        Block::Pre { text } => Block::Pre { text: escape(text, escaped) },
        Block::Rule => Block::Rule,
        Block::Table { rows } => Block::Table {
            rows: rows
                .iter()
                .map(|row| row.iter().map(|cell| Cell { runs: escape_runs(&cell.runs, escaped), header: cell.header }).collect())
                .collect(),
        },
    }
}

// ── layout ──────────────────────────────────────────────────────────────────────────────────

/// One thing drawn on a page.
#[derive(Debug, Clone, PartialEq)]
enum Item {
    /// A line of text; `y` is the baseline.
    Text { x: f64, y: f64, size: f64, runs: Vec<(Style, String)> },
    /// A rule from `(x1, y1)` to `(x2, y2)`, `grey` from 0 (black) to 1 (white).
    Rule { x1: f64, y1: f64, x2: f64, y2: f64, width: f64, grey: f64 },
}

struct Layout {
    pages: Vec<Vec<Item>>,
    /// The baseline of the next line on the current page.
    y: f64,
    /// The baseline of the first line on a page.
    top: f64,
    width: f64,
    height: f64,
    x0: f64,
    x1: f64,
}

impl Layout {
    fn new(page: (f64, f64)) -> Self {
        let (w, h) = page;
        let width = if w.is_finite() && w >= 2.0 * MARGIN + 36.0 { w } else { LETTER.0 };
        let height = if h.is_finite() && h >= 2.0 * MARGIN + 36.0 { h } else { LETTER.1 };
        let top = height - MARGIN - BODY_SIZE;
        Layout { pages: vec![Vec::new()], y: top, top, width, height, x0: MARGIN, x1: width - MARGIN }
    }

    fn avail(&self) -> f64 {
        (self.x1 - self.x0).max(36.0)
    }

    fn new_page(&mut self) {
        self.pages.push(Vec::new());
        self.y = self.top;
    }

    /// Room for `height` points below the cursor; a page break when there is none.
    fn ensure(&mut self, height: f64) {
        if self.y - height < MARGIN {
            self.new_page();
        }
    }

    /// Move down by `before` points of space, breaking the page when it does not fit.
    fn space(&mut self, before: f64) {
        if before <= 0.0 {
            return;
        }
        if self.y - before < MARGIN {
            self.new_page();
        } else {
            self.y -= before;
        }
    }

    fn push_text(&mut self, x: f64, y: f64, size: f64, runs: &[(Style, String)]) {
        if runs.is_empty() {
            return;
        }
        if let Some(page) = self.pages.last_mut() {
            page.push(Item::Text { x, y, size, runs: runs.to_vec() });
        }
    }

    fn push_rule(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, width: f64, grey: f64) {
        if let Some(page) = self.pages.last_mut() {
            page.push(Item::Rule { x1, y1, x2, y2, width, grey });
        }
    }

    /// The height a run of `lines` lines of `size` needs on a page.
    fn lines_height(lines: usize, size: f64) -> f64 {
        let lead = leading(size);
        (lines.saturating_sub(1) as f64) * lead + 0.75 * size
    }

    fn draw_lines(&mut self, x: f64, size: f64, lines: &[Vec<(Style, String)>]) {
        if lines.is_empty() {
            return;
        }
        let lead = leading(size);
        self.ensure(Self::lines_height(lines.len(), size));
        for (i, line) in lines.iter().enumerate() {
            let y = self.y - (i as f64) * lead;
            self.push_text(x, y, size, line);
        }
        self.y -= (lines.len() as f64) * lead;
    }

    fn block(&mut self, block: &Block) {
        match block {
            Block::Heading { level, runs } => {
                let size = HEADING_SIZES.get(usize::from(*level).saturating_sub(1)).copied().unwrap_or(BODY_SIZE);
                self.space(size * 0.7);
                let lines = wrap_runs(runs, size, self.avail(), self.avail(), true);
                self.draw_lines(self.x0, size, &lines);
                self.space(size * 0.35);
            }
            Block::Para { runs } => {
                let lines = wrap_runs(runs, BODY_SIZE, self.avail(), self.avail(), false);
                self.draw_lines(self.x0, BODY_SIZE, &lines);
                self.space(PARA_SPACE);
            }
            Block::Item { marker, depth, runs } => {
                let indent = LIST_INDENT * (*depth).min(MAX_NESTING) as f64;
                let x = self.x0 + indent;
                let marker_width = Stats::width(marker, Style::Regular, BODY_SIZE) + 6.0;
                let rest = (self.x1 - (x + marker_width)).max(36.0);
                let lines = wrap_runs(runs, BODY_SIZE, rest, rest, false);
                let lead = leading(BODY_SIZE);
                self.ensure(Self::lines_height(lines.len().max(1), BODY_SIZE));
                let baseline = self.y;
                self.push_text(x, baseline, BODY_SIZE, &[(Style::Regular, marker.clone())]);
                for (i, line) in lines.iter().enumerate() {
                    self.push_text(x + marker_width, baseline - (i as f64) * lead, BODY_SIZE, line);
                }
                self.y = baseline - (lines.len() as f64) * lead;
                self.space(2.0);
            }
            Block::Quote { depth, runs } => {
                let indent = QUOTE_INDENT * (*depth).min(MAX_NESTING) as f64;
                let x = self.x0 + indent;
                let room = (self.x1 - x).max(36.0);
                let lines = wrap_runs(runs, BODY_SIZE, room, room, false);
                let lead = leading(BODY_SIZE);
                self.ensure(Self::lines_height(lines.len().max(1), BODY_SIZE));
                let baseline = self.y;
                let top = baseline + BODY_SIZE * 0.8;
                let bottom = baseline - (lines.len().saturating_sub(1) as f64) * lead - BODY_SIZE * 0.25;
                self.push_rule(x - 8.0, top, x - 8.0, bottom, 1.0, 0.7);
                for (i, line) in lines.iter().enumerate() {
                    self.push_text(x, baseline - (i as f64) * lead, BODY_SIZE, line);
                }
                self.y = baseline - (lines.len() as f64) * lead;
                self.space(PARA_SPACE);
            }
            Block::Pre { text } => {
                let x = self.x0 + PRE_INDENT;
                let room = (self.x1 - x).max(36.0);
                let mut lines: Vec<Vec<(Style, String)>> = Vec::new();
                for raw in text.replace('\t', "    ").split('\n') {
                    if raw.trim().is_empty() {
                        lines.push(Vec::new());
                    } else {
                        lines.extend(wrap_runs(&[Run::new(raw, Style::Mono)], PRE_SIZE, room, room, false));
                    }
                }
                while lines.first().is_some_and(Vec::is_empty) {
                    lines.remove(0);
                }
                while lines.last().is_some_and(Vec::is_empty) {
                    lines.pop();
                }
                if lines.is_empty() {
                    lines.push(Vec::new());
                }
                self.space(PARA_SPACE);
                let lead = leading(PRE_SIZE);
                for line in &lines {
                    self.ensure(lead);
                    self.push_text(x, self.y, PRE_SIZE, line);
                    self.y -= lead;
                }
                self.space(PARA_SPACE);
            }
            Block::Rule => {
                self.space(6.0);
                let y = self.y - 2.0;
                self.push_rule(self.x0, y, self.x1, y, 0.5, 0.5);
                self.y = y - 6.0;
            }
            Block::Table { rows } => self.table(rows),
        }
    }

    fn table(&mut self, rows: &[Vec<Cell>]) {
        let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        // A column is as wide as its longest line, capped, and the columns are then scaled down
        // together to fit the text width. A header cell is measured in the bold face it is drawn
        // in, or the column would be a hair too narrow and the cell would break mid-word.
        let mut content: Vec<f64> = vec![MIN_COLUMN; columns];
        for row in rows {
            let header = row.iter().any(|cell| cell.header);
            for (i, cell) in row.iter().enumerate() {
                let width = Stats::runs_width(&cell_runs(cell, header), TABLE_SIZE).min(MAX_COLUMN);
                if let Some(slot) = content.get_mut(i) {
                    *slot = slot.max(width);
                }
            }
        }
        let gaps = COLUMN_GAP * (columns.saturating_sub(1) as f64);
        let padding = 2.0 * CELL_PAD * columns as f64;
        let natural: f64 = content.iter().sum::<f64>() + padding + gaps;
        if natural > self.avail() {
            let room = (self.avail() - padding - gaps).max(MIN_COLUMN * columns as f64);
            let total: f64 = content.iter().sum::<f64>().max(1.0);
            let scale = room / total;
            content = content.iter().map(|w| (w * scale).max(MIN_COLUMN)).collect();
        }
        let used: f64 = content.iter().sum::<f64>() + padding + gaps;
        let lead = leading(TABLE_SIZE);
        for row in rows {
            let header = row.iter().any(|c| c.header);
            let mut cells: Vec<Vec<Vec<(Style, String)>>> = Vec::new();
            let mut height = 1usize;
            for (i, cell) in row.iter().enumerate() {
                let width = content.get(i).copied().unwrap_or(MIN_COLUMN);
                let runs = cell_runs(cell, header);
                let lines = wrap_runs(&runs, TABLE_SIZE, width, width, false);
                height = height.max(lines.len().max(1));
                cells.push(lines);
            }
            let row_height = height as f64 * lead + 2.0 * CELL_PAD;
            self.ensure(row_height);
            self.space(CELL_PAD);
            let baseline = self.y;
            let mut x = self.x0;
            for (i, lines) in cells.iter().enumerate() {
                for (j, line) in lines.iter().enumerate() {
                    self.push_text(x + CELL_PAD, baseline - (j as f64) * lead, TABLE_SIZE, line);
                }
                x += content.get(i).copied().unwrap_or(MIN_COLUMN) + 2.0 * CELL_PAD + COLUMN_GAP;
            }
            self.y = baseline - (height.saturating_sub(1) as f64) * lead - TABLE_SIZE * 0.3;
            let rule_y = self.y - CELL_PAD * 0.5;
            let right = (self.x0 + used).min(self.x1);
            self.push_rule(self.x0, rule_y, right, rule_y, 0.5, if header { 0.35 } else { 0.75 });
            self.y = rule_y - CELL_PAD;
        }
        self.space(PARA_SPACE);
    }

    fn render(&self, doc: &mut Document) -> Result<(), CreateError> {
        // One `/Font` dictionary shared by every page: the four faces the blocks ask for.
        let mut fonts = Dict::new();
        for style in [Style::Regular, Style::Bold, Style::Italic, Style::Mono] {
            let mut font = Dict::new();
            font.set(b"Type".to_vec(), Object::name("Font"));
            font.set(b"Subtype".to_vec(), Object::name("Type1"));
            font.set(b"BaseFont".to_vec(), Object::name(style.face().base_font()));
            font.set(b"Encoding".to_vec(), Object::name("WinAnsiEncoding"));
            let reference = doc.add(font);
            fonts.set(style.key().as_bytes().to_vec(), Object::Ref(reference));
        }
        for page in &self.pages {
            let mut content = Vec::new();
            for item in page {
                match item {
                    Item::Text { x, y, size, runs } => {
                        content.extend_from_slice(&format!("BT 1 0 0 1 {x:.2} {y:.2} Tm\n").into_bytes());
                        let mut current: Option<Style> = None;
                        for (style, text) in runs {
                            if current != Some(*style) {
                                content.extend_from_slice(&format!("/{} {size:.2} Tf\n", style.key()).into_bytes());
                                current = Some(*style);
                            }
                            content.extend(literal(&win_ansi(text)));
                            content.extend_from_slice(b" Tj\n");
                        }
                        content.extend_from_slice(b"ET\n");
                    }
                    Item::Rule { x1, y1, x2, y2, width, grey } => {
                        let rule = format!("{grey:.2} G {width:.2} w {x1:.2} {y1:.2} m {x2:.2} {y2:.2} l S\n");
                        content.extend_from_slice(rule.as_bytes());
                    }
                }
            }
            let mut resources = Dict::new();
            resources.set(b"Font".to_vec(), Object::Dict(fonts.clone()));
            add_page(doc, self.width, self.height, resources, Some(content))?;
        }
        Ok(())
    }
}

/// The runs of one cell as they are drawn: a header cell is bold, unless its own runs already are.
fn cell_runs(cell: &Cell, header: bool) -> Vec<Run> {
    if header && !cell.runs.iter().any(|r| r.style == Style::Bold) {
        cell.runs.iter().map(|r| Run { text: r.text.clone(), style: Style::Bold }).collect()
    } else {
        cell.runs.clone()
    }
}

// ── wrapping ────────────────────────────────────────────────────────────────────────────────

/// The metrics every line is measured with: the standard fourteen, at the run's own face.
struct Stats;

impl Stats {
    fn width(text: &str, style: Style, size: f64) -> f64 {
        style.face().text_width(text, size)
    }

    fn runs_width(runs: &[Run], size: f64) -> f64 {
        runs.iter().map(|r| Self::width(&r.text, r.style, size)).sum()
    }
}

/// One word, or a forced line break.
struct Piece {
    text: String,
    style: Style,
    br: bool,
}

/// The words of `runs` in order, with `\n` as a forced break between them.
fn pieces(runs: &[Run]) -> Vec<Piece> {
    let mut out = Vec::new();
    for run in runs {
        let mut word = String::new();
        for c in run.text.chars() {
            match c {
                '\n' => {
                    if !word.is_empty() {
                        out.push(Piece { text: std::mem::take(&mut word), style: run.style, br: false });
                    }
                    out.push(Piece { text: String::new(), style: run.style, br: true });
                }
                ' ' => {
                    if !word.is_empty() {
                        out.push(Piece { text: std::mem::take(&mut word), style: run.style, br: false });
                    }
                }
                _ => word.push(c),
            }
        }
        if !word.is_empty() {
            out.push(Piece { text: word, style: run.style, br: false });
        }
    }
    out
}

/// Greedy line breaking over the runs of one block: the first line may be narrower than the rest
/// (a list marker hangs in the margin). A word longer than the line is broken by character.
fn wrap_runs(runs: &[Run], size: f64, first: f64, rest: f64, bold: bool) -> Vec<Vec<(Style, String)>> {
    let mut lines: Vec<Vec<(Style, String)>> = Vec::new();
    let mut line: Vec<(Style, String)> = Vec::new();
    let mut used = 0.0f64;
    let mut room = (first + SLACK).max(1.0);
    let style_of = |style: Style| if bold { Style::Bold } else { style };
    let push_word = |line: &mut Vec<(Style, String)>, used: &mut f64, text: &str, style: Style| {
        if let Some(last) = line.last_mut()
            && last.0 == style
        {
            last.1.push_str(text);
        } else {
            line.push((style, text.to_string()));
        }
        *used += Stats::width(text, style, size);
    };
    for piece in pieces(runs) {
        if piece.br {
            lines.push(std::mem::take(&mut line));
            used = 0.0;
            room = (rest + SLACK).max(1.0);
            continue;
        }
        let style = style_of(piece.style);
        let width = Stats::width(&piece.text, style, size);
        let space = Stats::width(" ", style, size);
        if line.is_empty() {
            if width <= room {
                push_word(&mut line, &mut used, &piece.text, style);
            } else {
                break_word(&mut lines, &mut line, &mut used, &piece.text, style, size, room, rest);
            }
            continue;
        }
        if used + space + width <= room {
            push_word(&mut line, &mut used, " ", style);
            push_word(&mut line, &mut used, &piece.text, style);
            continue;
        }
        lines.push(std::mem::take(&mut line));
        used = 0.0;
        room = (rest + SLACK).max(1.0);
        if width <= room {
            push_word(&mut line, &mut used, &piece.text, style);
        } else {
            break_word(&mut lines, &mut line, &mut used, &piece.text, style, size, room, rest);
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// A word wider than its line, split by character at the metrics.
#[allow(clippy::too_many_arguments)]
fn break_word(
    lines: &mut Vec<Vec<(Style, String)>>,
    line: &mut Vec<(Style, String)>,
    used: &mut f64,
    word: &str,
    style: Style,
    size: f64,
    room: f64,
    rest: f64,
) {
    let mut width = 0.0;
    let mut part = String::new();
    let mut room = room;
    for c in word.chars() {
        let c_width = Stats::width(c.encode_utf8(&mut [0; 4]), style, size);
        if !part.is_empty() && width + c_width > room {
            line.push((style, std::mem::take(&mut part)));
            *used = width;
            lines.push(std::mem::take(line));
            room = (rest + SLACK).max(1.0);
            width = 0.0;
        }
        part.push(c);
        width += c_width;
    }
    line.push((style, part));
    *used = width;
}
