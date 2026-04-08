//! Display line types for the pager.

/// Identifies an image group (for sixel replacement of preview rows).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum ImageGroup {
    Sixel(usize),        // index into a vec of sixel data
    PendingImage(usize), // index into pending_images
    Gif(usize),          // index into pending_gifs
}

/// Sixel data for an image inside a table cell or side-by-side group.
#[derive(Clone)]
pub(super) enum PositionedSixel {
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
pub(super) enum RichSegment {
    Text(String),
    Image { image_id: usize, width_cols: u16 },
}

/// A display line.
#[derive(Clone)]
pub(super) enum Line {
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
    /// Invisible marker: a wikilink at this column in the text.
    WikilinkRef { target: String, col: usize },
}

impl Line {
    /// Number of terminal rows this line occupies.
    pub(super) fn rows(&self) -> u16 {
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
            | Line::FootnoteDefEnd
            | Line::WikilinkRef { .. } => 0,
        }
    }
}

/// Sixel data stored separately, referenced by ImageGroup.
pub(super) struct SixelData {
    pub data: String,
    #[allow(dead_code)]
    pub height: u16,
}
