use std::collections::HashSet;
use std::io::Write;
use std::io::{self};
use std::path::Path;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use crossterm::cursor;
use crossterm::event::EnableMouseCapture;
use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use crossterm::event::MouseButton;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use crossterm::event::{self};
use crossterm::terminal::ClearType;
use crossterm::terminal::{self};

use crate::renderer::RenderOutput;

/// Identifies an image group (for sixel replacement of preview rows).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ImageGroup {
    Sixel(usize),        // index into a vec of sixel data
    PendingImage(usize), // index into pending_images
    Gif(usize),          // index into pending_gifs
}

/// Sixel data for an image inside a table cell or side-by-side group.
#[derive(Clone)]
enum PositionedSixel {
    /// Static sixel image at the given column offset.
    Static { col: u16, width: u16, data: String },
    /// Pending image at the given column offset (resolved at draw time).
    Pending {
        col: u16,
        width: u16,
        image_id: usize,
    },
    /// Animated GIF at the given column offset (frame looked up at draw time).
    Gif { col: u16, width: u16, gif_id: usize },
}

/// A segment of a rich text line (text mixed with inline images).
#[derive(Clone)]
enum RichSegment {
    Text(String),
    Image { image_id: usize, width_cols: u16 },
}

/// A display line.
#[derive(Clone)]
enum Line {
    /// A regular text line (may contain ANSI escapes).
    Text(String),
    /// A text line containing inline images (e.g. math equations).
    /// Height may be >1 if an inline image is taller than one row.
    RichText {
        segments: Vec<RichSegment>,
        height: u16,
    },
    /// One row of a half-block image preview, belonging to an image group.
    /// When all rows of the group are visible, the pager replaces them
    /// with the full sixel image.
    ImageRow {
        group: ImageGroup,
        row_in_group: u16,
        total_rows: u16,
        preview_text: String,
    },
    /// Start of a `<details>` block (invisible marker, zero height).
    #[allow(dead_code)]
    DetailsStart { id: usize },
    /// The `<summary>` line for a details block.
    DetailsSummary { id: usize, text: String },
    /// End of a `<details>` block (invisible marker, zero height).
    DetailsEnd { id: usize },
    /// A horizontally scrollable code block.
    CodeBlock { id: usize, height: u16 },
    /// A pure-text table row (borders, text-only cells).
    TableRow { content: String },
    /// A row containing positioned images (table cells or side-by-side).
    /// Shows half-block preview while scrolling; overlays sixels when
    /// all rows of the group are on screen.
    ImageStrip {
        content: String,
        sixels: Vec<PositionedSixel>,
        row_in_group: u16,
        total_rows: u16,
        /// Video controls to render on this row: (col, width, gif_id).
        video_controls: Vec<(u16, u16, usize)>,
    },
    /// Video playback control bar (play/pause + progress).
    VideoControls { gif_id: usize },
    /// Invisible marker: a footnote reference at this column in the text.
    FootnoteRef { label: String, col: usize },
    /// Invisible marker: start of a footnote definition.
    FootnoteDefStart { label: String },
    /// Invisible marker: end of a footnote definition.
    FootnoteDefEnd,
}

impl Line {
    /// Number of terminal rows this line occupies.
    fn rows(&self) -> u16 {
        match self {
            Line::Text(_)
            | Line::ImageRow { .. }
            | Line::TableRow { .. }
            | Line::ImageStrip { .. } => 1,
            Line::RichText { height, .. } => *height,
            Line::CodeBlock { height, .. } => *height,
            Line::VideoControls { .. } => 1,
            Line::DetailsSummary { .. } => 1,
            Line::DetailsStart { .. }
            | Line::DetailsEnd { .. }
            | Line::FootnoteRef { .. }
            | Line::FootnoteDefStart { .. }
            | Line::FootnoteDefEnd => 0,
        }
    }
}

/// Sixel data stored separately, referenced by ImageGroup.
struct SixelData {
    data: String,
    #[allow(dead_code)]
    height: u16,
}

