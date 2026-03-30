use std::path::Path;

use pulldown_cmark::Alignment;
use pulldown_cmark::CodeBlockKind;
use pulldown_cmark::Event;
use pulldown_cmark::HeadingLevel;
use pulldown_cmark::Options;
use pulldown_cmark::Parser;
use pulldown_cmark::Tag;
use pulldown_cmark::TagEnd;

use crate::font::Font;
use crate::sixel;

/// ANSI escape helpers
pub(crate) mod ansi {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";
    pub const ITALIC: &str = "\x1b[3m";
    pub const UNDERLINE: &str = "\x1b[4m";
    pub const STRIKETHROUGH: &str = "\x1b[9m";

    pub const OVERLINE: &str = "\x1b[53m";

    /// OSC 8 hyperlink start
    pub fn link_start(url: &str) -> String {
        format!("\x1b]8;;{url}\x1b\\")
    }

    /// OSC 8 hyperlink end
    pub fn link_end() -> String {
        "\x1b]8;;\x1b\\".to_string()
    }

    /// Set foreground color using 24-bit true color.
    pub fn fg_rgb(
        r: u8,
        g: u8,
        b: u8,
    ) -> String {
        format!("\x1b[38;2;{r};{g};{b}m")
    }

    /// Set background color using 24-bit true color.
    pub fn bg_rgb(
        r: u8,
        g: u8,
        b: u8,
    ) -> String {
        format!("\x1b[48;2;{r};{g};{b}m")
    }

    /// Count the visible (non-escape) characters in a string.
    pub fn visible_len(s: &str) -> usize {
        let mut len = 0;
        let mut in_escape = false;
        for ch in s.chars() {
            if ch == '\x1b' {
                in_escape = true;
            } else if in_escape {
                if ch.is_ascii_alphabetic() || ch == '\\' {
                    in_escape = false;
                }
            } else {
                len += 1;
            }
        }
        len
    }

    /// Slice a string with ANSI escapes at visible column boundaries.
    /// Returns the substring from visible column `start` with `width` visible
    /// chars. ANSI state is preserved at the boundaries.
    pub fn visible_slice(
        s: &str,
        start: usize,
        width: usize,
    ) -> String {
        let mut result = String::new();
        let mut col = 0;
        let mut in_escape = false;
        let mut pending_escape = String::new();

        for ch in s.chars() {
            if ch == '\x1b' {
                in_escape = true;
                pending_escape.clear();
                pending_escape.push(ch);
                continue;
            }
            if in_escape {
                pending_escape.push(ch);
                if ch.is_ascii_alphabetic() || ch == '\\' {
                    in_escape = false;
                    // Include escapes in the visible region
                    if col >= start && col < start + width {
                        result.push_str(&pending_escape);
                    } else if col < start {
                        // Track style for when we enter the visible region
                        result.push_str(&pending_escape);
                    }
                }
                continue;
            }

            if col >= start && col < start + width {
                result.push(ch);
            }
            col += 1;
            if col >= start + width {
                break;
            }
        }

        result
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

/// Crop an RGBA pixel buffer to a maximum width, keeping the left portion.
fn crop_pixels_width(
    pixels: &[u8],
    w: u32,
    h: u32,
    max_w: u32,
) -> (u32, Vec<u8>) {
    let new_w = max_w.min(w);
    let mut out = Vec::with_capacity((new_w * h * 4) as usize);
    for y in 0..h {
        let row_start = (y * w * 4) as usize;
        let row_end = row_start + (new_w * 4) as usize;
        out.extend_from_slice(&pixels[row_start..row_end]);
    }
    (new_w, out)
}

/// Heading font sizes in pixels for h1..h6
const HEADING_SIZES: [u32; 6] = [48, 40, 32, 28, 24, 20];

/// A fully laid-out table ready for direct rendering to stdout.
pub struct RenderedTable {
    /// Pre-computed column widths in visible characters.
    pub col_widths: Vec<usize>,
    /// Column alignments.
    pub alignments: Vec<Alignment>,
    /// Header row (if any).
    pub header: Option<Vec<RenderedTableCell>>,
    /// Data rows.
    pub rows: Vec<Vec<RenderedTableCell>>,
}

/// A cell in a rendered table.
pub struct RenderedTableCell {
    /// Styled text lines for this cell.
    pub text_lines: Vec<String>,
    /// Optional sixel image to render in the cell.
    pub sixel: Option<String>,
    /// Optional animated GIF ID (index into pending_gifs).
    pub gif_id: Option<usize>,
    /// Visible width of the cell content (for text: max line width; for images: image cols).
    pub width: usize,
    /// Number of terminal rows this cell occupies.
    pub height: usize,
}

/// Image data embedded in a table cell.
struct TableCellImage {
    /// Static sixel data (None if animated GIF).
    sixel: Option<String>,
    /// Animated GIF ID (index into pending_gifs), if this is a GIF.
    gif_id: Option<usize>,
    /// Half-block preview lines (fallback while scrolling / non-pager).
    preview: Vec<String>,
    width_cols: u32,
    height_rows: u16,
}

/// A table cell — either text with styling, or an image.
struct TableCell {
    text: String,
    bold: bool,
    italic: bool,
    code: bool,
    image: Option<TableCellImage>,
}

/// State for table parsing — created on Start(Table), consumed on flush.
struct TableState {
    alignments: Vec<Alignment>,
    in_head: bool,
    cell_buf: String,
    cell_image: Option<TableCellImage>,
    row_cells: Vec<TableCell>,
    header: Option<Vec<TableCell>>,
    rows: Vec<Vec<TableCell>>,
    cell_bold: bool,
    cell_italic: bool,
    cell_code: bool,
}

struct RenderState {
    heading_text: String,
    heading_level: usize,
    bold: bool,
    italic: bool,
    strikethrough: bool,
    link_url: Option<String>,
    in_code_block: bool,
    in_image: bool,
    blockquote_depth: usize,
    code_lang: Option<String>,
    code_buf: String,
    list_stack: Vec<Option<u64>>,
    item_index: Vec<usize>,
    base_path: Option<std::path::PathBuf>,
    html_block_buf: String,
    /// Active table parsing state (None when not inside a table).
    table: Option<TableState>,
    pending_images: Vec<sixel::PendingImage>,
    code_blocks: Vec<CodeBlock>,
    pending_gifs: Vec<sixel::PendingGif>,
    term_width: u16,
    next_details_id: usize,
    para_images: Vec<std::path::PathBuf>,
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
            in_image: false,
            blockquote_depth: 0,
            code_lang: None,
            code_buf: String::new(),
            list_stack: Vec::new(),
            item_index: Vec::new(),
            base_path,
            html_block_buf: String::new(),
            table: None,
            pending_images: Vec::new(),
            code_blocks: Vec::new(),
            pending_gifs: Vec::new(),
            term_width: crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80),
            next_details_id: 0,
            para_images: Vec::new(),
        }
    }

    fn in_table(&self) -> bool {
        self.table.is_some()
    }

    fn table(&mut self) -> &mut TableState {
        self.table.as_mut().expect("not inside a table")
    }

    /// Start background encoding for an image or GIF and emit an output block.
    fn emit_image(
        &mut self,
        path: &std::path::Path,
        out: &mut String,
        blocks: &mut Vec<OutputBlock>,
    ) {
        let max_w = sixel::terminal_pixel_width();
        if let Some(media) = encode_media(path, max_w) {
            flush_text(out, blocks);
            push_media(media, &mut self.pending_images, &mut self.pending_gifs, blocks);
        } else {
            out.push_str(&format!("\x1b[2m[image: {}]\x1b[0m", path.display()));
        }
    }
}

