use std::path::Path;

use pulldown_cmark::Alignment;
use pulldown_cmark::Event;
use pulldown_cmark::HeadingLevel;
use pulldown_cmark::Options;
use pulldown_cmark::Parser;
use pulldown_cmark::Tag;
use pulldown_cmark::TagEnd;

use crate::font::Font;
use crate::sixel;

/// ANSI escape helpers
mod ansi {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";
    pub const ITALIC: &str = "\x1b[3m";
    pub const UNDERLINE: &str = "\x1b[4m";
    pub const STRIKETHROUGH: &str = "\x1b[9m";

    /// OSC 8 hyperlink start
    pub fn link_start(url: &str) -> String {
        format!("\x1b]8;;{url}\x1b\\")
    }

    /// OSC 8 hyperlink end
    pub fn link_end() -> String {
        "\x1b]8;;\x1b\\".to_string()
    }

    /// Word-wrap a string that may contain ANSI escape sequences.
    ///
    /// Wraps at `width` visible columns, preserving escape sequences
    /// (which don't consume column space).
    pub fn wrap(
        text: &str,
        width: u16,
    ) -> String {
        let width = width as usize;
        if width == 0 {
            return text.to_string();
        }

        let mut result = String::with_capacity(text.len());
        let mut col = 0usize;
        // Track the last word boundary: (byte pos in result, visible col at that point)
        let mut last_break: Option<(usize, usize)> = None;
        let mut chars = text.chars().peekable();

        while let Some(ch) = chars.next() {
            // Copy escape sequences verbatim (zero visible width)
            if ch == '\x1b' {
                result.push(ch);
                while let Some(&_next) = chars.peek() {
                    let next = chars.next().unwrap();
                    result.push(next);
                    if next.is_ascii_alphabetic() || next == '\\' {
                        break;
                    }
                }
                continue;
            }

            if ch == '\n' {
                result.push('\n');
                col = 0;
                last_break = None;
                continue;
            }

            // Record word boundary (space position)
            if ch == ' ' {
                last_break = Some((result.len(), col));
            }

            result.push(ch);
            col += 1;

            // Check if we've exceeded the width
            if col > width {
                if let Some((break_pos, break_col)) = last_break.take() {
                    // Replace the space at the break point with a newline
                    result.replace_range(break_pos..break_pos + 1, "\n");
                    col -= break_col + 1;
                } else {
                    // No word boundary — insert a hard break before this char
                    let ch_len = ch.len_utf8();
                    let insert_pos = result.len() - ch_len;
                    result.insert(insert_pos, '\n');
                    col = 1;
                }
            }
        }

        result
    }
}

/// Heading font sizes in pixels for h1..h6
const HEADING_SIZES: [u32; 6] = [48, 40, 32, 28, 24, 20];

/// Heading colors [r, g, b] for h1..h6
const HEADING_COLORS: [[u8; 3]; 6] = [
    [255, 255, 255],
    [220, 220, 255],
    [200, 220, 255],
    [180, 210, 255],
    [170, 200, 240],
    [160, 190, 230],
];

struct RenderState {
    /// Text accumulated for the current heading (rendered as sixel)
    heading_text: String,
    /// Current heading level (0 = not in heading)
    heading_level: usize,
    /// Whether we are inside bold
    bold: bool,
    /// Whether we are inside italic
    italic: bool,
    /// Whether we are inside strikethrough
    strikethrough: bool,
    /// Current link URL (if inside a link)
    link_url: Option<String>,
    /// Whether we are inside a code block
    in_code_block: bool,
    /// List nesting with item index (None = unordered, Some(n) = ordered
    /// starting at n)
    list_stack: Vec<Option<u64>>,
    /// Whether we just started a list item (for prefix)
    item_index: Vec<usize>,
    /// Base path for resolving relative image paths
    base_path: Option<std::path::PathBuf>,
    /// Accumulator for block-level HTML (multiple Html events before
    /// End(HtmlBlock))
    html_block_buf: String,
    /// Table column alignments (set on Start(Table))
    table_alignments: Vec<Alignment>,
    /// Whether we are in the header row
    in_table_head: bool,
    /// Accumulated text for the current table cell
    table_cell_buf: String,
    /// Cells collected for the current row: (text, is_bold, is_italic, is_code)
    table_row_cells: Vec<(String, bool, bool, bool)>,
    /// All collected rows: header row (if any) + data rows
    table_header: Option<Vec<(String, bool, bool, bool)>>,
    table_rows: Vec<Vec<(String, bool, bool, bool)>>,
    /// Track inline styles within table cells
    table_cell_bold: bool,
    table_cell_italic: bool,
    table_cell_code: bool,
    /// Pending background image encodes, indexed by placeholder ID.
    pending_images: Vec<sixel::PendingImage>,
    /// Byte offset in `out` where the current paragraph started (for wrapping).
    para_start: Option<usize>,
    /// Terminal width for word wrapping.
    term_width: u16,
}

