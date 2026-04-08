//! HTML and CSS processing for inline and block-level HTML in markdown.

use quick_xml::events::Event as XmlEvent;
use quick_xml::reader::Reader as XmlReader;

use super::Font;
use super::OutputBlock;
use super::RenderState;
use super::ansi;
use super::flush_text;
use super::push_active_styles;
use super::render_heading_sixel;
use super::resolve_image_source;

// ---------------------------------------------------------------------------
// CSS style → ANSI escape conversion
// ---------------------------------------------------------------------------

/// Parse a CSS `style` attribute value and return ANSI escape sequences.
///
/// Handles `color`, `background-color`, `font-weight`, `font-style`,
/// and `text-decoration` properties.
pub(super) fn css_style_to_ansi(style: &str) -> String {
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
    crate::theme::parse_color(style_value)
}

// ---------------------------------------------------------------------------
// HTML parsing via quick-xml
// ---------------------------------------------------------------------------

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
pub(super) fn handle_inline_html(
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
                && let Some(source) = resolve_image_source(&src, &state.base_path)
            {
                super::emit_image_source(state, &source, out, blocks);
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
pub(super) fn handle_block_html(
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
                        flush_text(out, blocks, &mut state.pending_wikilinks);
                        blocks.push(OutputBlock::DetailsStart { id });
                    }
                    "hr" => {
                        out.push_str(&"\u{2500}".repeat(40));
                        out.push('\n');
                    }
                    "img" | "video" => {
                        if let Some(src) = xml_attr(e, b"src")
                            && let Some(source) = resolve_image_source(&src, &state.base_path)
                        {
                            super::emit_image_source(state, &source, out, blocks);
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
                            && let Some(source) = resolve_image_source(&src, &state.base_path)
                        {
                            super::emit_image_source(state, &source, out, blocks);
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
                    flush_text(out, blocks, &mut state.pending_wikilinks);
                    blocks.push(OutputBlock::DetailsEnd { id });
                } else if block_tag.as_deref() == Some(name.as_str()) {
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
            if !text.is_empty() {
                state.heading_text = text.to_string();
                state.heading_level = level;
                render_heading_sixel(state, out, blocks, font, theme);
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
                flush_text(out, blocks, &mut state.pending_wikilinks);
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
