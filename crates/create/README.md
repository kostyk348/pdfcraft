# pdfcraft-create

Layer L4. Create a PDF (Acrobat's Create a PDF tool), execution plan M10.2:

- `blank(width, height, pages)`: an empty document;
- `from_images(&[(name, bytes)])`: one page per image, sized from the image's resolution
  (PNG `pHYs`, JPEG JFIF density; 72 dpi when absent). JPEG data is embedded as is
  (`/DCTDecode`, grey, RGB or Adobe-inverted CMYK); PNG is decoded and stored with Flate,
  with transparency as a soft mask. An ICC profile in the file (JPEG `APP2 ICC_PROFILE`, PNG
  `iCCP`) that matches the image's colour space is kept as an `/ICCBased` colour space with
  the device space as `/Alternate`; images with the same profile share one profile object.
  JPEG metadata scanning skips marker fill bytes and reads application segments after the
  frame header, stopping at the first scan or end marker. Broken or mismatched ICC profiles
  still fall back to the device colour space;
- `from_images_with_resolution(images, ImageResolution::Dpi(dpi))`: override the page
  resolution (1–1200 dpi), without resampling or recompressing image pixels. Use 72 dpi
  for one point per pixel, or `ImageResolution::Embedded` for the same behavior as
  `from_images`. The largest page side is still capped at 14,400 points;
- `from_text(text, …)`: plain text set in Helvetica, wrapped and paginated. Text Helvetica
  can't draw is refused, naming the line and the character, never set as `?`.
- `from_text_escaped(text, …)`: the same for reports PdfCraft generates from documents
  (comment summaries, compare reports): such characters are written as `[U+XXXX]` and the first
  line says so (`ESCAPED_NOTE`).
- `from_html(title, html)` and `from_html_bytes(name, bytes)`: `create.from-html`. A **bounded**
  subset of HTML is laid out as a document, with the standard-14 metrics in `pdfcraft-fonts`, on
  US Letter pages: `h1`–`h3` (Helvetica-Bold at 20 / 16 / 13.5 pt), `h4`–`h6` at body size in bold,
  paragraphs and the other block containers, `br`, `ul`/`ol`/`li` with bullets, numbers and a
  hanging indent per level, `blockquote` (indented, with a rule down its left side), `pre`
  (Courier, whitespace and blank lines kept, long lines wrapped), `hr`, and simple tables
  (`tr`/`td`/`th`: columns sized to their content and scaled together to fit, cells wrapped, a
  hairline under every row, a header row in bold, and no row split across a page). `b`/`strong`,
  `i`/`em` and `code` change the face — there is no bold-italic: bold wins where they nest. A link
  contributes its text only. The common named entities and every numeric reference are decoded.
  Ignored by design: `style` and `script` (and `head`) **with their contents**, every attribute
  (so no colours, sizes or alignment), images — including an email's `cid:` parts, whose `alt`
  text is not written either — nested tables (their rows flatten into the outer grid), and any tag
  it does not know. Input is bounded: 1 MiB of HTML, 20 000 blocks, 64 KiB of `pre`, and nesting
  past six levels stops indenting; past a bound the document keeps what it has. Text that the
  standard fonts can't draw becomes `[U+XXXX]` with `ESCAPED_NOTE` first, as above.
- `from_email_bytes(name, bytes)`: an email (`.eml`, RFC 822 / MIME), read by `pdfcraft-eml`: the
  header lines, the body and the attachment list. A plain-text body is set as text (any language,
  as above); an **HTML** body goes through `from_html`, so its structure survives. Attachments are
  listed, not embedded.

Every function returns a `pdfcraft_cos::Document`; the caller writes it.