impl RenderState {
    fn new(base_path: Option<std::path::PathBuf>) -> Self {
        Self {
            heading_text: String::new(),
            heading_level: 0,
            bold: false,
            italic: false,
            strikethrough: false,
            link_url: None,
            in_code_block: false,
            list_stack: Vec::new(),
            item_index: Vec::new(),
            base_path,
            html_block_buf: String::new(),
            table_alignments: Vec::new(),
            in_table_head: false,
            table_cell_buf: String::new(),
            table_row_cells: Vec::new(),
            table_header: None,
            table_rows: Vec::new(),
            table_cell_bold: false,
            table_cell_italic: false,
            table_cell_code: false,
            pending_images: Vec::new(),
            para_start: None,
            term_width: crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80),
        }
    }

    fn in_table(&self) -> bool {
        !self.table_alignments.is_empty()
    }

    /// Emit a placeholder for a pending image and start background encoding.
    fn emit_image(
        &mut self,
        path: &std::path::Path,
        out: &mut String,
    ) {
        if let Some(pending) = sixel::encode_image_file_async(path, 800) {
            let id = self.pending_images.len();
            // Placeholder format recognized by the pager
            out.push_str(&format!("\x00IMG:{id}:{}\x00\n", pending.estimated_rows));
            self.pending_images.push(pending);
        }
    }

    fn push_style(
        &self,
        out: &mut String,
    ) {
        if self.bold {
            out.push_str(ansi::BOLD);
        }
        if self.italic {
            out.push_str(ansi::ITALIC);
        }
        if self.strikethrough {
            out.push_str(ansi::STRIKETHROUGH);
        }
    }

    fn list_indent(&self) -> String {
        "  ".repeat(self.list_stack.len().saturating_sub(1))
    }

    fn resolve_image_path(
        &self,
        src: &str,
    ) -> Option<std::path::PathBuf> {
        if src.starts_with("http://") || src.starts_with("https://") {
            return None;
        }
        let p = Path::new(src);
        if p.is_absolute() {
            Some(p.to_path_buf())
        } else {
            self.base_path.as_ref().map(|bp| bp.join(p))
        }
    }
}

// ---------------------------------------------------------------------------
// HTML parsing via quick-xml
// ---------------------------------------------------------------------------

use quick_xml::events::Event as XmlEvent;
use quick_xml::reader::Reader as XmlReader;

/// Get an attribute value from a quick-xml `BytesStart` tag.
fn xml_attr(
    tag: &quick_xml::events::BytesStart<'_>,
    name: &[u8],
) -> Option<String> {
    tag.attributes()
        .filter_map(|a| a.ok())
        .find(|a| a.key.as_ref() == name)
        .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()))
}

/// Get the lowercase tag name from a quick-xml `BytesStart`.
fn xml_tag_name(tag: &quick_xml::events::BytesStart<'_>) -> String {
    String::from_utf8_lossy(tag.name().as_ref()).to_ascii_lowercase()
}

