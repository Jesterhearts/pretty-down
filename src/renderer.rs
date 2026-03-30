use std::path::Path;

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

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
    /// List nesting with item index (None = unordered, Some(n) = ordered starting at n)
    list_stack: Vec<Option<u64>>,
    /// Whether we just started a list item (for prefix)
    item_index: Vec<usize>,
    /// Base path for resolving relative image paths
    base_path: Option<std::path::PathBuf>,
    /// Accumulator for block-level HTML (multiple Html events before End(HtmlBlock))
    html_block_buf: String,
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
        }
    }

    fn push_style(&self, out: &mut String) {
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

    fn resolve_image_path(&self, src: &str) -> Option<std::path::PathBuf> {
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
fn xml_attr(tag: &quick_xml::events::BytesStart<'_>, name: &[u8]) -> Option<String> {
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
fn handle_inline_html(tag_str: &str, state: &mut RenderState, out: &mut String) {
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
            if let Some(src) = xml_attr(tag, b"src") {
                if let Some(path) = state.resolve_image_path(&src) {
                    if let Some(sixel_data) = sixel::encode_image_file(&path, 800) {
                        out.push_str(&sixel_data);
                        out.push('\n');
                    }
                }
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
fn handle_html_close_tag(name: &str, state: &mut RenderState, out: &mut String) {
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
fn handle_block_html(html: &str, state: &mut RenderState, out: &mut String, font: &Font) {
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
                        if let Some(src) = xml_attr(e, b"src") {
                            if let Some(path) = state.resolve_image_path(&src) {
                                if let Some(sixel_data) = sixel::encode_image_file(&path, 800) {
                                    out.push_str(&sixel_data);
                                    out.push('\n');
                                }
                            }
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
                        if let Some(src) = xml_attr(e, b"src") {
                            if let Some(path) = state.resolve_image_path(&src) {
                                if let Some(sixel_data) = sixel::encode_image_file(&path, 800) {
                                    out.push_str(&sixel_data);
                                    out.push('\n');
                                }
                            }
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

/// Render markdown to a string containing ANSI escape codes and sixel sequences.
///
/// Headings are rendered as sixel images using the provided font.
/// Body text uses ANSI terminal styling.
/// Links use OSC 8 hyperlinks.
/// Images are rendered inline as sixel.
pub fn render(markdown: &str, font: &Font, base_path: Option<&Path>) -> String {
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

            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                out.push_str(ansi::RESET);
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
                state.italic = true;
                if state.heading_level == 0 {
                    out.push_str(ansi::ITALIC);
                }
            }
            Event::End(TagEnd::Emphasis) => {
                state.italic = false;
                if state.heading_level == 0 {
                    out.push_str(ansi::RESET);
                    state.push_style(&mut out);
                }
            }

            Event::Start(Tag::Strong) => {
                state.bold = true;
                if state.heading_level == 0 {
                    out.push_str(ansi::BOLD);
                }
            }
            Event::End(TagEnd::Strong) => {
                state.bold = false;
                if state.heading_level == 0 {
                    out.push_str(ansi::RESET);
                    state.push_style(&mut out);
                }
            }

            Event::Start(Tag::Strikethrough) => {
                state.strikethrough = true;
                if state.heading_level == 0 {
                    out.push_str(ansi::STRIKETHROUGH);
                }
            }
            Event::End(TagEnd::Strikethrough) => {
                state.strikethrough = false;
                if state.heading_level == 0 {
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
                    if let Some(sixel_data) = sixel::encode_image_file(&path, 800) {
                        out.push_str(&sixel_data);
                        out.push('\n');
                    }
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
                if state.heading_level > 0 {
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
                if state.heading_level > 0 {
                    state.heading_text.push_str(&text);
                } else if state.in_code_block {
                    // Indent each line of code blocks
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
                if state.heading_level > 0 {
                    state.heading_text.push(' ');
                } else {
                    out.push(' ');
                }
            }
            Event::HardBreak => {
                if state.heading_level > 0 {
                    state.heading_text.push(' ');
                } else {
                    out.push('\n');
                }
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

    out
}