/// Flatten output blocks into display lines for the pager.
/// Each image/sixel is expanded into individual preview rows that scroll
/// naturally. The draw loop replaces them with the full sixel when all
/// rows are visible.
fn flatten_blocks(
    output: &crate::renderer::RenderOutput,
    sixel_store: &mut Vec<SixelData>,
) -> Vec<Line> {
    use crate::renderer::OutputBlock;

    let mut lines = Vec::new();
    sixel_store.clear();

    for block in &output.blocks {
        match block {
            OutputBlock::Text(text) => {
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
    let mut s = String::new();
    s.push(left);
    for (i, w) in widths.iter().enumerate() {
        for _ in 0..w + 2 {
            s.push(fill);
        }
        if i < widths.len() - 1 {
            s.push(mid);
        }
    }
    s.push(right);
    s
}

/// Compute indices of lines that are visible given the current collapsed state.
/// Lines inside collapsed `<details>` blocks are excluded (except the summary).
fn visible_indices(
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

/// Per-document state for the pager.
struct DocumentState {
    /// Display name (filename stem or "untitled")
    name: String,
    /// Absolute path, if from a file (None for stdin)
    path: Option<PathBuf>,
    /// The owned render output.
    output: RenderOutput,
    /// Sixel data store (referenced by ImageGroup::Sixel indices).
    sixel_store: Vec<SixelData>,
    /// Flattened display lines.
    lines: Vec<Line>,
    /// Visible line indices (accounting for collapsed details).
    visible: Vec<usize>,
    /// Current scroll position (index into `visible`).
    scroll_offset: usize,
    /// Collapsed details block IDs.
    collapsed: HashSet<usize>,
    /// Per-GIF animation state: (current frame index, next frame deadline).
    gif_state: Vec<Option<(usize, std::time::Instant)>>,
    /// Per-code-block horizontal scroll offset.
    code_h_scroll: Vec<usize>,
    /// Per-video paused state.
    video_paused: Vec<bool>,
    /// Per-video progress dot position (jitter prevention).
    video_progress_pos: Vec<usize>,
    /// Number of pending images that were ready at last flatten.
    images_ready_count: usize,
}

impl DocumentState {
    fn new(
        name: String,
        path: Option<PathBuf>,
        output: RenderOutput,
    ) -> Self {
        let mut sixel_store = Vec::new();
        let lines = flatten_blocks(&output, &mut sixel_store);
        let collapsed: HashSet<usize> = lines
            .iter()
            .filter_map(|l| match l {
                Line::DetailsSummary { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        let visible = visible_indices(&lines, &collapsed);
        let gif_state = vec![None; output.pending_gifs.len()];
        let code_h_scroll = vec![0; output.code_blocks.len()];
        let video_paused = vec![false; output.pending_gifs.len()];
        let video_progress_pos = vec![0; output.pending_gifs.len()];
        let images_ready_count = output
            .pending_images
            .iter()
            .filter(|p| p.is_ready())
            .count();

        Self {
            name,
            path,
            output,
            sixel_store,
            lines,
            visible,
            scroll_offset: 0,
            collapsed,
            gif_state,
            code_h_scroll,
            video_paused,
            video_progress_pos,
            images_ready_count,
        }
    }

    /// Recompute visible indices from current lines and collapsed state.
    fn recompute_visible(&mut self) {
        self.visible = visible_indices(&self.lines, &self.collapsed);
        if self.scroll_offset >= self.visible.len() {
            self.scroll_offset = self.visible.len().saturating_sub(1);
        }
    }

    /// Re-render from a new output, preserving collapsed state.
    fn apply_output(
        &mut self,
        new_output: RenderOutput,
    ) {
        self.lines = flatten_blocks(&new_output, &mut self.sixel_store);
        self.visible = visible_indices(&self.lines, &self.collapsed);
        self.gif_state = vec![None; new_output.pending_gifs.len()];
        self.video_paused = vec![false; new_output.pending_gifs.len()];
        self.video_progress_pos = vec![0; new_output.pending_gifs.len()];
        self.code_h_scroll = vec![0; new_output.code_blocks.len()];
        if self.scroll_offset >= self.visible.len() {
            self.scroll_offset = self.visible.len().saturating_sub(1);
        }
        self.images_ready_count = new_output
            .pending_images
            .iter()
            .filter(|p| p.is_ready())
            .count();
        self.output = new_output;
    }

    /// Re-flatten from the existing output (e.g. when a pending image finishes).
    fn reflatten(&mut self) {
        self.lines = flatten_blocks(&self.output, &mut self.sixel_store);
        self.visible = visible_indices(&self.lines, &self.collapsed);
        if self.scroll_offset >= self.visible.len() {
            self.scroll_offset = self.visible.len().saturating_sub(1);
        }
        self.images_ready_count = self
            .output
            .pending_images
            .iter()
            .filter(|p| p.is_ready())
            .count();
    }
}

// ── Sidebar ──────────────────────────────────────────────────────────────

/// Whether the sidebar is collapsed or expanded.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SidebarMode {
    /// Hidden — shows a thin "│◂" strip that can be clicked to open.
    Collapsed,
    /// Expanded — shows document list with "│▸" collapse button.
    Open,
    /// File-open dialog is active.
    FileDialog,
}

/// State for the file-open dialog with path autocomplete.
struct FileDialogState {
    /// Current typed text (partial path).
    input: String,
    /// Cursor position in `input`.
    cursor: usize,
    /// Filtered candidate file paths.
    candidates: Vec<PathBuf>,
    /// Index of the highlighted candidate.
    selected: usize,
}

impl FileDialogState {
    fn new() -> Self {
        let mut s = Self {
            input: String::new(),
            cursor: 0,
            candidates: Vec::new(),
            selected: 0,
        };
        s.recompute_candidates();
        s
    }

    fn recompute_candidates(&mut self) {
        self.candidates.clear();
        self.selected = 0;

        // Expand ~ to home directory
        let expanded = expand_path(&self.input);

        let (dir, prefix) = if expanded.contains('/') {
            let p = PathBuf::from(&expanded);
            if expanded.ends_with('/') {
                (p, String::new())
            } else {
                let parent = p.parent().unwrap_or(Path::new(".")).to_path_buf();
                let prefix = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (parent, prefix)
            }
        } else {
            (PathBuf::from("."), expanded.clone())
        };

        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };

        let prefix_lower = prefix.to_ascii_lowercase();

        let mut dirs = Vec::new();
        let mut files = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !prefix.is_empty() && !name.to_ascii_lowercase().starts_with(&prefix_lower) {
                continue;
            }
            // Skip hidden files unless the prefix starts with '.'
            if name.starts_with('.') && !prefix.starts_with('.') {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if is_markdown_file(&name) {
                files.push(path);
            }
        }
        dirs.sort();
        files.sort();
        self.candidates.extend(dirs);
        self.candidates.extend(files);
    }
}

/// Expand shell constructs in a path string:
/// - `~` and `~user` via shellexpand (cross-platform)
/// - `$VAR` and `${VAR}` via shellexpand
/// - `%VAR%` on Windows via manual expansion
fn expand_path(input: &str) -> String {
    // shellexpand handles ~ (via dirs crate) and $VAR / ${VAR}
    let expanded = shellexpand::full(input).unwrap_or(std::borrow::Cow::Borrowed(input));

    // On Windows, also expand %VAR% patterns
    #[cfg(windows)]
    let expanded = expand_percent_vars(&expanded);

    expanded.into_owned()
}

/// Expand `%VAR%` patterns using `std::env::var` (Windows batch syntax).
#[cfg(windows)]
fn expand_percent_vars(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            let var_name: String = chars.by_ref().take_while(|&c| c != '%').collect();
            if !var_name.is_empty() {
                if let Ok(val) = std::env::var(&var_name) {
                    result.push_str(&val);
                    continue;
                }
            }
            // Not a valid variable — put back the literal %name%
            result.push('%');
            result.push_str(&var_name);
            result.push('%');
        } else {
            result.push(ch);
        }
    }
    result
}

fn is_markdown_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".md") || lower.ends_with(".markdown") || lower.ends_with(".mdx")
}

struct SidebarState {
    mode: SidebarMode,
    /// Column width when open (0 when collapsed drawing is handled separately).
    open_width: u16,
    /// Index of the active document.
    active_doc: usize,
    /// File-open dialog state.
    dialog: FileDialogState,
}

impl SidebarState {
    fn new() -> Self {
        Self {
            mode: SidebarMode::Collapsed,
            open_width: 26,
            active_doc: 0,
            dialog: FileDialogState::new(),
        }
    }

    /// Effective column width consumed by the sidebar.
    fn width(&self) -> u16 {
        match self.mode {
            SidebarMode::Collapsed => 2,
            SidebarMode::Open | SidebarMode::FileDialog => self.open_width,
        }
    }
}

/// Draw the sidebar into the terminal.
fn draw_sidebar(
    stdout: &mut io::Stdout,
    sidebar: &SidebarState,
    doc_names: &[&str],
    viewport_rows: u16,
) {
    let w = sidebar.width();

    match sidebar.mode {
        SidebarMode::Collapsed => {
            // Draw thin vertical line with ◂ indicator
            for row in 0..viewport_rows {
                crossterm::execute!(stdout, cursor::MoveTo(0, row)).unwrap();
                if row == viewport_rows / 2 {
                    write!(stdout, "\x1b[2m│\x1b[0m\x1b[1m❰\x1b[0m").unwrap();
                } else {
                    write!(stdout, "\x1b[2m│\x1b[0m ").unwrap();
                }
            }
        }
        SidebarMode::Open | SidebarMode::FileDialog => {
            let content_w = w.saturating_sub(1) as usize; // -1 for separator

            // Row 0: collapse button
            crossterm::execute!(stdout, cursor::MoveTo(0, 0)).unwrap();
            let header = format!(
                " \x1b[1m▸\x1b[0m {:<width$}",
                "Documents",
                width = content_w.saturating_sub(3)
            );
            write!(stdout, "{}", &header[..header.len().min((w as usize) * 4)]).unwrap();

            // Row 1: [+] Open file button
            crossterm::execute!(stdout, cursor::MoveTo(0, 1)).unwrap();
            let open_btn = format!(" \x1b[2m[+] Open...\x1b[0m");
            write!(stdout, "{open_btn}").unwrap();
            // Pad to sidebar width
            let btn_vis = crate::renderer::ansi::visible_len(&open_btn);
            for _ in btn_vis..content_w {
                write!(stdout, " ").unwrap();
            }

            // Row 2+: document list
            for (i, &name) in doc_names.iter().enumerate() {
                let row = i as u16 + 2;
                if row >= viewport_rows {
                    break;
                }
                crossterm::execute!(stdout, cursor::MoveTo(0, row)).unwrap();

                // Truncate name to fit
                let max_name = content_w.saturating_sub(2);
                let display: String = if name.len() > max_name && max_name > 2 {
                    format!("{}…", &name[..max_name - 1])
                } else {
                    name.to_string()
                };

                if i == sidebar.active_doc {
                    write!(stdout, " \x1b[4m{display}\x1b[0m").unwrap();
                } else {
                    write!(stdout, " \x1b[2m{display}\x1b[0m").unwrap();
                }
                let name_len = display.len() + 1;
                for _ in name_len..content_w {
                    write!(stdout, " ").unwrap();
                }
            }

            let doc_end = doc_names.len() as u16 + 2;
            for row in doc_end..viewport_rows {
                crossterm::execute!(stdout, cursor::MoveTo(0, row)).unwrap();
                for _ in 0..content_w {
                    write!(stdout, " ").unwrap();
                }
            }

            // Draw separator on rightmost column
            for row in 0..viewport_rows {
                crossterm::execute!(stdout, cursor::MoveTo(w - 1, row)).unwrap();
                write!(stdout, "\x1b[2m│\x1b[0m").unwrap();
            }

            // File dialog overlay (if active)
            if sidebar.mode == SidebarMode::FileDialog {
                draw_file_dialog(stdout, sidebar, viewport_rows);
            }
        }
    }
}

/// Draw the file-open dialog as an overlay.
fn draw_file_dialog(
    stdout: &mut io::Stdout,
    sidebar: &SidebarState,
    viewport_rows: u16,
) {
    let dialog = &sidebar.dialog;
    let dialog_width = sidebar.open_width.max(40) as usize;

    // Row 0: label
    crossterm::execute!(stdout, cursor::MoveTo(1, 0)).unwrap();
    write!(stdout, "\x1b[7m Open file: \x1b[0m").unwrap();

    // Row 1: input field
    crossterm::execute!(stdout, cursor::MoveTo(1, 1)).unwrap();
    let input_display: String = if dialog.input.len() > dialog_width - 3 {
        let start = dialog.input.len() - (dialog_width - 3);
        format!("…{}", &dialog.input[start..])
    } else {
        dialog.input.clone()
    };
    write!(stdout, " {input_display}\x1b[7m \x1b[0m").unwrap();
    // Pad
    let used = input_display.len() + 2;
    for _ in used..dialog_width {
        write!(stdout, " ").unwrap();
    }

    // Row 2+: candidates
    let max_candidates = (viewport_rows as usize).saturating_sub(3);
    for (i, candidate) in dialog.candidates.iter().enumerate().take(max_candidates) {
        let row = i as u16 + 2;
        crossterm::execute!(stdout, cursor::MoveTo(1, row)).unwrap();

        let display = candidate.to_string_lossy();
        let is_dir = candidate.is_dir();
        let suffix = if is_dir { "/" } else { "" };
        let truncated: String = if display.len() + suffix.len() > dialog_width - 2 {
            format!(
                "…{}{}",
                &display[display.len().saturating_sub(dialog_width - 3)..],
                suffix
            )
        } else {
            format!("{display}{suffix}")
        };

        if i == dialog.selected {
            write!(
                stdout,
                " \x1b[7m{truncated:<width$}\x1b[0m",
                width = dialog_width - 2
            )
            .unwrap();
        } else {
            write!(stdout, " {truncated:<width$}", width = dialog_width - 2).unwrap();
        }
    }
}

/// Set up a file watcher.
fn setup_watcher(path: &Path) -> Option<(mpsc::Receiver<()>, notify::RecommendedWatcher)> {
    use notify::Watcher;

    let (tx, rx) = mpsc::channel();
    let mut watcher =
        notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                use notify::EventKind::*;
                if matches!(event.kind, Modify(_) | Create(_)) {
                    let _ = tx.send(());
                }
            }
        })
        .ok()?;

    watcher
        .watch(path, notify::RecursiveMode::NonRecursive)
        .ok()?;

    Some((rx, watcher))
}