/// Handle an inline HTML tag event, modifying state and appending to output.
///
/// pulldown-cmark emits each inline HTML tag as a separate string like
/// `<b>`, `</b>`, `<a href="...">`, `<br>`, `<img src="...">`.
fn handle_inline_html(
    tag_str: &str,
    state: &mut RenderState,
    out: &mut String,
) {
    let mut reader = XmlReader::from_str(tag_str);
    reader.config_mut().check_end_names = false;
    reader.config_mut().allow_unmatched_ends = true;

    while let Ok(event) = reader.read_event() {
        match event {
            XmlEvent::Start(ref e) | XmlEvent::Empty(ref e) => {
                handle_html_open_tag(&xml_tag_name(e), e, state, out);
            }
            XmlEvent::End(ref e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_ascii_lowercase();
                handle_html_close_tag(&name, state, out);
            }
            XmlEvent::Eof => break,
            _ => {}
        }
    }
}

/// Process an opening (or self-closing) HTML tag.
fn handle_html_open_tag(
    name: &str,
    tag: &quick_xml::events::BytesStart<'_>,
    state: &mut RenderState,
    out: &mut String,
) {
    match name {
        "b" | "strong" => {
            state.bold = true;
            if state.heading_level == 0 {
                out.push_str(ansi::BOLD);
            }
        }
        "i" | "em" => {
            state.italic = true;
            if state.heading_level == 0 {
                out.push_str(ansi::ITALIC);
            }
        }
        "del" | "s" | "strike" => {
            state.strikethrough = true;
            if state.heading_level == 0 {
                out.push_str(ansi::STRIKETHROUGH);
            }
        }
        "code" => {
            if state.heading_level == 0 {
                out.push_str(ansi::DIM);
            }
        }
        "a" => {
            let url = xml_attr(tag, b"href").unwrap_or_default();
            if state.heading_level == 0 {
                out.push_str(&ansi::link_start(&url));
                out.push_str(ansi::UNDERLINE);
            }
            state.link_url = Some(url);
        }
        "br" => {
            if state.heading_level > 0 {
                state.heading_text.push(' ');
            } else {
                out.push('\n');
            }
        }
        "img" => {
            if let Some(src) = xml_attr(tag, b"src")
                && let Some(path) = state.resolve_image_path(&src)
                && let Some(sixel_data) = sixel::encode_image_file(&path, 800)
            {
                out.push_str(&sixel_data);
                out.push('\n');
            }
        }
        "hr" => {
            out.push_str(&"\u{2500}".repeat(40));
            out.push('\n');
        }
        _ => {}
    }
}

/// Process a closing HTML tag.
fn handle_html_close_tag(
    name: &str,
    state: &mut RenderState,
    out: &mut String,
) {
    match name {
        "b" | "strong" => {
            state.bold = false;
            if state.heading_level == 0 {
                out.push_str(ansi::RESET);
                state.push_style(out);
            }
        }
        "i" | "em" => {
            state.italic = false;
            if state.heading_level == 0 {
                out.push_str(ansi::RESET);
                state.push_style(out);
            }
        }
        "del" | "s" | "strike" => {
            state.strikethrough = false;
            if state.heading_level == 0 {
                out.push_str(ansi::RESET);
                state.push_style(out);
            }
        }
        "code" => {
            if state.heading_level == 0 {
                out.push_str(ansi::RESET);
                state.push_style(out);
            }
        }
        "a" => {
            if state.heading_level == 0 {
                out.push_str(ansi::RESET);
                out.push_str(&ansi::link_end());
            }
            state.link_url = None;
        }
        _ => {}
    }
}

