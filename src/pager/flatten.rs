//! Flatten render output blocks into scrollable display lines.

use std::collections::HashSet;

use super::line::ImageGroup;
use super::line::Line;
use super::line::PositionedSixel;
use super::line::RichSegment;
use super::line::SixelData;

/// Flatten output blocks into display lines for the pager.
/// Each image/sixel is expanded into individual preview rows that scroll
/// naturally. The draw loop replaces them with the full sixel when all
/// rows are visible.
pub(super) fn flatten_blocks(
    output: &crate::renderer::RenderOutput,
    sixel_store: &mut Vec<SixelData>,
) -> Vec<Line> {
    use crate::renderer::OutputBlock;

    let mut lines = Vec::new();
    sixel_store.clear();

    for block in &output.blocks {
        match block {
            OutputBlock::Text {
                wrapped: text,
                wikilinks,
                ..
            } => {
                let base_line_idx = lines.len();
                let mut line_iter = text.split('\n');
                // First line: if the previous line is a RichText (from InlineImage),
                // append this text to it instead of creating a new line
                if let Some(first) = line_iter.next() {
                    if let Some(Line::RichText { segments, .. }) = lines.last_mut() {
                        segments.push(RichSegment::Text(first.to_string()));
                    } else {
                        lines.push(Line::Text(first.to_string()));
                    }
                }
                for line in line_iter {
                    lines.push(Line::Text(line.to_string()));
                }
                // Insert wikilink markers at the correct line positions
                // (inserted after all text lines so indices are stable)
                let mut insertions: Vec<(usize, Line)> = Vec::new();
                for wl in wikilinks {
                    let target_idx = base_line_idx + wl.line;
                    insertions.push((
                        target_idx,
                        Line::WikilinkRef {
                            target: wl.target.clone(),
                            col: wl.col,
                        },
                    ));
                }
                // Insert in reverse order so indices stay valid
                insertions.sort_by(|a, b| b.0.cmp(&a.0));
                for (idx, line) in insertions {
                    if idx <= lines.len() {
                        lines.insert(idx, line);
                    }
                }
            }
            OutputBlock::InlineImage(id) => {
                let p = output.pending_images.get(*id);
                let _rows = p.map(|p| p.estimated_rows()).unwrap_or(1);
                let cols = p
                    .map(|p| {
                        p.preview()
                            .first()
                            .map(|l| crate::renderer::ansi::visible_len(l) as u16)
                            .unwrap_or(4)
                    })
                    .unwrap_or(4);

                // Convert the last Text line to a RichText, or start a new one
                let prev = lines.pop();
                let mut segments = match prev {
                    Some(Line::Text(t)) => vec![RichSegment::Text(t)],
                    Some(Line::RichText { segments, .. }) => segments,
                    other => {
                        if let Some(line) = other {
                            lines.push(line);
                        }
                        vec![]
                    }
                };
                segments.push(RichSegment::Image {
                    image_id: *id,
                    width_cols: cols,
                });

                // Height is max of all inline images in this line
                let height = segments
                    .iter()
                    .filter_map(|s| match s {
                        RichSegment::Image { image_id, .. } => output
                            .pending_images
                            .get(*image_id)
                            .map(|p| p.estimated_rows()),
                        _ => None,
                    })
                    .max()
                    .unwrap_or(1)
                    .max(1);

                lines.push(Line::RichText { segments, height });
            }
            OutputBlock::Sixel {
                data,
                height,
                preview,
            } => {
                let group_id = sixel_store.len();
                sixel_store.push(SixelData {
                    data: data.clone(),
                    height: *height,
                });
                let group = ImageGroup::Sixel(group_id);
                let total = *height;
                for (i, pline) in preview.iter().enumerate().take(total as usize) {
                    lines.push(Line::ImageRow {
                        group,
                        row_in_group: i as u16,
                        total_rows: total,
                        preview_text: pline.clone(),
                    });
                }
                // Pad if preview has fewer lines than height
                for i in preview.len()..total as usize {
                    lines.push(Line::ImageRow {
                        group,
                        row_in_group: i as u16,
                        total_rows: total,
                        preview_text: String::new(),
                    });
                }
            }
            OutputBlock::Image(id) => {
                let p = output.pending_images.get(*id);
                let rows = p.map(|p| p.estimated_rows()).unwrap_or(1);
                let group = ImageGroup::PendingImage(*id);
                let preview = p.map(|p| p.preview()).unwrap_or_default();
                for i in 0..rows {
                    let preview_text = preview.get(i as usize).cloned().unwrap_or_default();
                    lines.push(Line::ImageRow {
                        group,
                        row_in_group: i,
                        total_rows: rows,
                        preview_text,
                    });
                }
            }
            OutputBlock::Gif(id) => {
                let g = output.pending_gifs.get(*id);
                let rows = g.map(|g| g.estimated_rows).unwrap_or(1);
                let preview = g.map(|g| &g.preview[..]).unwrap_or(&[]);
                let is_video = g.is_some_and(|g| g.is_video);
                let group = ImageGroup::Gif(*id);
                for i in 0..rows {
                    let preview_text = preview.get(i as usize).cloned().unwrap_or_default();
                    lines.push(Line::ImageRow {
                        group,
                        row_in_group: i,
                        total_rows: rows,
                        preview_text,
                    });
                }
                if is_video {
                    lines.push(Line::VideoControls { gif_id: *id });
                }
            }
            OutputBlock::Code(id) => {
                let height = output
                    .code_blocks
                    .get(*id)
                    .map(|b| b.lines.len() as u16 + 1) // +1 for scrollbar
                    .unwrap_or(1);
                lines.push(Line::CodeBlock { id: *id, height });
            }
            OutputBlock::SideBySide(items) => {
                flatten_side_by_side(items, output, &mut lines);
            }
            OutputBlock::Table(table) => {
                flatten_table(table, &output.pending_gifs, &mut lines);
            }
            OutputBlock::DetailsStart { id } => {
                lines.push(Line::DetailsStart { id: *id });
            }
            OutputBlock::DetailsSummary { id, text } => {
                lines.push(Line::DetailsSummary {
                    id: *id,
                    text: text.clone(),
                });
            }
            OutputBlock::DetailsEnd { id } => {
                lines.push(Line::DetailsEnd { id: *id });
            }
            OutputBlock::FootnoteRef { label, col } => {
                lines.push(Line::FootnoteRef {
                    label: label.clone(),
                    col: *col,
                });
            }
            OutputBlock::FootnoteDefStart { label } => {
                lines.push(Line::FootnoteDefStart {
                    label: label.clone(),
                });
            }
            OutputBlock::FootnoteDefEnd => {
                lines.push(Line::FootnoteDefEnd);
            }
        }
    }

    // Remove trailing empty lines
    while matches!(lines.last(), Some(Line::Text(t)) if t.is_empty()) {
        lines.pop();
    }

    lines
}