/// Run the interactive pager.
pub fn run(
    output: RenderOutput,
    file_name: Option<&str>,
    file_path: Option<&Path>,
    watch_path: Option<&Path>,
    render_fn: &dyn Fn(&Path) -> RenderOutput,
) {
    let name = file_name.unwrap_or("untitled").to_string();
    let doc_path = file_path.map(|p| p.to_path_buf());

    let mut documents = vec![DocumentState::new(name, doc_path, output)];
    let mut sidebar = SidebarState::new();

    if documents[sidebar.active_doc].lines.is_empty() {
        return;
    }

    let mut stdout = io::stdout();

    let (mut term_cols, mut term_rows) = terminal::size().unwrap_or((80, 24));
    let mut viewport_rows = term_rows.saturating_sub(1);

    // If content fits and no watch and no pending images, just print directly
    let has_pending;
    {
        let d = &documents[sidebar.active_doc];
        let total_rows: u16 = d.lines.iter().map(|l| l.rows()).sum();
        has_pending = d.lines.iter().any(|l| {
            matches!(
                l,
                Line::ImageRow {
                    group: ImageGroup::PendingImage(_),
                    ..
                }
            )
        });
        if total_rows <= viewport_rows && watch_path.is_none() && !has_pending {
            print_output(&d.output);
            return;
        }
    }

    let watcher_state = watch_path.and_then(setup_watcher);
    let watch_rx = watcher_state.as_ref().map(|(rx, _)| rx);
    let _watcher = watcher_state.as_ref().map(|(_, w)| w);
    let watching = watch_rx.is_some();

    terminal::enable_raw_mode().unwrap();
    crossterm::execute!(
        stdout,
        terminal::EnterAlternateScreen,
        cursor::Hide,
        EnableMouseCapture,
    )
    .unwrap();

    let mut needs_redraw = true;

    let mut last_draw = DrawResult {
        summary_rows: Vec::new(),
        video_control_rows: Vec::new(),
        footnote_rows: Vec::new(),
    };

    loop {
        let i = sidebar.active_doc;

        if {
            let d = &mut documents[i];
            advance_gif_frames(&mut d.gif_state, &d.output.pending_gifs, &d.video_paused)
        } {
            needs_redraw = true;
        }

        // Re-flatten if a pending image just finished loading
        {
            let ready_now = documents[i]
                .output
                .pending_images
                .iter()
                .filter(|p| p.is_ready())
                .count();
            if ready_now > documents[i].images_ready_count {
                documents[i].reflatten();
                needs_redraw = true;
            }
        }

        if needs_redraw {
            // Begin synchronized update — all draws are batched until the
            // matching end marker, preventing flicker.
            write!(stdout, "\x1b[?2026h").unwrap();

            let sw = sidebar.width();
            {
                let d = &mut documents[i];
                let gif_frames: Vec<usize> = d
                    .gif_state
                    .iter()
                    .map(|slot| slot.map(|(idx, _)| idx).unwrap_or(0))
                    .collect();

                last_draw = draw_screen(
                    &mut stdout,
                    &d.lines,
                    &d.visible,
                    &d.collapsed,
                    &d.sixel_store,
                    &d.output.pending_images,
                    &d.output.pending_gifs,
                    &gif_frames,
                    &d.video_paused,
                    &mut d.video_progress_pos,
                    &d.output.code_blocks,
                    &d.code_h_scroll,
                    d.scroll_offset,
                    viewport_rows,
                    term_cols,
                    term_rows,
                    watching,
                    sw,
                );
            }
            {
                let names: Vec<&str> = documents.iter().map(|d| d.name.as_str()).collect();
                draw_sidebar(&mut stdout, &sidebar, &names, viewport_rows);
            }

            // End synchronized update — terminal flushes the buffered frame.
            write!(stdout, "\x1b[?2026l").unwrap();
            stdout.flush().unwrap();

            // Register any newly visible GIFs that aren't tracked yet
            {
                let d = &mut documents[i];
                let scroll = d.scroll_offset;
                let vis_len = d.visible.len();
                for vi_idx in scroll..vis_len {
                    let vi = d.visible[vi_idx];
                    match &d.lines[vi] {
                        Line::ImageRow {
                            group: ImageGroup::Gif(id),
                            row_in_group: 0,
                            ..
                        } => {
                            let id = *id;
                            register_gif(id, &mut d.gif_state, &d.output.pending_gifs);
                        }
                        Line::ImageStrip { sixels, .. } => {
                            let gif_ids: Vec<usize> = sixels
                                .iter()
                                .filter_map(|s| match s {
                                    PositionedSixel::Gif { gif_id, .. } => Some(*gif_id),
                                    _ => None,
                                })
                                .collect();
                            for gid in gif_ids {
                                register_gif(gid, &mut d.gif_state, &d.output.pending_gifs);
                            }
                        }
                        _ => {}
                    }
                }
            }
            needs_redraw = false;
        }

        // Check for file changes
        if let Some(rx) = watch_rx
            && rx.try_recv().is_ok()
        {
            while rx.try_recv().is_ok() {}
            std::thread::sleep(Duration::from_millis(50));

            if let Some(p) = documents[i].path.clone() {
                documents[i].apply_output(render_fn(&p));
            }
            needs_redraw = true;
            continue;
        }

        {
            if check_visible_images_ready(
                &documents[i].lines,
                &documents[i].visible,
                &documents[i].output.pending_images,
                documents[i].scroll_offset,
                viewport_rows,
            ) {
                needs_redraw = true;
            }
        }

        // Find the earliest GIF frame deadline
        let next_gif_deadline = documents[i]
            .gif_state
            .iter()
            .filter_map(|slot| slot.as_ref().map(|(_, deadline)| *deadline))
            .min();

        let poll_timeout = if let Some(t) = next_gif_deadline {
            t.saturating_duration_since(std::time::Instant::now())
                .max(Duration::from_millis(1))
        } else if watching || has_pending {
            Duration::from_millis(50)
        } else {
            Duration::from_secs(60)
        };

        if !event::poll(poll_timeout).unwrap_or(false) {
            continue;
        }

        let Ok(ev) = event::read() else {
            continue;
        };

        // Handle terminal resize — re-render with new dimensions
        if let Event::Resize(cols, rows) = ev {
            term_cols = cols;
            viewport_rows = rows.saturating_sub(1);
            term_rows = rows;
            crate::sixel::invalidate_terminal_size();
            if let Some(p) = documents[i].path.clone() {
                documents[i].apply_output(render_fn(&p));
            }
            needs_redraw = true;
            continue;
        }

        // ── Sidebar events ────────────────────────────────────────

        // File dialog keyboard handling (highest priority when active)
        if sidebar.mode == SidebarMode::FileDialog {
            if let Event::Key(key) = ev {
                match key.code {
                    KeyCode::Esc => {
                        sidebar.mode = SidebarMode::Open;
                        needs_redraw = true;
                    }
                    KeyCode::Enter => {
                        if let Some(selected) = sidebar
                            .dialog
                            .candidates
                            .get(sidebar.dialog.selected)
                            .cloned()
                        {
                            if selected.is_dir() {
                                sidebar.dialog.input = selected.to_string_lossy().into_owned();
                                if !sidebar.dialog.input.ends_with('/') {
                                    sidebar.dialog.input.push('/');
                                }
                                sidebar.dialog.cursor = sidebar.dialog.input.len();
                                sidebar.dialog.recompute_candidates();
                            } else {
                                // Open the file
                                let new_output = render_fn(&selected);
                                let name = selected
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| "untitled".into());
                                let doc_path = Some(selected);
                                documents.push(DocumentState::new(name, doc_path, new_output));
                                sidebar.active_doc = documents.len() - 1;
                                sidebar.mode = SidebarMode::Open;
                            }
                        }
                        needs_redraw = true;
                    }
                    KeyCode::Backspace => {
                        if !sidebar.dialog.input.is_empty() {
                            sidebar.dialog.input.pop();
                            sidebar.dialog.cursor = sidebar.dialog.input.len();
                            sidebar.dialog.recompute_candidates();
                        }
                        needs_redraw = true;
                    }
                    KeyCode::Up => {
                        sidebar.dialog.selected = sidebar.dialog.selected.saturating_sub(1);
                        needs_redraw = true;
                    }
                    KeyCode::Down => {
                        if sidebar.dialog.selected + 1 < sidebar.dialog.candidates.len() {
                            sidebar.dialog.selected += 1;
                        }
                        needs_redraw = true;
                    }
                    KeyCode::Tab => {
                        // Autocomplete: fill in selected candidate
                        if let Some(selected) =
                            sidebar.dialog.candidates.get(sidebar.dialog.selected)
                        {
                            sidebar.dialog.input = selected.to_string_lossy().into_owned();
                            if selected.is_dir() && !sidebar.dialog.input.ends_with('/') {
                                sidebar.dialog.input.push('/');
                            }
                            sidebar.dialog.cursor = sidebar.dialog.input.len();
                            sidebar.dialog.recompute_candidates();
                        }
                        needs_redraw = true;
                    }
                    KeyCode::Char(c) => {
                        sidebar.dialog.input.push(c);
                        sidebar.dialog.cursor = sidebar.dialog.input.len();
                        sidebar.dialog.recompute_candidates();
                        needs_redraw = true;
                    }
                    _ => {}
                }
                continue;
            }
        }

        // Sidebar mouse click handling
        if let Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            ..
        }) = ev
        {
            let sw = sidebar.width();
            if column < sw {
                match sidebar.mode {
                    SidebarMode::Collapsed => {
                        sidebar.mode = SidebarMode::Open;
                        needs_redraw = true;
                        continue;
                    }
                    SidebarMode::Open => {
                        if row == 0 {
                            // Collapse button
                            sidebar.mode = SidebarMode::Collapsed;
                            needs_redraw = true;
                            continue;
                        }
                        if row == 1 {
                            // [+] Open button
                            sidebar.dialog = FileDialogState::new();
                            sidebar.mode = SidebarMode::FileDialog;
                            needs_redraw = true;
                            continue;
                        }
                        // Document list (row 2+)
                        let doc_idx = (row - 2) as usize;
                        if doc_idx < documents.len() && doc_idx != sidebar.active_doc {
                            sidebar.active_doc = doc_idx;
                            needs_redraw = true;
                            continue;
                        }
                    }
                    SidebarMode::FileDialog => {
                        // Click on candidate in dialog
                        if row >= 2 {
                            let candidate_idx = (row - 2) as usize;
                            if candidate_idx < sidebar.dialog.candidates.len() {
                                sidebar.dialog.selected = candidate_idx;
                                needs_redraw = true;
                                continue;
                            }
                        }
                    }
                }
            }
        }

        // Handle mouse clicks on details summaries
        if let Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row,
            ..
        }) = ev
            && let Some(&(_, id)) = last_draw.summary_rows.iter().find(|(r, _)| *r == row)
        {
            if documents[i].collapsed.contains(&id) {
                documents[i].collapsed.remove(&id);
            } else {
                documents[i].collapsed.insert(id);
            }
            documents[i].recompute_visible();
            needs_redraw = true;
            continue;
        }

        // Handle click on video control bar
        if let Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row,
            ..
        }) = ev
            && let Some(&(_, gif_id)) = last_draw.video_control_rows.iter().find(|(r, _)| *r == row)
        {
            if let Some(p) = documents[i].video_paused.get_mut(gif_id) {
                *p = !*p;
            }
            needs_redraw = true;
            continue;
        }

        // Handle click on footnote (jump to def or back to ref)
        if let Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row,
            column,
            ..
        }) = ev
            && let Some((_, _, _, label, is_def)) = last_draw
                .footnote_rows
                .iter()
                .find(|(r, c0, c1, _, _)| *r == row && column >= *c0 && column < *c1)
            && let Some(target_vi) =
                find_footnote_target(&documents[i].lines, &documents[i].visible, label, !is_def)
        {
            documents[i].scroll_offset = target_vi;
            needs_redraw = true;
            continue;
        }

        // Handle mouse horizontal scroll on code blocks
        {
            let h_scroll_dir = match &ev {
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::ScrollRight,
                    ..
                }) => Some((true, None)),
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::ScrollLeft,
                    ..
                }) => Some((false, None)),
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::ScrollDown,
                    row,
                    modifiers,
                    ..
                }) if modifiers.contains(KeyModifiers::SHIFT) => Some((true, Some(*row))),
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::ScrollUp,
                    row,
                    modifiers,
                    ..
                }) if modifiers.contains(KeyModifiers::SHIFT) => Some((false, Some(*row))),
                _ => None,
            };
            if let Some((right, row_override)) = h_scroll_dir {
                let row = row_override.unwrap_or({
                    if let Event::Mouse(MouseEvent { row, .. }) = &ev {
                        *row
                    } else {
                        0
                    }
                });
                if let Some(block_id) = find_code_block_at_row(
                    &documents[i].lines,
                    &documents[i].visible,
                    &documents[i].output.code_blocks,
                    documents[i].scroll_offset,
                    viewport_rows,
                    row,
                ) {
                    let max = documents[i].output.code_blocks[block_id]
                        .max_width
                        .saturating_sub(term_cols as usize);
                    if let Some(entry) = documents[i].code_h_scroll.get_mut(block_id) {
                        if right {
                            *entry = (*entry + 4).min(max);
                        } else {
                            *entry = entry.saturating_sub(4);
                        }
                        needs_redraw = true;
                        continue;
                    }
                }
            }
        }

        // Handle mouse vertical scroll
        if let Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            ..
        }) = ev
        {
            documents[i].scroll_offset =
                advance_lines(&documents[i].visible, documents[i].scroll_offset, 3);
            needs_redraw = true;
            continue;
        }
        if let Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            ..
        }) = ev
        {
            documents[i].scroll_offset = retreat_lines(documents[i].scroll_offset, 3);
            needs_redraw = true;
            continue;
        }

        if let Event::Key(key) = ev {
            match key {
                // Toggle sidebar
                KeyEvent {
                    code: KeyCode::Tab, ..
                } => {
                    sidebar.mode = match sidebar.mode {
                        SidebarMode::Collapsed => SidebarMode::Open,
                        SidebarMode::Open => SidebarMode::Collapsed,
                        SidebarMode::FileDialog => SidebarMode::Collapsed,
                    };
                    needs_redraw = true;
                }

                // Open file dialog
                KeyEvent {
                    code: KeyCode::Char('o'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                } => {
                    sidebar.dialog = FileDialogState::new();
                    sidebar.mode = SidebarMode::FileDialog;
                    needs_redraw = true;
                }

                KeyEvent {
                    code: KeyCode::Char('q'),
                    ..
                }
                | KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }
                | KeyEvent {
                    code: KeyCode::Esc, ..
                } => break,

                // Scroll down
                KeyEvent {
                    code: KeyCode::Char('j') | KeyCode::Down,
                    ..
                } => {
                    documents[i].scroll_offset =
                        advance_lines(&documents[i].visible, documents[i].scroll_offset, 1);
                    needs_redraw = true;
                }
                // Scroll up
                KeyEvent {
                    code: KeyCode::Char('k') | KeyCode::Up,
                    ..
                } => {
                    documents[i].scroll_offset = retreat_lines(documents[i].scroll_offset, 1);
                    needs_redraw = true;
                }
                // Half page down
                KeyEvent {
                    code: KeyCode::Char('d'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }
                | KeyEvent {
                    code: KeyCode::Char('d'),
                    ..
                }
                | KeyEvent {
                    code: KeyCode::PageDown,
                    ..
                } => {
                    let half = (viewport_rows / 2) as usize;
                    documents[i].scroll_offset =
                        advance_lines(&documents[i].visible, documents[i].scroll_offset, half);
                    needs_redraw = true;
                }
                // Half page up
                KeyEvent {
                    code: KeyCode::Char('u'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }
                | KeyEvent {
                    code: KeyCode::Char('u'),
                    ..
                }
                | KeyEvent {
                    code: KeyCode::PageUp,
                    ..
                } => {
                    let half = (viewport_rows / 2) as usize;
                    documents[i].scroll_offset = retreat_lines(documents[i].scroll_offset, half);
                    needs_redraw = true;
                }
                // Top
                KeyEvent {
                    code: KeyCode::Char('g'),
                    ..
                }
                | KeyEvent {
                    code: KeyCode::Home,
                    ..
                } => {
                    documents[i].scroll_offset = 0;
                    needs_redraw = true;
                }
                // Bottom
                KeyEvent {
                    code: KeyCode::Char('G'),
                    ..
                }
                | KeyEvent {
                    code: KeyCode::End, ..
                } => {
                    documents[i].scroll_offset =
                        scroll_to_end(&documents[i].lines, &documents[i].visible, viewport_rows);
                    needs_redraw = true;
                }
                // Space = page down
                KeyEvent {
                    code: KeyCode::Char(' '),
                    ..
                } => {
                    documents[i].scroll_offset = advance_lines(
                        &documents[i].visible,
                        documents[i].scroll_offset,
                        viewport_rows as usize,
                    );
                    needs_redraw = true;
                }
                // Horizontal scroll code blocks
                KeyEvent {
                    code: KeyCode::Char('h') | KeyCode::Left,
                    ..
                } => {
                    if let Some(block_id) = first_visible_code_block(
                        &documents[i].lines,
                        &documents[i].visible,
                        documents[i].scroll_offset,
                    ) {
                        if let Some(entry) = documents[i].code_h_scroll.get_mut(block_id) {
                            *entry = entry.saturating_sub(4);
                        }
                        needs_redraw = true;
                    }
                }
                KeyEvent {
                    code: KeyCode::Char('l') | KeyCode::Right,
                    ..
                } => {
                    if let Some(block_id) = first_visible_code_block(
                        &documents[i].lines,
                        &documents[i].visible,
                        documents[i].scroll_offset,
                    ) {
                        let max = documents[i]
                            .output
                            .code_blocks
                            .get(block_id)
                            .map(|b| b.max_width.saturating_sub(term_cols as usize))
                            .unwrap_or(0);
                        if let Some(entry) = documents[i].code_h_scroll.get_mut(block_id) {
                            *entry = (*entry + 4).min(max);
                        }
                        needs_redraw = true;
                    }
                }
                KeyEvent {
                    code: KeyCode::Char('r'),
                    ..
                } => {
                    if let Some(p) = documents[i].path.clone() {
                        documents[i].apply_output(render_fn(&p));
                    }
                    needs_redraw = true;
                }

                // Toggle details or video play/pause
                KeyEvent {
                    code: KeyCode::Enter,
                    ..
                } => {
                    let toggled_video = first_visible_video_controls(
                        &documents[i].lines,
                        &documents[i].visible,
                        documents[i].scroll_offset,
                    );
                    if let Some(gif_id) = toggled_video {
                        if let Some(p) = documents[i].video_paused.get_mut(gif_id) {
                            *p = !*p;
                        }
                        needs_redraw = true;
                    } else if let Some(id) = first_visible_details(
                        &documents[i].lines,
                        &documents[i].visible,
                        documents[i].scroll_offset,
                    ) {
                        if documents[i].collapsed.contains(&id) {
                            documents[i].collapsed.remove(&id);
                        } else {
                            documents[i].collapsed.insert(id);
                        }
                        documents[i].recompute_visible();
                        needs_redraw = true;
                    }
                }

                _ => {}
            }
        }
    }

    crossterm::execute!(
        stdout,
        crossterm::event::DisableMouseCapture,
        cursor::Show,
        terminal::LeaveAlternateScreen,
    )
    .unwrap();
    terminal::disable_raw_mode().unwrap();
}

