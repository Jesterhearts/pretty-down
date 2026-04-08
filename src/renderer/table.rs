//! Table data structures, rendering, and layout.

use pulldown_cmark::Alignment;

use super::OutputBlock;
use super::RenderState;
use super::ansi;
use super::flush_text;
use crate::sixel;

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
    /// Visible width of the cell content (for text: max line width; for images:
    /// image cols).
    pub width: usize,
    /// Number of terminal rows this cell occupies.
    pub height: usize,
}

/// Image data embedded in a table cell.
pub(super) struct TableCellImage {
    /// Static sixel data (None if animated GIF).
    pub sixel: Option<String>,
    /// Animated GIF ID (index into pending_gifs), if this is a GIF.
    pub gif_id: Option<usize>,
    /// Half-block preview lines (fallback while scrolling / non-pager).
    pub preview: Vec<String>,
    pub width_cols: u32,
    pub height_rows: u16,
}

/// A table cell — either text with styling, or an image.
pub(super) struct TableCell {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub image: Option<TableCellImage>,
}

/// State for table parsing — created on Start(Table), consumed on flush.
pub(super) struct TableState {
    pub alignments: Vec<Alignment>,
    pub in_head: bool,
    pub cell_buf: String,
    pub cell_image: Option<TableCellImage>,
    pub row_cells: Vec<TableCell>,
    pub header: Option<Vec<TableCell>>,
    pub rows: Vec<Vec<TableCell>>,
    pub cell_bold: bool,
    pub cell_italic: bool,
    pub cell_code: bool,
}

/// Render a table with Unicode box-drawing borders.
pub(super) fn flush_table(
    state: &mut RenderState,
    out: &mut String,
    blocks: &mut Vec<OutputBlock>,
) {
    let table = state
        .table
        .take()
        .expect("flush_table called outside table");
    let header_cells = table.header;
    let row_cells = table.rows;
    let alignments = table.alignments;

    let build_rendered_row = |cells: &[TableCell], is_header: bool| -> Vec<RenderedTableCell> {
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
                    let lines: Vec<String> = styled.split('\n').map(|s| s.to_string()).collect();
                    let width = lines
                        .iter()
                        .map(|l| ansi::visible_len(l))
                        .max()
                        .unwrap_or(0);
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
    let rows: Vec<Vec<RenderedTableCell>> = row_cells
        .iter()
        .map(|r| build_rendered_row(r, false))
        .collect();

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

    flush_text(out, blocks, &mut state.pending_wikilinks);
    blocks.push(OutputBlock::Table(RenderedTable {
        col_widths,
        alignments,
        header,
        rows,
    }));
}

fn styled_cell_text(
    cell: &TableCell,
    is_header: bool,
) -> String {
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

/// Render an image into a table cell (synchronous encoding with height snapping).
pub(super) fn render_image_in_table_cell(
    path: &std::path::Path,
    state: &mut RenderState,
) {
    let max_cols: u32 = 30;
    let max_px = max_cols * sixel::cell_pixel_width();

    // Try animated encoding (video/GIF)
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
