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
    /// Language of the current code block (for syntax highlighting)
    code_lang: Option<String>,
    /// Accumulated code block content (for syntax highlighting)
    code_buf: String,
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
    /// Rendered code blocks for horizontal scrolling.
    code_blocks: Vec<CodeBlock>,
    /// Pending GIF animations, indexed by placeholder ID.
    pending_gifs: Vec<sixel::PendingGif>,
    /// Terminal width for word wrapping.
    term_width: u16,
    /// Counter for assigning unique IDs to `<details>` blocks.
    next_details_id: usize,
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
            code_lang: None,
            code_buf: String::new(),
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
            code_blocks: Vec::new(),
            pending_gifs: Vec::new(),
            term_width: crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80),
            next_details_id: 0,
        }
    }

    fn in_table(&self) -> bool {
        !self.table_alignments.is_empty()
    }

    /// Start background encoding for an image or GIF and emit an output block.
    fn emit_image(
        &mut self,
        path: &std::path::Path,
        out: &mut String,
        blocks: &mut Vec<OutputBlock>,
    ) {
        let max_w = sixel::terminal_pixel_width();
        if let Some(pending) = sixel::encode_gif_async(path, max_w) {
            let id = self.pending_gifs.len();
            flush_text(out, blocks);
            blocks.push(OutputBlock::Gif(id));
            self.pending_gifs.push(pending);
        } else if let Some(pending) = sixel::encode_image_file_async(path, max_w) {
            let id = self.pending_images.len();
            flush_text(out, blocks);
            blocks.push(OutputBlock::Image(id));
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
                && let Some(sixel_data) =
                    sixel::encode_image_file(&path, sixel::terminal_pixel_width())
            {
                out.push_str(&sixel_data);
                out.push('\n');
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
                state.push_style(out);
            }
        }
        "i" | "em" | "var" | "cite" => {
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
        "code" | "sup" | "sub" => {
            if state.heading_level == 0 {
                out.push_str(ansi::RESET);
                state.push_style(out);
            }
        }
        "u" | "ins" | "mark" => {
            if state.heading_level == 0 {
                out.push_str(ansi::RESET);
                state.push_style(out);
            }
        }
        "kbd" | "samp" => {
            if state.heading_level == 0 {
                out.push(' ');
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
        // Styled elements — just reset
        "span" | "div" => {
            if state.heading_level == 0 {
                out.push_str(ansi::RESET);
                state.push_style(out);
            }
        }
        _ => {
            // Reset after any unknown styled element
            if state.heading_level == 0 {
                out.push_str(ansi::RESET);
                state.push_style(out);
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
                    "img" => {
                        if let Some(src) = xml_attr(e, b"src")
                            && let Some(path) = state.resolve_image_path(&src)
                            && let Some(sixel_data) =
                                sixel::encode_image_file(&path, sixel::terminal_pixel_width())
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
                            && let Some(sixel_data) =
                                sixel::encode_image_file(&path, sixel::terminal_pixel_width())
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
/// A rendered code block with syntax-highlighted lines.
pub struct CodeBlock {
    /// Each line with ANSI color escapes.
    pub lines: Vec<String>,
    /// Maximum visible column width across all lines.
    pub max_width: usize,
}

/// A block of rendered output.
pub enum OutputBlock {
    /// ANSI-styled text (may contain newlines).
    Text(String),
    /// A sixel image (e.g. a heading).
    Sixel { data: String, height: u16 },
    /// Index into `pending_images`.
    Image(usize),
    /// Index into `pending_gifs`.
    Gif(usize),
    /// Index into `code_blocks`.
    Code(usize),
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

/// Render markdown into a sequence of output blocks.
pub fn render(
    markdown: &str,
    font: &Font,
    base_path: Option<&Path>,
    theme: &crate::theme::Theme,
    highlighter: &crate::highlight::Highlighter,
) -> RenderOutput {
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
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
                if state.heading_level > 0 {
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
                        let data = sixel::encode_rgba(w, h, &pixels);
                        let height = sixel::pixel_height_to_rows(h);
                        flush_text(&mut out, &mut blocks);
                        blocks.push(OutputBlock::Sixel { data, height });
                    }
                    state.heading_level = 0;
                }
            }

            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                out.push_str(ansi::RESET);
                // Word-wrap the paragraph text
                let wrapped = ansi::wrap(&std::mem::take(&mut out), state.term_width);
                out.push_str(&wrapped);
                out.push_str("\n\n");
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
                state.in_code_block = false;
                let code = std::mem::take(&mut state.code_buf);

                // Build highlighted lines
                let highlighted = state
                    .code_lang
                    .as_deref()
                    .and_then(|lang| highlighter.highlight(&code, lang));

                let styled_lines: Vec<String> = if let Some(colored) = highlighted {
                    // Extract the background escape (if any) to prepend to each line,
                    // since splitting on newlines loses it from all but the first.
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

                // Compute max visible width
                let max_width = styled_lines
                    .iter()
                    .map(|l| ansi::visible_len(l))
                    .max()
                    .unwrap_or(0);

                let id = state.code_blocks.len();
                let _height = styled_lines.len();
                state.code_blocks.push(CodeBlock {
                    lines: styled_lines,
                    max_width,
                });

                flush_text(&mut out, &mut blocks);
                blocks.push(OutputBlock::Code(id));
                state.code_lang = None;
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
                    state.emit_image(&path, &mut out, &mut blocks);
                }
            }
            Event::End(TagEnd::Image) => {}

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
                out.push_str(&theme.blockquote.to_ansi());
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
                    out.push_str(&theme.code_inline.to_ansi());
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
                    // Accumulate code for syntax highlighting at End(CodeBlock)
                    state.code_buf.push_str(&text);
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
                handle_block_html(&html, &mut state, &mut out, &mut blocks, font, theme);
            }
            Event::Html(html) => {
                state.html_block_buf.push_str(&html);
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