/// Register a GIF for animation if not already tracked.
fn register_gif(
    id: usize,
    gif_state: &mut [Option<(usize, std::time::Instant)>],
    pending_gifs: &[crate::sixel::PendingGif],
) {
    if let Some(slot) = gif_state.get_mut(id)
        && slot.is_none()
    {
        let delay = pending_gifs
            .get(id)
            .and_then(|g| g.frame(0))
            .map(|f| f.delay_ms)
            .unwrap_or(100);
        *slot = Some((
            0,
            std::time::Instant::now() + Duration::from_millis(delay as u64),
        ));
    }
}

/// Print output directly, resolving pending images synchronously.
pub fn print_output(output: &RenderOutput) {
    let mut sixel_store = Vec::new();
    let lines = flatten_blocks(output, &mut sixel_store);
    // For non-pager output, print sixel for images at row 0 of each group
    let mut printed_groups = std::collections::HashSet::new();
    for line in &lines {
        match line {
            Line::Text(t) => println!("{t}"),
            Line::RichText { segments, .. } => {
                for seg in segments {
                    match seg {
                        RichSegment::Text(t) => print!("{t}"),
                        RichSegment::Image { image_id, .. } => {
                            if let Some(p) = output.pending_images.get(*image_id) {
                                let sixel = p.wait();
                                if !sixel.is_empty() {
                                    print!("{sixel}");
                                }
                            }
                        }
                    }
                }
                println!();
            }
            Line::ImageRow {
                group,
                row_in_group,
                ..
            } => {
                if *row_in_group == 0 && printed_groups.insert(*group) {
                    match group {
                        ImageGroup::Sixel(id) => {
                            if let Some(sd) = sixel_store.get(*id) {
                                println!("{}", sd.data);
                            }
                        }
                        ImageGroup::PendingImage(id) => {
                            if let Some(p) = output.pending_images.get(*id) {
                                let sixel = p.wait();
                                if !sixel.is_empty() {
                                    println!("{sixel}");
                                }
                            }
                        }
                        ImageGroup::Gif(id) => {
                            if let Some(gif) = output.pending_gifs.get(*id) {
                                while !gif.is_done() && gif.frame_count() == 0 {
                                    std::thread::sleep(Duration::from_millis(10));
                                }
                                if let Some(frame) = gif.frame(0) {
                                    println!("{}", frame.sixel);
                                }
                            }
                        }
                    }
                }
                // Skip non-first rows of the group (sixel covers them)
            }
            Line::DetailsSummary { text, .. } => {
                println!("\x1b[1m\u{25BC} {text}\x1b[0m");
            }
            Line::CodeBlock { id, .. } => {
                if let Some(block) = output.code_blocks.get(*id) {
                    for line in &block.lines {
                        println!("  {line}");
                    }
                    println!();
                }
            }
            Line::DetailsStart { .. }
            | Line::DetailsEnd { .. }
            | Line::FootnoteRef { .. }
            | Line::FootnoteDefStart { .. }
            | Line::FootnoteDefEnd => {}
            Line::VideoControls { .. } => {} // no controls in non-pager mode
            Line::TableRow { content } => {
                println!("{content}");
            }
            Line::ImageStrip {
                sixels,
                row_in_group,
                ..
            } => {
                if *row_in_group == 0 {
                    // Emit sixels on first row (they render downward)
                    for s in sixels {
                        match s {
                            PositionedSixel::Static { col, data, .. } => {
                                print!("\x1b[{col}G{data}");
                            }
                            PositionedSixel::Pending { col, image_id, .. } => {
                                if let Some(p) = output.pending_images.get(*image_id) {
                                    let sixel = p.wait();
                                    if !sixel.is_empty() {
                                        print!("\x1b[{col}G{sixel}");
                                    }
                                }
                            }
                            PositionedSixel::Gif { col, gif_id, .. } => {
                                if let Some(gif) = output.pending_gifs.get(*gif_id) {
                                    while !gif.is_done() && gif.frame_count() == 0 {
                                        std::thread::sleep(Duration::from_millis(10));
                                    }
                                    if let Some(frame) = gif.frame(0) {
                                        print!("\x1b[{col}G{}", frame.sixel);
                                    }
                                }
                            }
                        }
                    }
                    println!();
                }
                // Subsequent rows: skip — sixel from row 0 covers them
            }
        }
    }
}