fn flatten_side_by_side(
    items: &[crate::renderer::SideBySideItem],
    output: &crate::renderer::RenderOutput,
    lines: &mut Vec<Line>,
) {
    use crate::renderer::SideBySideItem;

    struct ItemInfo {
        preview: Vec<String>,
        cols: u16,
        rows: u16,
        sixel: PositionedSixel,
    }

    let mut infos = Vec::new();
    let gap = 1u16; // columns between images

    for item in items {
        match item {
            SideBySideItem::Image(id) => {
                if let Some(p) = output.pending_images.get(*id) {
                    let pv = p.preview();
                    let cols = pv
                        .first()
                        .map(|l| crate::renderer::ansi::visible_len(l) as u16)
                        .unwrap_or(1);
                    infos.push(ItemInfo {
                        preview: pv,
                        cols,
                        rows: p.estimated_rows(),
                        sixel: PositionedSixel::Pending {
                            col: 0, // filled in below
                            width: cols,
                            image_id: *id,
                        },
                    });
                }
            }
            SideBySideItem::Gif(id) => {
                if let Some(g) = output.pending_gifs.get(*id) {
                    let cols = g
                        .preview
                        .first()
                        .map(|l| crate::renderer::ansi::visible_len(l) as u16)
                        .unwrap_or(1);
                    infos.push(ItemInfo {
                        preview: g.preview.clone(),
                        cols,
                        rows: g.estimated_rows,
                        sixel: PositionedSixel::Gif {
                            col: 0, // filled in below
                            width: cols,
                            gif_id: *id,
                        },
                    });
                }
            }
        }
    }

    if infos.is_empty() {
        return;
    }

    // Assign column offsets
    let mut col_offset: u16 = 0;
    for info in &mut infos {
        match &mut info.sixel {
            PositionedSixel::Static { col, .. }
            | PositionedSixel::Pending { col, .. }
            | PositionedSixel::Gif { col, .. } => {
                *col = col_offset;
            }
        }
        col_offset += info.cols + gap;
    }

    let max_rows = infos.iter().map(|i| i.rows).max().unwrap_or(0);

    for row_idx in 0..max_rows {
        let mut content = String::new();
        let mut sixels = Vec::new();

        // On first row, record sixels for overlay when fully visible
        if row_idx == 0 {
            for info in &infos {
                sixels.push(info.sixel.clone());
            }
        }

        // Build content with half-block preview text for each image
        for (i, info) in infos.iter().enumerate() {
            if i > 0 {
                for _ in 0..gap {
                    content.push(' ');
                }
            }
            let preview_line = info
                .preview
                .get(row_idx as usize)
                .map(|s| s.as_str())
                .unwrap_or("");
            content.push_str(preview_line);
            // Pad to full width if preview line is shorter
            let vis_len = crate::renderer::ansi::visible_len(preview_line);
            for _ in vis_len..info.cols as usize {
                content.push(' ');
            }
        }

        lines.push(Line::ImageStrip {
            content,
            sixels,
            row_in_group: row_idx,
            total_rows: max_rows,
            video_controls: vec![],
        });
    }
}