fn push_active_styles(
    bold: bool,
    italic: bool,
    strikethrough: bool,
    out: &mut String,
) {
    if bold {
        out.push_str(ansi::BOLD);
    }
    if italic {
        out.push_str(ansi::ITALIC);
    }
    if strikethrough {
        out.push_str(ansi::STRIKETHROUGH);
    }
}

fn list_indent(depth: usize) -> String {
    "  ".repeat(depth.saturating_sub(1))
}

fn resolve_image_path(
    src: &str,
    base_path: &Option<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    if src.starts_with("http://") || src.starts_with("https://") {
        return None;
    }
    let p = Path::new(src);
    if p.is_absolute() {
        Some(p.to_path_buf())
    } else {
        base_path.as_ref().map(|bp| bp.join(p))
    }
}

// ---------------------------------------------------------------------------
// CSS style → ANSI escape conversion
// ---------------------------------------------------------------------------

/// Parse a CSS `style` attribute value and return ANSI escape sequences.
///
/// Handles `color`, `background-color`, `font-weight`, `font-style`,
/// and `text-decoration` properties.
fn css_style_to_ansi(style: &str) -> String {
    let mut result = String::new();

    for decl in style.split(';') {
        let decl = decl.trim();
        if decl.is_empty() {
            continue;
        }
        let Some((prop, value)) = decl.split_once(':') else {
            continue;
        };
        let prop = prop.trim().to_ascii_lowercase();
        let value = value.trim();

        match prop.as_str() {
            "color" => {
                if let Some((r, g, b)) = parse_css_color_simple(value) {
                    result.push_str(&ansi::fg_rgb(r, g, b));
                }
            }
            "background-color" | "background" => {
                if let Some((r, g, b)) = parse_css_color_simple(value) {
                    result.push_str(&ansi::bg_rgb(r, g, b));
                }
            }
            "font-weight" => {
                let v = value.to_ascii_lowercase();
                if v == "bold" || v == "700" || v == "800" || v == "900" {
                    result.push_str(ansi::BOLD);
                }
            }
            "font-style" => {
                let v = value.to_ascii_lowercase();
                if v == "italic" || v == "oblique" {
                    result.push_str(ansi::ITALIC);
                }
            }
            "text-decoration" | "text-decoration-line" => {
                let v = value.to_ascii_lowercase();
                if v.contains("underline") {
                    result.push_str(ansi::UNDERLINE);
                }
                if v.contains("line-through") {
                    result.push_str(ansi::STRIKETHROUGH);
                }
                if v.contains("overline") {
                    result.push_str(ansi::OVERLINE);
                }
            }
            _ => {}
        }
    }

    result
}