struct DrawResult {
    /// Maps terminal row → details block ID for click handling.
    summary_rows: Vec<(u16, usize)>,
    /// Maps terminal row → gif_id for video control click handling.
    video_control_rows: Vec<(u16, usize)>,
    /// Maps (terminal row, col_start, col_end, label, is_definition) for
    /// navigation.
    footnote_rows: Vec<(u16, u16, u16, String, bool)>,
}

/// Draw the current view.
/// `scroll_offset` indexes into `visible`, which maps to actual line indices.
/// Try to render a full sixel image for an image group. Returns true if
/// rendered.
fn advance_gif_frames(
    gif_state: &mut [Option<(usize, std::time::Instant)>],
    pending_gifs: &[crate::sixel::PendingGif],
    video_paused: &[bool],
) -> bool {
    let now = std::time::Instant::now();
    let mut any_advanced = false;
    for (id, slot) in gif_state.iter_mut().enumerate() {
        let Some((frame_idx, deadline)) = slot else {
            continue;
        };
        if now >= *deadline
            && !video_paused.get(id).copied().unwrap_or(false)
            && let Some(gif) = pending_gifs.get(id)
        {
            let count = gif.frame_count();
            if count > 1 {
                *frame_idx += 1;
                if gif.is_done() {
                    *frame_idx %= count;
                }
                gif.playback_idx
                    .store(*frame_idx, std::sync::atomic::Ordering::Relaxed);
                if let Some(frame) = gif.frame(*frame_idx) {
                    *deadline = now + Duration::from_millis(frame.delay_ms as u64);
                }
                any_advanced = true;
            }
        }
    }
    any_advanced
}