fn flatten_table(
    table: &crate::renderer::RenderedTable,
    pending_gifs: &[crate::sixel::PendingGif],
    lines: &mut Vec<Line>,
) {
    use pulldown_cmark::Alignment;

    let w = &table.col_widths;

    // Top border
    lines.push(Line::TableRow {
        content: render_border_str(w, '╭', '┬', '╮', '─'),
    });

    let all_rows: Vec<(&Vec<crate::renderer::RenderedTableCell>, bool)> = table
        .header
        .iter()
        .map(|h| (h, true))
        .chain(table.rows.iter().map(|r| (r, false)))
        .collect();

    for (row, is_header) in &all_rows {
        let row_height = row.iter().map(|c| c.height).max().unwrap_or(1);
        let has_images = row.iter().any(|c| c.sixel.is_some() || c.gif_id.is_some());

        for line_idx in 0..row_height {
            let mut content = String::new();
            let mut sixels: Vec<PositionedSixel> = vec![];
            let mut col_offset: u16 = 0;

            content.push('│');
            col_offset += 1;

            for (col, width) in w.iter().enumerate() {
                content.push(' ');
                col_offset += 1;

                let cell = row.get(col);
                let align = table
                    .alignments
                    .get(col)
                    .copied()
                    .unwrap_or(Alignment::None);

                if let Some(cell) = cell {
                    if cell.sixel.is_some() || cell.gif_id.is_some() {
                        // Image cell: record sixel/gif position on first line
                        if line_idx == 0 {
                            let w = *width as u16;
                            if let Some(gif_id) = cell.gif_id {
                                sixels.push(PositionedSixel::Gif {
                                    col: col_offset,
                                    width: w,
                                    gif_id,
                                });
                            } else if let Some(ref sixel_data) = cell.sixel {
                                sixels.push(PositionedSixel::Static {
                                    col: col_offset,
                                    width: w,
                                    data: sixel_data.clone(),
                                });
                            }
                        }
                        // Use half-block preview text (visible while scrolling)
                        let preview_line = cell
                            .text_lines
                            .get(line_idx)
                            .map(|s| s.as_str())
                            .unwrap_or("");
                        let vis_len = crate::renderer::ansi::visible_len(preview_line);
                        content.push_str(preview_line);
                        for _ in vis_len..*width {
                            content.push(' ');
                        }
                    } else {
                        let line = cell
                            .text_lines
                            .get(line_idx)
                            .map(|s| s.as_str())
                            .unwrap_or("");
                        let vis_len = crate::renderer::ansi::visible_len(line);
                        let padding = width.saturating_sub(vis_len);

                        match align {
                            Alignment::Right => {
                                for _ in 0..padding {
                                    content.push(' ');
                                }
                                content.push_str(line);
                            }
                            Alignment::Center => {
                                let left = padding / 2;
                                for _ in 0..left {
                                    content.push(' ');
                                }
                                content.push_str(line);
                                for _ in 0..padding - left {
                                    content.push(' ');
                                }
                            }
                            _ => {
                                content.push_str(line);
                                for _ in 0..padding {
                                    content.push(' ');
                                }
                            }
                        }
                    }
                } else {
                    for _ in 0..*width {
                        content.push(' ');
                    }
                }

                col_offset += *width as u16;
                content.push(' ');
                col_offset += 1;

                if col < w.len() - 1 {
                    content.push('┆');
                    col_offset += 1;
                }
            }
            content.push('│');

            if has_images {
                lines.push(Line::ImageStrip {
                    content,
                    sixels,
                    row_in_group: line_idx as u16,
                    total_rows: row_height as u16,
                    video_controls: vec![],
                });
            } else {
                lines.push(Line::TableRow { content });
            }
        }

        // Emit video controls row for cells that contain videos
        let mut vid_controls: Vec<(u16, u16, usize)> = Vec::new();
        let mut col_off: u16 = 1; // after leading │
        for (col, width) in w.iter().enumerate() {
            col_off += 1; // space padding
            if let Some(cell) = row.get(col)
                && let Some(gif_id) = cell.gif_id
                && pending_gifs.get(gif_id).is_some_and(|g| g.is_video)
            {
                vid_controls.push((col_off, *width as u16, gif_id));
            }
            col_off += *width as u16 + 1; // cell width + trailing space
            if col < w.len() - 1 {
                col_off += 1; // separator ┆
            }
        }
        if !vid_controls.is_empty() {
            let mut ctrl_content = String::new();
            ctrl_content.push('│');
            for (col, width) in w.iter().enumerate() {
                ctrl_content.push(' ');
                for _ in 0..*width {
                    ctrl_content.push(' ');
                }
                ctrl_content.push(' ');
                if col < w.len() - 1 {
                    ctrl_content.push('┆');
                }
            }
            ctrl_content.push('│');
            lines.push(Line::ImageStrip {
                content: ctrl_content,
                sixels: vec![],
                row_in_group: 0,
                total_rows: 0,
                video_controls: vid_controls,
            });
        }

        // Header separator
        if *is_header {
            lines.push(Line::TableRow {
                content: render_border_str(w, '╞', '╪', '╡', '═'),
            });
        }
    }

    // Bottom border
    lines.push(Line::TableRow {
        content: render_border_str(w, '╰', '┴', '╯', '─'),
    });
}

fn render_border_str(
    widths: &[usize],
    left: char,
    mid: char,
    right: char,
    fill: char,
) -> String {
    let fill_str = fill.to_string();
    let inner: Vec<String> = widths.iter().map(|w| fill_str.repeat(w + 2)).collect();
    format!("{left}{}{right}", inner.join(&mid.to_string()))
}

/// Compute indices of lines that are visible given the current collapsed state.
/// Lines inside collapsed `<details>` blocks are excluded (except the summary).
pub(super) fn visible_indices(
    lines: &[Line],
    collapsed: &HashSet<usize>,
) -> Vec<usize> {
    let mut visible = Vec::new();
    let mut skip_id: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if let Some(sid) = skip_id {
            if matches!(line, Line::DetailsEnd { id } if *id == sid) {
                skip_id = None;
                // Include the DetailsEnd so draw knows the block ended
                visible.push(i);
            }
            continue;
        }
        match line {
            Line::DetailsSummary { id, .. } if collapsed.contains(id) => {
                visible.push(i);
                skip_id = Some(*id);
            }
            _ => visible.push(i),
        }
    }
    visible
}