/// Parse a CSS color value from a string (named colors, #hex, rgb()).
fn parse_css_color_simple(style_value: &str) -> Option<(u8, u8, u8)> {
    let val = style_value.trim();

    // #rrggbb
    if let Some(hex) = val.strip_prefix('#') {
        return parse_hex_color(hex);
    }

    // rgb(r, g, b)
    if let Some(inner) = val
        .strip_prefix("rgb(")
        .or_else(|| val.strip_prefix("RGB("))
        && let Some(inner) = inner.strip_suffix(')')
    {
        let parts: Vec<&str> = inner.split(',').collect();
        if parts.len() == 3 {
            let r = parts[0].trim().parse().ok()?;
            let g = parts[1].trim().parse().ok()?;
            let b = parts[2].trim().parse().ok()?;
            return Some((r, g, b));
        }
    }

    // Named colors (common subset)
    match val.to_ascii_lowercase().as_str() {
        "black" => Some((0, 0, 0)),
        "white" => Some((255, 255, 255)),
        "red" => Some((255, 0, 0)),
        "green" => Some((0, 128, 0)),
        "blue" => Some((0, 0, 255)),
        "yellow" => Some((255, 255, 0)),
        "cyan" | "aqua" => Some((0, 255, 255)),
        "magenta" | "fuchsia" => Some((255, 0, 255)),
        "orange" => Some((255, 165, 0)),
        "purple" => Some((128, 0, 128)),
        "pink" => Some((255, 192, 203)),
        "gray" | "grey" => Some((128, 128, 128)),
        "lightgray" | "lightgrey" => Some((211, 211, 211)),
        "darkgray" | "darkgrey" => Some((169, 169, 169)),
        "brown" => Some((139, 69, 19)),
        "navy" => Some((0, 0, 128)),
        "teal" => Some((0, 128, 128)),
        "olive" => Some((128, 128, 0)),
        "maroon" => Some((128, 0, 0)),
        "lime" => Some((0, 255, 0)),
        "silver" => Some((192, 192, 192)),
        "coral" => Some((255, 127, 80)),
        "salmon" => Some((250, 128, 114)),
        "gold" => Some((255, 215, 0)),
        "skyblue" => Some((135, 206, 235)),
        "violet" => Some((238, 130, 238)),
        "indigo" => Some((75, 0, 130)),
        "crimson" => Some((220, 20, 60)),
        _ => None,
    }
}