fn check_visible_images_ready(
    lines: &[Line],
    visible: &[usize],
    pending: &[crate::sixel::PendingImage],
    scroll_offset: usize,
    viewport_rows: u16,
) -> bool {
    let has_pending = lines.iter().any(|l| {
        matches!(
            l,
            Line::ImageRow {
                group: ImageGroup::PendingImage(_),
                ..
            }
        )
    });
    if !has_pending {
        return false;
    }
    let mut rows = 0u16;
    for &vi in &visible[scroll_offset..] {
        if rows >= viewport_rows {
            break;
        }
        if let Line::ImageRow {
            group: ImageGroup::PendingImage(id),
            row_in_group: 0,
            ..
        } = &lines[vi]
            && pending.get(*id).is_some_and(|p| p.is_ready())
        {
            return true;
        }
        rows += lines[vi].rows();
    }
    false
}

fn try_render_full_image(
    stdout: &mut io::Stdout,
    group: ImageGroup,
    row: u16,
    sixel_store: &[SixelData],
    pending_images: &[crate::sixel::PendingImage],
    gifs: &[crate::sixel::PendingGif],
    gif_frames: &[usize],
    content_col: u16,
) -> bool {
    crossterm::execute!(stdout, cursor::MoveTo(content_col, row)).unwrap();
    match group {
        ImageGroup::Sixel(id) => {
            if let Some(sd) = sixel_store.get(id) {
                write!(stdout, "{}", sd.data).unwrap();
                return true;
            }
        }
        ImageGroup::PendingImage(id) => {
            if let Some(p) = pending_images.get(id)
                && p.is_ready()
            {
                let sixel = p.wait();
                if !sixel.is_empty() {
                    write!(stdout, "{sixel}").unwrap();
                    return true;
                }
            }
        }
        ImageGroup::Gif(id) => {
            if let Some(gif) = gifs.get(id) {
                let frame_idx = gif_frames.get(id).copied().unwrap_or(0);
                if let Some(frame) = gif.frame(frame_idx) {
                    write!(stdout, "{}", frame.sixel).unwrap();
                    return true;
                }
            }
        }
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn draw_screen(
    stdout: &mut io::Stdout,
    lines: &[Line],
    visible: &[usize],
    collapsed: &HashSet<usize>,
    sixel_store: &[SixelData],
    pending_images: &[crate::sixel::PendingImage],
    gifs: &[crate::sixel::PendingGif],
    gif_frames: &[usize],
    video_paused: &[bool],
    video_progress_pos: &mut [usize],
    code_blocks: &[crate::renderer::CodeBlock],
    code_h_scroll: &[usize],
    scroll_offset: usize,
    viewport_rows: u16,
    term_cols: u16,
    term_rows: u16,
    watching: bool,
    content_col: u16,
) -> DrawResult {
    crossterm::execute!(
        stdout,
        terminal::Clear(ClearType::All),
        cursor::MoveTo(content_col, 0),
    )
    .unwrap();

    let mut rows_used: u16 = 0;
    let mut vis_idx = scroll_offset;
    let mut summary_rows: Vec<(u16, usize)> = Vec::new();
    let mut video_control_rows: Vec<(u16, usize)> = Vec::new();
    let mut footnote_rows: Vec<(u16, u16, u16, String, bool)> = Vec::new();
    // Pending footnote refs: (label, col_start) — associated with the next text row
    let mut pending_footnote_refs: Vec<(String, usize)> = Vec::new();
    let mut pending_footnote_def: Option<String> = None;

    while vis_idx < visible.len() && rows_used < viewport_rows {
        let line_idx = visible[vis_idx];
        match &lines[line_idx] {
            Line::Text(text) => {
                crossterm::execute!(stdout, cursor::MoveTo(content_col, rows_used)).unwrap();
                write!(stdout, "{text}\r").unwrap();
                // Associate pending footnote markers with this row
                for (label, col_start) in pending_footnote_refs.drain(..) {
                    // [N] is typically 3-5 chars wide
                    let col_end = col_start + 5;
                    footnote_rows.push((rows_used, col_start as u16, col_end as u16, label, false));
                }
                if let Some(label) = pending_footnote_def.take() {
                    // Definition [N] starts at column 0
                    footnote_rows.push((rows_used, 0, 10, label, true));
                }
                rows_used += 1;
            }
            Line::RichText { segments, height } => {
                crossterm::execute!(stdout, cursor::MoveTo(content_col, rows_used)).unwrap();
                // First pass: write text segments, leave gaps for images
                let mut image_positions: Vec<(u16, usize)> = Vec::new();
                let mut col: u16 = 0;
                for seg in segments {
                    match seg {
                        RichSegment::Text(text) => {
                            write!(stdout, "{text}").unwrap();
                            col += crate::renderer::ansi::visible_len(text) as u16;
                        }
                        RichSegment::Image {
                            image_id,
                            width_cols,
                        } => {
                            image_positions.push((col, *image_id));
                            // Write spaces as placeholder
                            let gap = " ".repeat(*width_cols as usize);
                            write!(stdout, "{gap}").unwrap();
                            col += width_cols;
                        }
                    }
                }
                write!(stdout, "\r").unwrap();
                // Second pass: overlay sixel images at their positions
                for (img_col, image_id) in &image_positions {
                    if let Some(p) = pending_images.get(*image_id)
                        && p.is_ready()
                    {
                        let sixel = p.wait();
                        if !sixel.is_empty() {
                            crossterm::execute!(
                                stdout,
                                cursor::MoveTo(content_col + *img_col, rows_used)
                            )
                            .unwrap();
                            write!(stdout, "{sixel}").unwrap();
                        }
                    }
                }
                rows_used += height;
            }
            Line::ImageRow {
                group,
                row_in_group,
                total_rows,
                preview_text,
            } => {
                // Check if this is the first row AND all rows of the
                // group fit — if so, render the full sixel/gif.
                let render_full = *row_in_group == 0
                    && rows_used + total_rows <= viewport_rows
                    && try_render_full_image(
                        stdout,
                        *group,
                        rows_used,
                        sixel_store,
                        pending_images,
                        gifs,
                        gif_frames,
                        content_col,
                    );

                if render_full {
                    // Skip the remaining ImageRows of this group
                    rows_used += total_rows;
                    vis_idx += 1;
                    // Advance past remaining rows of this group
                    while vis_idx < visible.len() {
                        let next = visible[vis_idx];
                        if let Line::ImageRow { group: g, .. } = &lines[next]
                            && g == group
                        {
                            vis_idx += 1;
                            continue;
                        }
                        break;
                    }
                    continue; // skip the vis_idx += 1 at bottom
                } else {
                    // Render one preview row
                    crossterm::execute!(stdout, cursor::MoveTo(content_col, rows_used)).unwrap();
                    if preview_text.is_empty() {
                        write!(stdout, "\x1b[2m  [loading...]\x1b[0m\r").unwrap();
                    } else {
                        write!(stdout, "{preview_text}\x1b[0m\r").unwrap();
                    }
                    rows_used += 1;
                }
            }
            Line::TableRow { content } => {
                crossterm::execute!(stdout, cursor::MoveTo(content_col, rows_used)).unwrap();
                write!(stdout, "{content}\r").unwrap();
                rows_used += 1;
            }
            Line::ImageStrip {
                content,
                sixels,
                row_in_group,
                total_rows,
                video_controls,
            } => {
                // Check if this is the start of an image group and all rows fit
                let render_sixels = !sixels.is_empty()
                    && *row_in_group == 0
                    && *total_rows > 0
                    && rows_used + total_rows <= viewport_rows;

                if render_sixels {
                    // Write content for all rows of this group first
                    let start_row = rows_used;
                    crossterm::execute!(stdout, cursor::MoveTo(content_col, rows_used)).unwrap();
                    write!(stdout, "{content}\r").unwrap();
                    rows_used += 1;
                    vis_idx += 1;
                    let mut remaining = *total_rows - 1;
                    while remaining > 0 && vis_idx < visible.len() && rows_used < viewport_rows {
                        let next = visible[vis_idx];
                        if let Line::ImageStrip { content: c, .. } = &lines[next] {
                            crossterm::execute!(stdout, cursor::MoveTo(content_col, rows_used))
                                .unwrap();
                            write!(stdout, "{c}\r").unwrap();
                            rows_used += 1;
                            vis_idx += 1;
                            remaining -= 1;
                        } else {
                            break;
                        }
                    }
                    // Clear image areas before sixel overlay
                    for s in sixels.iter() {
                        let (col, w) = match s {
                            PositionedSixel::Static { col, width, .. }
                            | PositionedSixel::Pending { col, width, .. }
                            | PositionedSixel::Gif { col, width, .. } => (*col, *width),
                        };
                        let blank: String = " ".repeat(w as usize);
                        for r in start_row..rows_used {
                            crossterm::execute!(stdout, cursor::MoveTo(content_col + col, r))
                                .unwrap();
                            write!(stdout, "{blank}").unwrap();
                        }
                    }
                    // Overlay sixels at start row (they render downward)
                    for s in sixels {
                        match s {
                            PositionedSixel::Static { col, data, .. } => {
                                crossterm::execute!(
                                    stdout,
                                    cursor::MoveTo(content_col + *col, start_row)
                                )
                                .unwrap();
                                write!(stdout, "{data}").unwrap();
                            }
                            PositionedSixel::Pending { col, image_id, .. } => {
                                if let Some(p) = pending_images.get(*image_id)
                                    && p.is_ready()
                                {
                                    let sixel = p.wait();
                                    if !sixel.is_empty() {
                                        crossterm::execute!(
                                            stdout,
                                            cursor::MoveTo(content_col + *col, start_row)
                                        )
                                        .unwrap();
                                        write!(stdout, "{sixel}").unwrap();
                                    }
                                }
                            }
                            PositionedSixel::Gif { col, gif_id, .. } => {
                                let frame_idx = gif_frames.get(*gif_id).copied().unwrap_or(0);
                                if let Some(gif) = gifs.get(*gif_id)
                                    && let Some(frame) = gif.frame(frame_idx)
                                {
                                    crossterm::execute!(
                                        stdout,
                                        cursor::MoveTo(content_col + *col, start_row)
                                    )
                                    .unwrap();
                                    write!(stdout, "{}", frame.sixel).unwrap();
                                }
                            }
                        }
                    }
                    continue; // skip vis_idx += 1 at bottom
                }

                // No sixel overlay — just render content (preview shows)
                crossterm::execute!(stdout, cursor::MoveTo(content_col, rows_used)).unwrap();
                write!(stdout, "{content}\r").unwrap();
                for &(col, w, gif_id) in video_controls {
                    let controls = format_video_controls(
                        w as usize,
                        gif_id,
                        gifs,
                        gif_frames,
                        video_paused,
                        video_progress_pos,
                    );
                    crossterm::execute!(stdout, cursor::MoveTo(content_col + col, rows_used))
                        .unwrap();
                    write!(stdout, "{controls}").unwrap();
                    video_control_rows.push((rows_used, gif_id));
                }
                rows_used += 1;
            }
            Line::DetailsStart { .. } | Line::DetailsEnd { .. } => {}
            Line::FootnoteRef { label, col } => {
                pending_footnote_refs.push((label.clone(), *col));
            }
            Line::FootnoteDefStart { label } => {
                pending_footnote_def = Some(label.clone());
            }
            Line::FootnoteDefEnd => {}
            Line::DetailsSummary { id, text } => {
                let is_collapsed = collapsed.contains(id);
                let triangle = if is_collapsed { "\u{25B6}" } else { "\u{25BC}" };
                crossterm::execute!(stdout, cursor::MoveTo(content_col, rows_used)).unwrap();
                write!(stdout, "\x1b[1m{triangle} {text}\x1b[0m\r").unwrap();
                summary_rows.push((rows_used, *id));
                rows_used += 1;
            }
            Line::CodeBlock { id, height } => {
                if let Some(block) = code_blocks.get(*id) {
                    let h_offset = code_h_scroll.get(*id).copied().unwrap_or(0);
                    let avail_cols = term_cols.saturating_sub(content_col) as usize;

                    // Render each code line with horizontal scroll
                    for line in &block.lines {
                        if rows_used >= viewport_rows {
                            break;
                        }
                        crossterm::execute!(stdout, cursor::MoveTo(content_col, rows_used))
                            .unwrap();
                        let sliced =
                            crate::renderer::ansi::visible_slice(line, h_offset, avail_cols);
                        write!(stdout, "  {sliced}\x1b[0m\r").unwrap();
                        rows_used += 1;
                    }

                    // Scrollbar row
                    if rows_used < viewport_rows {
                        crossterm::execute!(stdout, cursor::MoveTo(content_col, rows_used))
                            .unwrap();
                        if block.max_width > avail_cols {
                            let bar = render_scrollbar(h_offset, block.max_width, avail_cols);
                            write!(stdout, "\x1b[2m{bar}\x1b[0m\r").unwrap();
                        }
                        rows_used += 1;
                    }
                } else {
                    rows_used += height;
                }
            }
            Line::VideoControls { gif_id } => {
                crossterm::execute!(stdout, cursor::MoveTo(content_col, rows_used)).unwrap();
                let video_cols = gifs
                    .get(*gif_id)
                    .and_then(|g| g.preview.first())
                    .map(|p| crate::renderer::ansi::visible_len(p))
                    .unwrap_or(term_cols as usize);
                let controls = format_video_controls(
                    video_cols,
                    *gif_id,
                    gifs,
                    gif_frames,
                    video_paused,
                    video_progress_pos,
                );
                write!(stdout, " {controls} \r").unwrap();
                video_control_rows.push((rows_used, *gif_id));
                rows_used += 1;
            }
        }
        vis_idx += 1;
    }

    // Status bar
    let progress = if visible.is_empty() {
        100
    } else {
        (vis_idx * 100) / visible.len()
    };
    let watch_indicator = if watching { " [watching]" } else { "" };
    let cols = term_cols.saturating_sub(content_col) as usize;
    let status = format!(
        " [{progress}%]{watch_indicator} j/k:scroll  h/l:code  d/u:half  g/G:top/end  \
         enter:toggle  r:reload  q:quit"
    );
    // Truncate to terminal width to prevent wrapping
    let status: String = status.chars().take(cols).collect();
    crossterm::execute!(stdout, cursor::MoveTo(content_col, term_rows - 1)).unwrap();
    write!(stdout, "\x1b[7m{status:<width$}\x1b[0m", width = cols).unwrap();

    DrawResult {
        summary_rows,
        video_control_rows,
        footnote_rows,
    }
}

/// Find the visible index of a footnote's counterpart (ref→def or def→ref).
fn find_footnote_target(
    lines: &[Line],
    visible: &[usize],
    label: &str,
    find_def: bool,
) -> Option<usize> {
    for (vi, &li) in visible.iter().enumerate() {
        match &lines[li] {
            Line::FootnoteDefStart { label: l } if find_def && l == label => return Some(vi),
            Line::FootnoteRef { label: l, .. } if !find_def && l == label => return Some(vi),
            _ => {}
        }
    }
    None
}

/// Find the details block to toggle.
///
/// First checks if `scroll_offset` is inside an expanded details block
/// (i.e. past its summary but before its end) by scanning backward.
/// If so, returns that block's ID. Otherwise returns the first
/// `DetailsSummary` visible from `scroll_offset` onward.
fn first_visible_details(
    lines: &[Line],
    visible: &[usize],
    scroll_offset: usize,
) -> Option<usize> {
    // Check if we're inside a details block by scanning backward for an
    // unmatched DetailsStart/DetailsSummary before the current position.
    let current_line_idx = visible.get(scroll_offset).copied().unwrap_or(0);
    let mut depth: Vec<usize> = Vec::new();
    for &idx in visible.iter().take(scroll_offset + 1) {
        match &lines[idx] {
            Line::DetailsSummary { id, .. } => {
                depth.push(*id);
            }
            Line::DetailsEnd { id } => {
                depth.retain(|d| d != id);
            }
            _ => {}
        }
    }
    // If we're inside a block (depth is non-empty), return the innermost one
    if let Some(&id) = depth.last() {
        // Only if we're actually past the summary (not ON it)
        let on_summary =
            matches!(&lines[current_line_idx], Line::DetailsSummary { id: sid, .. } if *sid == id);
        if !on_summary {
            return Some(id);
        }
    }

    // Otherwise, find the first summary at or after scroll_offset
    for &idx in &visible[scroll_offset..] {
        if let Line::DetailsSummary { id, .. } = &lines[idx] {
            return Some(*id);
        }
    }
    None
}

/// Format a video progress bar: "▶  ───●──────" or "⏸  ───●──────".
/// `width` is the total available columns for the controls.
/// `last_pos` tracks the high-water mark to prevent jitter during loading.
fn format_video_controls(
    width: usize,
    gif_id: usize,
    gifs: &[crate::sixel::PendingGif],
    gif_frames: &[usize],
    video_paused: &[bool],
    last_pos: &mut [usize],
) -> String {
    let paused = video_paused.get(gif_id).copied().unwrap_or(false);
    let btn = if paused { "\u{25B6}" } else { "\u{23F8}" };
    let frame_idx = gif_frames.get(gif_id).copied().unwrap_or(0);
    let gif = gifs.get(gif_id);
    let cycle = gif.map(|g| g.cycle_length()).unwrap_or(0);
    let done = gif.is_some_and(|g| g.is_done());
    // btn(1) + 2 spaces = 3 chars overhead
    let bar_width = width.saturating_sub(3);
    let mut progress_pos = if cycle > 1 && bar_width > 0 {
        let pos_in_cycle = frame_idx % cycle;
        (pos_in_cycle * bar_width / cycle).min(bar_width - 1)
    } else {
        0
    };

    // While still loading, only allow forward movement to prevent jitter
    // from cycle_length() estimate changing as frames are added.
    // Once done, allow free movement (looping resets to 0).
    if !done && let Some(prev) = last_pos.get_mut(gif_id) {
        if progress_pos < *prev {
            progress_pos = *prev;
        }
        *prev = progress_pos;
    }

    let mut bar = String::new();
    for i in 0..bar_width {
        if i == progress_pos {
            bar.push('\u{25CF}');
        } else {
            bar.push('\u{2500}');
        }
    }
    format!("\x1b[2m{btn}  {bar}\x1b[0m")
}

/// Find which code block (if any) occupies the given terminal row.
/// Find the first visible video control bar from scroll_offset.
fn first_visible_video_controls(
    lines: &[Line],
    visible: &[usize],
    scroll_offset: usize,
) -> Option<usize> {
    for &vi in &visible[scroll_offset..] {
        if let Line::VideoControls { gif_id } = &lines[vi] {
            return Some(*gif_id);
        }
    }
    None
}

/// Find the first visible code block from scroll_offset.
fn first_visible_code_block(
    lines: &[Line],
    visible: &[usize],
    scroll_offset: usize,
) -> Option<usize> {
    for &vi in &visible[scroll_offset..] {
        if let Line::CodeBlock { id, .. } = &lines[vi] {
            return Some(*id);
        }
    }
    None
}

fn find_code_block_at_row(
    lines: &[Line],
    visible: &[usize],
    _code_blocks: &[crate::renderer::CodeBlock],
    scroll_offset: usize,
    viewport_rows: u16,
    target_row: u16,
) -> Option<usize> {
    let mut rows_used: u16 = 0;
    for &vi in &visible[scroll_offset..] {
        if rows_used >= viewport_rows {
            break;
        }
        let line = &lines[vi];
        let line_rows = line.rows();
        if let Line::CodeBlock { id, .. } = line
            && target_row >= rows_used
            && target_row < rows_used + line_rows
        {
            return Some(*id);
        }
        rows_used += line_rows;
    }
    None
}

fn render_scrollbar(
    offset: usize,
    content_width: usize,
    view_width: usize,
) -> String {
    if content_width <= view_width {
        return String::new();
    }
    let bar_width = view_width.saturating_sub(2); // leave space for arrows
    if bar_width == 0 {
        return String::new();
    }

    // Thumb size proportional to visible portion
    let thumb_size = ((view_width as f64 / content_width as f64) * bar_width as f64)
        .ceil()
        .max(1.0) as usize;
    let max_offset = content_width.saturating_sub(view_width);
    let thumb_pos = if max_offset > 0 {
        ((offset as f64 / max_offset as f64) * (bar_width - thumb_size) as f64) as usize
    } else {
        0
    };

    let mut bar = String::with_capacity(view_width);
    bar.push('\u{25C0}'); // ◀
    for i in 0..bar_width {
        if i >= thumb_pos && i < thumb_pos + thumb_size {
            bar.push('\u{2588}'); // █
        } else {
            bar.push('\u{2500}'); // ─
        }
    }
    bar.push('\u{25B6}'); // ▶
    bar
}

fn advance_lines(
    visible: &[usize],
    offset: usize,
    count: usize,
) -> usize {
    (offset + count).min(visible.len().saturating_sub(1))
}

fn retreat_lines(
    offset: usize,
    count: usize,
) -> usize {
    offset.saturating_sub(count)
}

fn scroll_to_end(
    lines: &[Line],
    visible: &[usize],
    viewport_rows: u16,
) -> usize {
    let mut rows: u16 = 0;
    for (i, &line_idx) in visible.iter().enumerate().rev() {
        rows += lines[line_idx].rows();
        if rows > viewport_rows {
            return i + 1;
        }
    }
    0
}