/// Handle a block-level HTML string accumulated from one or more `Html` events.
///
/// Uses quick-xml to walk the tags and text, dispatching to the same rendering
/// logic used for inline tags, plus block-level elements like `<h1>`-`<h6>`,
/// `<pre>`, `<hr>`, `<blockquote>`, etc.
fn handle_block_html(
    html: &str,
    state: &mut RenderState,
    out: &mut String,
    font: &Font,
) {
    let html = html.trim();
    if html.is_empty() {
        return;
    }

    let mut reader = XmlReader::from_str(html);
    reader.config_mut().check_end_names = false;
    reader.config_mut().allow_unmatched_ends = true;

    // Track the outermost block element so we know how to wrap content.
    let mut block_tag: Option<String> = None;
    let mut text_buf = String::new();
    let mut in_pre = false;

    loop {
        match reader.read_event() {
            Ok(XmlEvent::Start(ref e)) => {
                let name = xml_tag_name(e);
                match name.as_str() {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "p" | "pre" | "blockquote" => {
                        block_tag = Some(name.clone());
                        text_buf.clear();
                        if name == "pre" {
                            in_pre = true;
                        }
                    }
                    "hr" => {
                        out.push_str(&"\u{2500}".repeat(40));
                        out.push('\n');
                    }
                    "img" => {
                        if let Some(src) = xml_attr(e, b"src")
                            && let Some(path) = state.resolve_image_path(&src)
                            && let Some(sixel_data) = sixel::encode_image_file(&path, 800)
                        {
                            out.push_str(&sixel_data);
                            out.push('\n');
                        }
                    }
                    "br" => {
                        if block_tag.is_some() {
                            text_buf.push('\n');
                        } else {
                            out.push('\n');
                        }
                    }
                    // Nested inline tags inside blocks — just collect text
                    "code" | "b" | "strong" | "i" | "em" | "a" | "del" | "s" | "strike" => {}
                    _ => {}
                }
            }
            Ok(XmlEvent::Empty(ref e)) => {
                let name = xml_tag_name(e);
                match name.as_str() {
                    "hr" => {
                        out.push_str(&"\u{2500}".repeat(40));
                        out.push('\n');
                    }
                    "br" => {
                        if block_tag.is_some() {
                            text_buf.push('\n');
                        } else {
                            out.push('\n');
                        }
                    }
                    "img" => {
                        if let Some(src) = xml_attr(e, b"src")
                            && let Some(path) = state.resolve_image_path(&src)
                            && let Some(sixel_data) = sixel::encode_image_file(&path, 800)
                        {
                            out.push_str(&sixel_data);
                            out.push('\n');
                        }
                    }
                    _ => {}
                }
            }
            Ok(XmlEvent::Text(ref e)) => {
                if let Ok(text) = e.html_content() {
                    if block_tag.is_some() {
                        text_buf.push_str(&text);
                    } else {
                        out.push_str(&text);
                    }
                }
            }
            Ok(XmlEvent::End(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_ascii_lowercase();
                if block_tag.as_deref() == Some(&name) {
                    // Flush the block
                    emit_block(&name, &text_buf, state, out, font, in_pre);
                    block_tag = None;
                    text_buf.clear();
                    in_pre = false;
                }
            }
            Ok(XmlEvent::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    // If there's leftover text with no block wrapper, output it
    if !text_buf.is_empty() {
        if let Some(tag) = &block_tag {
            emit_block(tag, &text_buf, state, out, font, in_pre);
        } else {
            out.push_str(&text_buf);
            out.push('\n');
        }
    }
}

/// Emit a completed block element.
fn emit_block(
    tag: &str,
    text: &str,
    state: &mut RenderState,
    out: &mut String,
    font: &Font,
    in_pre: bool,
) {
    let text = if in_pre {
        text.to_string()
    } else {
        text.trim().to_string()
    };

    match tag {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level: usize = tag[1..].parse().unwrap_or(1);
            let idx = (level - 1).min(5);
            if !text.is_empty() {
                let (w, h, pixels) =
                    crate::font::render_text(font, &text, HEADING_SIZES[idx], HEADING_COLORS[idx]);
                if w > 0 && h > 0 {
                    out.push_str(&sixel::encode_rgba(w, h, &pixels));
                }
                out.push('\n');
            }
        }
        "p" => {
            out.push_str(&text);
            out.push_str(ansi::RESET);
            out.push_str("\n\n");
        }
        "pre" => {
            out.push_str(ansi::DIM);
            out.push_str("  ");
            for (i, line) in text.lines().enumerate() {
                if i > 0 {
                    out.push_str("\n  ");
                }
                out.push_str(line);
            }
            out.push_str(ansi::RESET);
            out.push_str("\n\n");
        }
        "blockquote" => {
            out.push_str(ansi::DIM);
            out.push_str("  \u{2502} ");
            out.push_str(&text);
            out.push_str(ansi::RESET);
            out.push_str("\n\n");
        }
        _ => {
            if !text.is_empty() {
                out.push_str(&text);
                out.push('\n');
            }
        }
    }

    let _ = state; // avoid unused warning
}

/// Build a comfy-table from accumulated table state and append to output.
fn flush_table(
    state: &mut RenderState,
    out: &mut String,
) {
    use comfy_table::Attribute;
    use comfy_table::Cell;
    use comfy_table::CellAlignment;
    use comfy_table::ContentArrangement;
    use comfy_table::Table;
    use comfy_table::modifiers::UTF8_ROUND_CORNERS;
    use comfy_table::presets::UTF8_FULL_CONDENSED;

    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.apply_modifier(UTF8_ROUND_CORNERS);
    table.set_content_arrangement(ContentArrangement::Dynamic);

    // Set column alignments
    for (i, align) in state.table_alignments.iter().enumerate() {
        let ca = match align {
            Alignment::Left | Alignment::None => CellAlignment::Left,
            Alignment::Center => CellAlignment::Center,
            Alignment::Right => CellAlignment::Right,
        };
        if let Some(col) = table.column_mut(i) {
            col.set_cell_alignment(ca);
        }
    }

    // Header row
    if let Some(header) = state.table_header.take() {
        let cells: Vec<Cell> = header
            .into_iter()
            .map(|(text, _, _, _)| Cell::new(text).add_attribute(Attribute::Bold))
            .collect();
        table.set_header(cells);
    }

    // Data rows
    for row in std::mem::take(&mut state.table_rows) {
        let cells: Vec<Cell> = row
            .into_iter()
            .enumerate()
            .map(|(i, (text, bold, italic, code))| {
                let mut cell = Cell::new(text);
                if bold {
                    cell = cell.add_attribute(Attribute::Bold);
                }
                if italic {
                    cell = cell.add_attribute(Attribute::Italic);
                }
                if code {
                    cell = cell.add_attribute(Attribute::Dim);
                }
                // Apply column alignment per-cell
                if let Some(align) = state.table_alignments.get(i) {
                    cell = cell.set_alignment(match align {
                        Alignment::Left | Alignment::None => CellAlignment::Left,
                        Alignment::Center => CellAlignment::Center,
                        Alignment::Right => CellAlignment::Right,
                    });
                }
                cell
            })
            .collect();
        table.add_row(cells);
    }

    out.push_str(&table.to_string());
    out.push_str("\n\n");

    state.table_alignments.clear();
}

/// Output from rendering markdown, containing the text output and any
/// images still being encoded in background threads.
pub struct RenderOutput {
    pub text: String,
    pub pending_images: Vec<sixel::PendingImage>,
}

/// Render markdown to a string containing ANSI escape codes, sixel sequences,
/// and image placeholders.
///
/// Headings are rendered as sixel images using the provided font.
/// Body text uses ANSI terminal styling. Links use OSC 8 hyperlinks.
/// Images are encoded in background threads — their placeholders
/// (`\x00IMG:id:rows\x00`) are resolved by the pager when scrolled into view.
pub fn render(
    markdown: &str,
    font: &Font,
    base_path: Option<&Path>,
) -> RenderOutput {
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let parser = Parser::new_ext(markdown, options);

    let mut out = String::new();
    let mut state = RenderState::new(base_path.map(|p| p.to_path_buf()));

    for event in parser {
        match event {
            // ── Block-level tags ──────────────────────────────────────
            Event::Start(Tag::Heading { level, .. }) => {
                state.heading_level = match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                };
                state.heading_text.clear();
            }
            Event::End(TagEnd::Heading(_)) => {
                if state.heading_level > 0 {
                    let idx = (state.heading_level - 1).min(5);
                    let size = HEADING_SIZES[idx];
                    let color = HEADING_COLORS[idx];
                    let text = std::mem::take(&mut state.heading_text);
                    let (w, h, pixels) = crate::font::render_text(font, &text, size, color);
                    if w > 0 && h > 0 {
                        out.push_str(&sixel::encode_rgba(w, h, &pixels));
                    }
                    out.push('\n');
                    state.heading_level = 0;
                }
            }

            Event::Start(Tag::Paragraph) => {
                state.para_start = Some(out.len());
            }
            Event::End(TagEnd::Paragraph) => {
                out.push_str(ansi::RESET);
                // Word-wrap the paragraph content
                if let Some(start) = state.para_start.take() {
                    let para_text = out[start..].to_string();
                    out.truncate(start);
                    out.push_str(&ansi::wrap(&para_text, state.term_width));
                }
                out.push_str("\n\n");
            }

            // ── Code blocks ──────────────────────────────────────────
            Event::Start(Tag::CodeBlock(_)) => {
                state.in_code_block = true;
                out.push_str(ansi::DIM);
                out.push_str("  ");
            }
            Event::End(TagEnd::CodeBlock) => {
                state.in_code_block = false;
                out.push_str(ansi::RESET);
                out.push('\n');
            }

            // ── Inline styling ───────────────────────────────────────
            Event::Start(Tag::Emphasis) => {
                if state.in_table() {
                    state.table_cell_italic = true;
                } else {
                    state.italic = true;
                    if state.heading_level == 0 {
                        out.push_str(ansi::ITALIC);
                    }
                }
            }
            Event::End(TagEnd::Emphasis) => {
                if state.in_table() {
                    state.table_cell_italic = false;
                } else {
                    state.italic = false;
                    if state.heading_level == 0 {
                        out.push_str(ansi::RESET);
                        state.push_style(&mut out);
                    }
                }
            }

            Event::Start(Tag::Strong) => {
                if state.in_table() {
                    state.table_cell_bold = true;
                } else {
                    state.bold = true;
                    if state.heading_level == 0 {
                        out.push_str(ansi::BOLD);
                    }
                }
            }
            Event::End(TagEnd::Strong) => {
                if state.in_table() {
                    state.table_cell_bold = false;
                } else {
                    state.bold = false;
                    if state.heading_level == 0 {
                        out.push_str(ansi::RESET);
                        state.push_style(&mut out);
                    }
                }
            }

            Event::Start(Tag::Strikethrough) => {
                state.strikethrough = true;
                if state.heading_level == 0 && !state.in_table() {
                    out.push_str(ansi::STRIKETHROUGH);
                }
            }
            Event::End(TagEnd::Strikethrough) => {
                state.strikethrough = false;
                if state.heading_level == 0 && !state.in_table() {
                    out.push_str(ansi::RESET);
                    state.push_style(&mut out);
                }
            }

            // ── Links ────────────────────────────────────────────────
            Event::Start(Tag::Link { dest_url, .. }) => {
                let url = dest_url.to_string();
                if state.heading_level == 0 {
                    out.push_str(&ansi::link_start(&url));
                    out.push_str(ansi::UNDERLINE);
                }
                state.link_url = Some(url);
            }
            Event::End(TagEnd::Link) => {
                if state.heading_level == 0 {
                    out.push_str(ansi::RESET);
                    out.push_str(&ansi::link_end());
                }
                state.link_url = None;
            }

            // ── Images ───────────────────────────────────────────────
            Event::Start(Tag::Image { dest_url, .. }) => {
                if let Some(path) = state.resolve_image_path(&dest_url) {
                    state.emit_image(&path, &mut out);
                }
            }
            Event::End(TagEnd::Image) => {}

            // ── Lists ────────────────────────────────────────────────
            Event::Start(Tag::List(first_item)) => {
                state.list_stack.push(first_item);
                state.item_index.push(0);
            }
            Event::End(TagEnd::List(_)) => {
                state.list_stack.pop();
                state.item_index.pop();
                if state.list_stack.is_empty() {
                    out.push('\n');
                }
            }
            Event::Start(Tag::Item) => {
                let indent = state.list_indent();
                if let Some(idx) = state.item_index.last_mut() {
                    let prefix = match state.list_stack.last() {
                        Some(Some(start)) => {
                            let n = *start as usize + *idx;
                            format!("{indent}{n}. ")
                        }
                        _ => format!("{indent}  \u{2022} "),
                    };
                    out.push_str(&prefix);
                    *idx += 1;
                }
            }
            Event::End(TagEnd::Item) => {
                out.push_str(ansi::RESET);
                out.push('\n');
            }

            // ── Block quote ──────────────────────────────────────────
            Event::Start(Tag::BlockQuote(_)) => {
                out.push_str(ansi::DIM);
                out.push_str("  \u{2502} ");
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                out.push_str(ansi::RESET);
            }

            // ── Horizontal rule ──────────────────────────────────────
            Event::Rule => {
                out.push_str(&"\u{2500}".repeat(40));
                out.push('\n');
            }

            // ── Inline code ──────────────────────────────────────────
            Event::Code(code) => {
                if state.in_table() {
                    state.table_cell_buf.push_str(&code);
                    state.table_cell_code = true;
                } else if state.heading_level > 0 {
                    state.heading_text.push_str(&code);
                } else {
                    out.push_str(ansi::DIM);
                    out.push_str(&code);
                    out.push_str(ansi::RESET);
                    state.push_style(&mut out);
                }
            }

            // ── Text content ─────────────────────────────────────────
            Event::Text(text) => {
                if state.in_table() {
                    state.table_cell_buf.push_str(&text);
                } else if state.heading_level > 0 {
                    state.heading_text.push_str(&text);
                } else if state.in_code_block {
                    for (i, line) in text.lines().enumerate() {
                        if i > 0 {
                            out.push_str("\n  ");
                        }
                        out.push_str(line);
                    }
                } else {
                    out.push_str(&text);
                }
            }

            Event::SoftBreak => {
                if state.in_table() {
                    state.table_cell_buf.push(' ');
                } else if state.heading_level > 0 {
                    state.heading_text.push(' ');
                } else {
                    out.push(' ');
                }
            }
            Event::HardBreak => {
                if state.in_table() {
                    state.table_cell_buf.push(' ');
                } else if state.heading_level > 0 {
                    state.heading_text.push(' ');
                } else {
                    out.push('\n');
                }
            }

            // ── Tables ────────────────────────────────────────────────
            Event::Start(Tag::Table(alignments)) => {
                state.table_alignments = alignments;
                state.table_header = None;
                state.table_rows.clear();
            }
            Event::End(TagEnd::Table) => {
                flush_table(&mut state, &mut out);
            }
            Event::Start(Tag::TableHead) => {
                state.in_table_head = true;
                state.table_row_cells.clear();
            }
            Event::End(TagEnd::TableHead) => {
                state.in_table_head = false;
                state.table_header = Some(std::mem::take(&mut state.table_row_cells));
            }
            Event::Start(Tag::TableRow) => {
                state.table_row_cells.clear();
            }
            Event::End(TagEnd::TableRow) => {
                let row = std::mem::take(&mut state.table_row_cells);
                state.table_rows.push(row);
            }
            Event::Start(Tag::TableCell) => {
                state.table_cell_buf.clear();
                state.table_cell_bold = false;
                state.table_cell_italic = false;
                state.table_cell_code = false;
            }
            Event::End(TagEnd::TableCell) => {
                let text = std::mem::take(&mut state.table_cell_buf);
                state.table_row_cells.push((
                    text,
                    state.table_cell_bold,
                    state.table_cell_italic,
                    state.table_cell_code,
                ));
            }

            // ── Inline HTML ──────────────────────────────────────────
            Event::InlineHtml(html) => {
                handle_inline_html(&html, &mut state, &mut out);
            }

            // ── Block HTML ───────────────────────────────────────────
            Event::Start(Tag::HtmlBlock) => {
                state.html_block_buf.clear();
            }
            Event::End(TagEnd::HtmlBlock) => {
                let html = std::mem::take(&mut state.html_block_buf);
                handle_block_html(&html, &mut state, &mut out, font);
            }
            Event::Html(html) => {
                state.html_block_buf.push_str(&html);
            }

            // Ignore everything else (footnotes, etc.)
            _ => {}
        }
    }

    RenderOutput {
        text: out,
        pending_images: state.pending_images,
    }
}