fn parse_hex_color(hex: &str) -> Option<(u8, u8, u8)> {
    match hex.len() {
        // #rgb
        3 => {
            let r = u8::from_str_radix(&hex[0..1], 16).ok()?;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()?;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()?;
            Some((r * 17, g * 17, b * 17))
        }
        // #rrggbb
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some((r, g, b))
        }
        _ => None,
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
    blocks: &mut Vec<OutputBlock>,
) {
    let mut reader = XmlReader::from_str(tag_str);
    reader.config_mut().check_end_names = false;
    reader.config_mut().allow_unmatched_ends = true;

    while let Ok(event) = reader.read_event() {
        match event {
            XmlEvent::Start(ref e) | XmlEvent::Empty(ref e) => {
                handle_html_open_tag(&xml_tag_name(e), e, state, out, blocks);
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
    blocks: &mut Vec<OutputBlock>,
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
        "img" | "video" => {
            if let Some(src) = xml_attr(tag, b"src")
                && let Some(path) = resolve_image_path(&src, &state.base_path)
            {
                state.emit_image(&path, out, blocks);
            }
        }
        "hr" => {
            out.push_str(&"\u{2500}".repeat(40));
            out.push('\n');
        }
        "u" | "ins" => {
            if state.heading_level == 0 {
                out.push_str(ansi::UNDERLINE);
            }
        }
        "mark" => {
            if state.heading_level == 0 {
                // Yellow background highlight
                out.push_str(&ansi::bg_rgb(255, 255, 0));
                out.push_str(&ansi::fg_rgb(0, 0, 0));
            }
        }
        "kbd" | "samp" => {
            if state.heading_level == 0 {
                // Reverse video for keyboard/sample
                out.push_str(ansi::DIM);
                out.push(' ');
            }
        }
        "var" | "cite" => {
            if state.heading_level == 0 {
                out.push_str(ansi::ITALIC);
            }
        }
        "sup" => {
            // Terminal can't do real superscript — just use dim
            if state.heading_level == 0 {
                out.push_str(ansi::DIM);
            }
        }
        "sub" => {
            if state.heading_level == 0 {
                out.push_str(ansi::DIM);
            }
        }
        // <span> and <div> — apply CSS styles if present
        "span" | "div" | "p" => {
            if state.heading_level == 0
                && let Some(style) = xml_attr(tag, b"style")
            {
                out.push_str(&css_style_to_ansi(&style));
            }
        }
        _ => {
            // For any unknown tag, still try to apply style attribute
            if state.heading_level == 0
                && let Some(style) = xml_attr(tag, b"style")
            {
                out.push_str(&css_style_to_ansi(&style));
            }
        }
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
                push_active_styles(state.bold, state.italic, state.strikethrough, out);
            }
        }
        "i" | "em" | "var" | "cite" => {
            state.italic = false;
            if state.heading_level == 0 {
                out.push_str(ansi::RESET);
                push_active_styles(state.bold, state.italic, state.strikethrough, out);
            }
        }
        "del" | "s" | "strike" => {
            state.strikethrough = false;
            if state.heading_level == 0 {
                out.push_str(ansi::RESET);
                push_active_styles(state.bold, state.italic, state.strikethrough, out);
            }
        }
        "code" | "sup" | "sub" => {
            if state.heading_level == 0 {
                out.push_str(ansi::RESET);
                push_active_styles(state.bold, state.italic, state.strikethrough, out);
            }
        }
        "u" | "ins" | "mark" => {
            if state.heading_level == 0 {
                out.push_str(ansi::RESET);
                push_active_styles(state.bold, state.italic, state.strikethrough, out);
            }
        }
        "kbd" | "samp" => {
            if state.heading_level == 0 {
                out.push(' ');
                out.push_str(ansi::RESET);
                push_active_styles(state.bold, state.italic, state.strikethrough, out);
            }
        }
        "a" => {
            if state.heading_level == 0 {
                out.push_str(ansi::RESET);
                out.push_str(&ansi::link_end());
            }
            state.link_url = None;
        }
        // Styled elements — just reset
        "span" | "div" => {
            if state.heading_level == 0 {
                out.push_str(ansi::RESET);
                push_active_styles(state.bold, state.italic, state.strikethrough, out);
            }
        }
        _ => {
            // Reset after any unknown styled element
            if state.heading_level == 0 {
                out.push_str(ansi::RESET);
                push_active_styles(state.bold, state.italic, state.strikethrough, out);
            }
        }
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
    blocks: &mut Vec<OutputBlock>,
    font: &Font,
    theme: &crate::theme::Theme,
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
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "p" | "pre" | "blockquote"
                    | "summary" => {
                        block_tag = Some(name.clone());
                        text_buf.clear();
                        if name == "pre" {
                            in_pre = true;
                        }
                    }
                    "details" => {
                        let id = state.next_details_id;
                        state.next_details_id += 1;
                        flush_text(out, blocks);
                        blocks.push(OutputBlock::DetailsStart { id });
                    }
                    "hr" => {
                        out.push_str(&"\u{2500}".repeat(40));
                        out.push('\n');
                    }
                    "img" | "video" => {
                        if let Some(src) = xml_attr(e, b"src")
                            && let Some(path) = resolve_image_path(&src, &state.base_path)
                        {
                            state.emit_image(&path, out, blocks);
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
                    "img" | "video" => {
                        if let Some(src) = xml_attr(e, b"src")
                            && let Some(path) = resolve_image_path(&src, &state.base_path)
                        {
                            state.emit_image(&path, out, blocks);
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
                if name == "details" {
                    // Emit end-of-details marker
                    let id = state.next_details_id.saturating_sub(1);
                    flush_text(out, blocks);
                    blocks.push(OutputBlock::DetailsEnd { id });
                } else if block_tag.as_deref() == Some(&name) {
                    emit_block(&name, &text_buf, state, out, blocks, font, theme, in_pre);
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
            emit_block(tag, &text_buf, state, out, blocks, font, theme, in_pre);
        } else {
            out.push_str(&text_buf);
            out.push('\n');
        }
    }
}

/// Emit a completed block element.
#[allow(clippy::too_many_arguments)]
fn emit_block(
    tag: &str,
    text: &str,
    state: &mut RenderState,
    out: &mut String,
    blocks: &mut Vec<OutputBlock>,
    font: &Font,
    theme: &crate::theme::Theme,
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
                let color = theme.heading_color(level);
                let (w, h, pixels) =
                    crate::font::render_text(font, &text, HEADING_SIZES[idx], color);
                let max_w = sixel::terminal_pixel_width();
                let (w, pixels) = if w > max_w {
                    crop_pixels_width(&pixels, w, h, max_w)
                } else {
                    (w, pixels)
                };
                if w > 0 && h > 0 {
                    let snapped_h = sixel::snap_height_to_cells(h);
                    let pixels = if snapped_h > h {
                        let mut padded = pixels;
                        padded.resize((w * snapped_h * 4) as usize, 0);
                        padded
                    } else {
                        pixels
                    };
                    let data = sixel::encode_rgba(w, snapped_h, &pixels);
                    let height = sixel::pixel_height_to_rows(snapped_h);
                    let preview = sixel::preview_from_pixels(&pixels, w, snapped_h, height);
                    flush_text(out, blocks);
                    blocks.push(OutputBlock::Sixel {
                        data,
                        height,
                        preview,
                    });
                }
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
        "summary" => {
            if !text.is_empty() {
                // Emit a summary marker — the pager renders this with a
                // disclosure triangle and handles expand/collapse.
                let id = state.next_details_id.saturating_sub(1);
                flush_text(out, blocks);
                blocks.push(OutputBlock::DetailsSummary {
                    id,
                    text: text.to_string(),
                });
            }
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

/// Render a table with Unicode box-drawing borders.
fn flush_table(
    state: &mut RenderState,
    out: &mut String,
    blocks: &mut Vec<OutputBlock>,
) {
    let table = state.table.take().expect("flush_table called outside table");
    let header_cells = table.header;
    let row_cells = table.rows;
    let alignments = table.alignments;

    let build_rendered_row =
        |cells: &[TableCell], is_header: bool| -> Vec<RenderedTableCell> {
            cells
                .iter()
                .map(|cell| {
                    if let Some(ref img) = cell.image {
                        RenderedTableCell {
                            text_lines: img.preview.clone(),
                            sixel: img.sixel.clone(),
                            gif_id: img.gif_id,
                            width: img.width_cols as usize,
                            height: img.height_rows as usize,
                        }
                    } else {
                        let styled = styled_cell_text(cell, is_header);
                        let lines: Vec<String> =
                            styled.split('\n').map(|s| s.to_string()).collect();
                        let width =
                            lines.iter().map(|l| ansi::visible_len(l)).max().unwrap_or(0);
                        let height = lines.len().max(1);
                        RenderedTableCell {
                            text_lines: lines,
                            sixel: None,
                            gif_id: None,
                            width,
                            height,
                        }
                    }
                })
                .collect()
        };

    let header = header_cells.as_ref().map(|h| build_rendered_row(h, true));
    let rows: Vec<Vec<RenderedTableCell>> =
        row_cells.iter().map(|r| build_rendered_row(r, false)).collect();

    // Compute column widths
    let col_count = alignments.len().max(
        header
            .iter()
            .chain(rows.iter())
            .map(|r| r.len())
            .max()
            .unwrap_or(0),
    );
    let mut col_widths = vec![0usize; col_count];
    for row in header.iter().chain(rows.iter()) {
        for (col, cell) in row.iter().enumerate() {
            if col < col_count {
                col_widths[col] = col_widths[col].max(cell.width);
            }
        }
    }

    flush_text(out, blocks);
    blocks.push(OutputBlock::Table(RenderedTable {
        col_widths,
        alignments,
        header,
        rows,
    }));

}

fn styled_cell_text(cell: &TableCell, is_header: bool) -> String {
    let mut s = String::new();
    if is_header {
        s.push_str(ansi::BOLD);
    }
    if cell.bold {
        s.push_str(ansi::BOLD);
    }
    if cell.italic {
        s.push_str(ansi::ITALIC);
    }
    if cell.code {
        s.push_str(ansi::DIM);
    }
    s.push_str(&cell.text);
    if cell.bold || cell.italic || cell.code || is_header {
        s.push_str(ansi::RESET);
    }
    s
}
/// Output from rendering markdown, containing the text output and any
/// images still being encoded in background threads.
/// A rendered code block with syntax-highlighted lines.
pub struct CodeBlock {
    /// Each line with ANSI color escapes.
    pub lines: Vec<String>,
    /// Maximum visible column width across all lines.
    pub max_width: usize,
}

/// A block of rendered output.
/// One image in a side-by-side group.
pub enum SideBySideItem {
    /// Index into `pending_images`.
    Image(usize),
    /// Index into `pending_gifs`.
    Gif(usize),
}

pub enum OutputBlock {
    /// ANSI-styled text (may contain newlines).
    Text(String),
    /// A sixel image (e.g. a heading) with half-block preview.
    Sixel {
        data: String,
        height: u16,
        preview: Vec<String>,
    },
    /// Index into `pending_images`.
    Image(usize),
    /// Index into `pending_gifs`.
    Gif(usize),
    /// Multiple images laid out side-by-side.
    SideBySide(Vec<SideBySideItem>),
    /// Index into `code_blocks`.
    Code(usize),
    /// A pre-laid-out table rendered directly to stdout by the pager.
    Table(RenderedTable),
    /// Start of a collapsible details section.
    DetailsStart { id: usize },
    /// Summary line for a details section.
    DetailsSummary { id: usize, text: String },
    /// End of a collapsible details section.
    DetailsEnd { id: usize },
}

pub struct RenderOutput {
    pub blocks: Vec<OutputBlock>,
    pub pending_images: Vec<sixel::PendingImage>,
    pub pending_gifs: Vec<sixel::PendingGif>,
    pub code_blocks: Vec<CodeBlock>,
}

/// Flush the text buffer into a Text block if non-empty.
fn flush_text(
    out: &mut String,
    blocks: &mut Vec<OutputBlock>,
) {
    if !out.is_empty() {
        blocks.push(OutputBlock::Text(std::mem::take(out)));
    }
}

/// Result of encoding an image/GIF/video asynchronously.
enum EncodedMedia {
    Image(sixel::PendingImage),
    Gif(sixel::PendingGif),
}

/// Try to encode a media file asynchronously: video → GIF → static image.
/// Returns None if all encoding attempts fail.
fn encode_media(path: &std::path::Path, max_width: u32) -> Option<EncodedMedia> {
    if sixel::is_video(path) {
        if let Some(pending) = sixel::encode_video_async(path, max_width) {
            return Some(EncodedMedia::Gif(pending));
        }
    }
    if let Some(pending) = sixel::encode_gif_async(path, max_width) {
        return Some(EncodedMedia::Gif(pending));
    }
    if let Some(pending) = sixel::encode_image_file_async(path, max_width) {
        return Some(EncodedMedia::Image(pending));
    }
    None
}

/// Push encoded media into the appropriate pending vec and emit an output block.
fn push_media(
    media: EncodedMedia,
    pending_images: &mut Vec<sixel::PendingImage>,
    pending_gifs: &mut Vec<sixel::PendingGif>,
    blocks: &mut Vec<OutputBlock>,
) {
    match media {
        EncodedMedia::Gif(pending) => {
            let id = pending_gifs.len();
            blocks.push(OutputBlock::Gif(id));
            pending_gifs.push(pending);
        }
        EncodedMedia::Image(pending) => {
            let id = pending_images.len();
            blocks.push(OutputBlock::Image(id));
            pending_images.push(pending);
        }
    }
}

/// Push encoded media and return a SideBySideItem.
fn push_media_side_by_side(
    media: EncodedMedia,
    pending_images: &mut Vec<sixel::PendingImage>,
    pending_gifs: &mut Vec<sixel::PendingGif>,
) -> SideBySideItem {
    match media {
        EncodedMedia::Gif(pending) => {
            let id = pending_gifs.len();
            pending_gifs.push(pending);
            SideBySideItem::Gif(id)
        }
        EncodedMedia::Image(pending) => {
            let id = pending_images.len();
            pending_images.push(pending);
            SideBySideItem::Image(id)
        }
    }
}

fn render_heading_sixel(
    state: &mut RenderState,
    out: &mut String,
    blocks: &mut Vec<OutputBlock>,
    font: &Font,
    theme: &crate::theme::Theme,
) {
    if state.heading_level == 0 {
        return;
    }
    let idx = (state.heading_level - 1).min(5);
    let size = HEADING_SIZES[idx];
    let color = theme.heading_color(state.heading_level);
    let text = std::mem::take(&mut state.heading_text);
    let (w, h, pixels) = crate::font::render_text(font, &text, size, color);
    let max_w = sixel::terminal_pixel_width();
    let (w, pixels) = if w > max_w {
        crop_pixels_width(&pixels, w, h, max_w)
    } else {
        (w, pixels)
    };
    if w > 0 && h > 0 {
        // Pad pixel height to a cell boundary so the sixel occupies
        // exactly the predicted number of rows (no fractional overflow).
        let snapped_h = sixel::snap_height_to_cells(h);
        let pixels = if snapped_h > h {
            let mut padded = pixels;
            padded.resize((w * snapped_h * 4) as usize, 0);
            padded
        } else {
            pixels
        };
        let data = sixel::encode_rgba(w, snapped_h, &pixels);
        let height = sixel::pixel_height_to_rows(snapped_h);
        let preview = sixel::preview_from_pixels(&pixels, w, snapped_h, height);
        flush_text(out, blocks);
        blocks.push(OutputBlock::Sixel {
            data,
            height,
            preview,
        });
    }
    state.heading_level = 0;
}

fn end_paragraph(
    state: &mut RenderState,
    out: &mut String,
    blocks: &mut Vec<OutputBlock>,
    theme: &crate::theme::Theme,
) {
    out.push_str(ansi::RESET);
    if state.blockquote_depth > 0 {
        let prefix = format!(
            "{}{}",
            theme.blockquote.to_ansi(),
            "  \u{2502} ".repeat(state.blockquote_depth)
        );
        let indent_width = 4 * state.blockquote_depth as u16;
        let wrap_width = state.term_width.saturating_sub(indent_width);
        let raw = std::mem::take(out);
        let wrapped = ansi::wrap(&raw, wrap_width);
        for (i, line) in wrapped.lines().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(&prefix);
            out.push_str(line);
            out.push_str(ansi::RESET);
        }
    } else {
        let wrapped = ansi::wrap(&std::mem::take(out), state.term_width);
        out.push_str(&wrapped);
    }
    out.push_str("\n\n");

    // Flush buffered paragraph images
    let images = std::mem::take(&mut state.para_images);
    if !images.is_empty() {
        flush_text(out, blocks);
        flush_para_images(state, &images, blocks);
    }
}

fn flush_para_images(
    state: &mut RenderState,
    images: &[std::path::PathBuf],
    blocks: &mut Vec<OutputBlock>,
) {
    let max_w = sixel::terminal_pixel_width();

    if images.len() == 1 {
        if let Some(media) = encode_media(&images[0], max_w) {
            push_media(media, &mut state.pending_images, &mut state.pending_gifs, blocks);
        } else {
            blocks.push(OutputBlock::Text(format!(
                "\x1b[2m[image: {}]\x1b[0m",
                images[0].display()
            )));
        }
        return;
    }

    // Multiple images — encode each at max_w / n for side-by-side
    let per_image_w = max_w / images.len() as u32;
    let mut items = Vec::new();
    for path in images {
        if let Some(media) = encode_media(path, per_image_w) {
            items.push(push_media_side_by_side(
                media,
                &mut state.pending_images,
                &mut state.pending_gifs,
            ));
        }
    }
    if items.is_empty() {
        // All encodings failed — show fallback text
        let names: Vec<_> = images.iter().map(|p| format!("{}", p.display())).collect();
        blocks.push(OutputBlock::Text(format!(
            "\x1b[2m[images: {}]\x1b[0m",
            names.join(", ")
        )));
    } else {
        blocks.push(OutputBlock::SideBySide(items));
    }
}

fn end_code_block(
    state: &mut RenderState,
    out: &mut String,
    blocks: &mut Vec<OutputBlock>,
    theme: &crate::theme::Theme,
    highlighter: &crate::highlight::Highlighter,
) {
    state.in_code_block = false;
    let code = std::mem::take(&mut state.code_buf);

    let highlighted = state
        .code_lang
        .as_deref()
        .and_then(|lang| highlighter.highlight(&code, lang));

    let styled_lines: Vec<String> = if let Some(colored) = highlighted {
        let bg_escape = if colored.starts_with("\x1b[48;") {
            colored
                .find('m')
                .map(|i| colored[..=i].to_string())
                .unwrap_or_default()
        } else {
            String::new()
        };
        colored
            .lines()
            .map(|l| {
                if !bg_escape.is_empty() && !l.starts_with(&bg_escape) {
                    format!("{bg_escape}{l}")
                } else {
                    l.to_string()
                }
            })
            .collect()
    } else {
        let style = theme.code_block.to_ansi();
        code.lines().map(|l| format!("{style}{l}\x1b[0m")).collect()
    };

    let max_width = styled_lines
        .iter()
        .map(|l| ansi::visible_len(l))
        .max()
        .unwrap_or(0);

    flush_text(out, blocks);
    blocks.push(OutputBlock::Code(state.code_blocks.len()));
    state.code_blocks.push(CodeBlock {
        lines: styled_lines,
        max_width,
    });
    state.code_lang = None;
}

fn render_image_in_table_cell(
    path: &std::path::Path,
    state: &mut RenderState,
) {
    let max_cols: u32 = 30;
    let max_px = max_cols * sixel::cell_pixel_width();

    // Try animated encoding (video/GIF) — skip static async since
    // table cells use synchronous encoding with height snapping
    let animated = if sixel::is_video(path) {
        sixel::encode_video_async(path, max_px)
    } else {
        sixel::encode_gif_async(path, max_px)
    };
    if let Some(pending) = animated {
        let gif_id = state.pending_gifs.len();
        let cols = pending
            .preview
            .first()
            .map(|l| ansi::visible_len(l) as u32)
            .unwrap_or(1)
            .min(max_cols);
        let rows = pending.estimated_rows;
        let preview = pending.preview.clone();
        state.pending_gifs.push(pending);
        state.table().cell_image = Some(TableCellImage {
            sixel: None,
            gif_id: Some(gif_id),
            preview,
            width_cols: cols,
            height_rows: rows,
        });
        return;
    }

    // Static image fallback — synchronous encoding with height snapping
    let img = match image::open(path) {
        Ok(img) => img.to_rgba8(),
        Err(_) => return,
    };

    let img = sixel::scale_image(img, max_px);
    let cols = sixel::preview_columns(img.width()).min(max_cols);

    let snapped_h = sixel::snap_height_to_cells(img.height());
    let rows = sixel::pixel_height_to_rows(snapped_h);

    let mut padded = img.clone();
    if snapped_h > padded.height() {
        let mut new_img = image::RgbaImage::new(padded.width(), snapped_h);
        image::imageops::overlay(&mut new_img, &padded, 0, 0);
        padded = new_img;
    }

    let sixel = sixel::encode_rgba(padded.width(), snapped_h, padded.as_raw());
    let preview = sixel::half_block_preview(&img, cols, rows);

    state.table().cell_image = Some(TableCellImage {
        sixel: Some(sixel),
        gif_id: None,
        preview,
        width_cols: cols,
        height_rows: rows,
    });
}

/// Render markdown into a sequence of output blocks.
pub fn render(
    markdown: &str,
    font: &Font,
    base_path: Option<&Path>,
    theme: &crate::theme::Theme,
    highlighter: &crate::highlight::Highlighter,
) -> RenderOutput {
    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    let parser = Parser::new_ext(markdown, options);

    let mut out = String::new();
    let mut blocks: Vec<OutputBlock> = Vec::new();
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
                render_heading_sixel(&mut state, &mut out, &mut blocks, font, theme);
            }

            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                end_paragraph(&mut state, &mut out, &mut blocks, theme);
            }

            // ── Code blocks ──────────────────────────────────────────
            Event::Start(Tag::CodeBlock(kind)) => {
                state.in_code_block = true;
                state.code_buf.clear();
                state.code_lang = match kind {
                    CodeBlockKind::Fenced(lang) if !lang.is_empty() => Some(lang.to_string()),
                    _ => None,
                };
            }
            Event::End(TagEnd::CodeBlock) => {
                end_code_block(&mut state, &mut out, &mut blocks, theme, highlighter);
            }

            // ── Inline styling ───────────────────────────────────────
            Event::Start(Tag::Emphasis) => {
                if state.in_table() {
                    state.table().cell_italic = true;
                } else {
                    state.italic = true;
                    if state.heading_level == 0 {
                        out.push_str(ansi::ITALIC);
                    }
                }
            }
            Event::End(TagEnd::Emphasis) => {
                if state.in_table() {
                    state.table().cell_italic = false;
                } else {
                    state.italic = false;
                    if state.heading_level == 0 {
                        out.push_str(ansi::RESET);
                        push_active_styles(state.bold, state.italic, state.strikethrough, &mut out);
                    }
                }
            }

            Event::Start(Tag::Strong) => {
                if state.in_table() {
                    state.table().cell_bold = true;
                } else {
                    state.bold = true;
                    if state.heading_level == 0 {
                        out.push_str(ansi::BOLD);
                    }
                }
            }
            Event::End(TagEnd::Strong) => {
                if state.in_table() {
                    state.table().cell_bold = false;
                } else {
                    state.bold = false;
                    if state.heading_level == 0 {
                        out.push_str(ansi::RESET);
                        push_active_styles(state.bold, state.italic, state.strikethrough, &mut out);
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
                    push_active_styles(state.bold, state.italic, state.strikethrough, &mut out);
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
                state.in_image = true;
                if let Some(path) = resolve_image_path(&dest_url, &state.base_path) {
                    if state.in_table() {
                        render_image_in_table_cell(&path, &mut state);
                    } else {
                        // Buffer image path for side-by-side detection
                        flush_text(&mut out, &mut blocks);
                        state.para_images.push(path);
                    }
                }
            }
            Event::End(TagEnd::Image) => {
                state.in_image = false;
            }

            // ── Lists ────────────────────────────────────────────────
            Event::Start(Tag::List(first_item)) => {
                if !state.list_stack.is_empty() {
                    // Nested list — start on a new line
                    out.push('\n');
                }
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
                let indent = list_indent(state.list_stack.len());
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
                state.blockquote_depth += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                state.blockquote_depth = state.blockquote_depth.saturating_sub(1);
            }

            // ── Horizontal rule ──────────────────────────────────────
            Event::Rule => {
                out.push_str(&"\u{2500}".repeat(40));
                out.push('\n');
            }

            // ── Inline code ──────────────────────────────────────────
            Event::Code(code) => {
                if state.in_table() {
                    state.table().cell_buf.push_str(&code);
                    state.table().cell_code = true;
                } else if state.heading_level > 0 {
                    state.heading_text.push_str(&code);
                } else {
                    out.push_str(&theme.code_inline.to_ansi());
                    out.push_str(&code);
                    out.push_str(ansi::RESET);
                    push_active_styles(state.bold, state.italic, state.strikethrough, &mut out);
                }
            }

            // ── Text content ─────────────────────────────────────────
            Event::Text(text) => {
                if state.in_image {
                    // Suppress alt text — image is rendered as sixel
                } else if state.in_table() {
                    state.table().cell_buf.push_str(&text);
                } else if state.heading_level > 0 {
                    state.heading_text.push_str(&text);
                } else if state.in_code_block {
                    state.code_buf.push_str(&text);
                } else {
                    out.push_str(&text);
                }
            }

            Event::SoftBreak => {
                if state.in_table() {
                    state.table().cell_buf.push(' ');
                } else if state.heading_level > 0 {
                    state.heading_text.push(' ');
                } else {
                    out.push(' ');
                }
            }
            Event::HardBreak => {
                if state.in_table() {
                    state.table().cell_buf.push(' ');
                } else if state.heading_level > 0 {
                    state.heading_text.push(' ');
                } else {
                    out.push('\n');
                }
            }

            // ── Tables ────────────────────────────────────────────────
            Event::Start(Tag::Table(alignments)) => {
                state.table = Some(TableState {
                    alignments,
                    in_head: false,
                    cell_buf: String::new(),
                    cell_image: None,
                    row_cells: Vec::new(),
                    header: None,
                    rows: Vec::new(),
                    cell_bold: false,
                    cell_italic: false,
                    cell_code: false,
                });
            }
            Event::End(TagEnd::Table) => {
                flush_table(&mut state, &mut out, &mut blocks);
            }
            Event::Start(Tag::TableHead) => {
                state.table().in_head = true;
                state.table().row_cells.clear();
            }
            Event::End(TagEnd::TableHead) => {
                state.table().in_head = false;
                state.table().header = Some(std::mem::take(&mut state.table().row_cells));
            }
            Event::Start(Tag::TableRow) => {
                state.table().row_cells.clear();
            }
            Event::End(TagEnd::TableRow) => {
                let row = std::mem::take(&mut state.table().row_cells);
                state.table().rows.push(row);
            }
            Event::Start(Tag::TableCell) => {
                state.table().cell_buf.clear();
                state.table().cell_image = None;
                state.table().cell_bold = false;
                state.table().cell_italic = false;
                state.table().cell_code = false;
            }
            Event::End(TagEnd::TableCell) => {
                let t = state.table();
                let cell = TableCell {
                    text: std::mem::take(&mut t.cell_buf),
                    image: t.cell_image.take(),
                    bold: t.cell_bold,
                    italic: t.cell_italic,
                    code: t.cell_code,
                };
                state.table().row_cells.push(cell);
            }

            // ── Inline HTML ──────────────────────────────────────────
            Event::InlineHtml(html) => {
                handle_inline_html(&html, &mut state, &mut out, &mut blocks);
            }

            // ── Block HTML ───────────────────────────────────────────
            Event::Start(Tag::HtmlBlock) => {
                state.html_block_buf.clear();
            }
            Event::End(TagEnd::HtmlBlock) => {
                let html = std::mem::take(&mut state.html_block_buf);
                handle_block_html(&html, &mut state, &mut out, &mut blocks, font, theme);
            }
            Event::Html(html) => {
                state.html_block_buf.push_str(&html);
            }

            // ── Task list checkboxes ───────────────────────────────
            Event::TaskListMarker(checked) => {
                if checked {
                    out.push_str("\x1b[32m\u{2611}\x1b[0m "); // ☑ green
                } else {
                    out.push_str("\x1b[2m\u{2610}\x1b[0m "); // ☐ dim
                }
            }

            // Ignore everything else (footnotes, etc.)
            _ => {}
        }
    }

    // Flush any remaining text
    flush_text(&mut out, &mut blocks);

    RenderOutput {
        blocks,
        pending_images: state.pending_images,
        pending_gifs: state.pending_gifs,
        code_blocks: state.code_blocks,
    }
}
