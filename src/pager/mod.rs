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
use crate::renderer::rewrap_output;

mod flatten;
mod line;

use line::ImageGroup;
use line::Line;
use line::PositionedSixel;
use line::RichSegment;
use line::SixelData;

use flatten::flatten_blocks;
use flatten::visible_indices;

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
}

/// Recompute visible indices from current lines and collapsed state.
fn recompute_visible(doc: &mut DocumentState) {
    doc.visible = visible_indices(&doc.lines, &doc.collapsed);
    if doc.scroll_offset >= doc.visible.len() {
        doc.scroll_offset = doc.visible.len().saturating_sub(1);
    }
}

/// Re-wrap text at a new width without re-encoding images.
fn rewrap_document(
    doc: &mut DocumentState,
    width: u16,
    theme: &crate::theme::Theme,
) {
    rewrap_output(&mut doc.output, width, theme);
    doc.lines = flatten_blocks(&doc.output, &mut doc.sixel_store);
    doc.visible = visible_indices(&doc.lines, &doc.collapsed);
    if doc.scroll_offset >= doc.visible.len() {
        doc.scroll_offset = doc.visible.len().saturating_sub(1);
    }
}

/// Re-render from a new output, preserving collapsed state.
fn apply_output(
    doc: &mut DocumentState,
    new_output: RenderOutput,
) {
    doc.lines = flatten_blocks(&new_output, &mut doc.sixel_store);
    doc.visible = visible_indices(&doc.lines, &doc.collapsed);
    doc.gif_state = vec![None; new_output.pending_gifs.len()];
    doc.video_paused = vec![false; new_output.pending_gifs.len()];
    doc.video_progress_pos = vec![0; new_output.pending_gifs.len()];
    doc.code_h_scroll = vec![0; new_output.code_blocks.len()];
    if doc.scroll_offset >= doc.visible.len() {
        doc.scroll_offset = doc.visible.len().saturating_sub(1);
    }
    doc.images_ready_count = new_output
        .pending_images
        .iter()
        .filter(|p| p.is_ready())
        .count();
    doc.output = new_output;
}

/// Re-flatten from the existing output (e.g. when a pending image finishes).
fn reflatten(doc: &mut DocumentState) {
    doc.lines = flatten_blocks(&doc.output, &mut doc.sixel_store);
    doc.visible = visible_indices(&doc.lines, &doc.collapsed);
    if doc.scroll_offset >= doc.visible.len() {
        doc.scroll_offset = doc.visible.len().saturating_sub(1);
    }
    doc.images_ready_count = doc
        .output
        .pending_images
        .iter()
        .filter(|p| p.is_ready())
        .count();
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
        recompute_candidates(&mut s);
        s
    }
}

/// Recompute file dialog candidates from the current input.
fn recompute_candidates(dialog: &mut FileDialogState) {
    dialog.candidates.clear();
    dialog.selected = 0;

    let expanded = expand_path(&dialog.input);

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
    dialog.candidates.extend(dirs);
    dialog.candidates.extend(files);
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
) -> io::Result<()> {
    let w = sidebar.width();

    match sidebar.mode {
        SidebarMode::Collapsed => {
            crossterm::execute!(stdout, cursor::MoveTo(0, 0))?;
            write!(stdout, "\x1b[1m❰\x1b[0m ")?;
            crossterm::execute!(stdout, cursor::MoveTo(0, 1))?;
            write!(stdout, "\x1b[2m─\x1b[0m ")?;
            for row in 2..viewport_rows {
                crossterm::execute!(stdout, cursor::MoveTo(0, row))?;
                write!(stdout, "\x1b[2m│\x1b[0m ")?;
            }
        }
        SidebarMode::Open | SidebarMode::FileDialog => {
            let content_w = w.saturating_sub(1) as usize;

            crossterm::execute!(stdout, cursor::MoveTo(0, 0))?;
            let header = format!(
                " \x1b[1m▸\x1b[0m {:<width$}",
                "Documents",
                width = content_w.saturating_sub(3)
            );
            write!(stdout, "{}", &header[..header.len().min((w as usize) * 4)])?;

            crossterm::execute!(stdout, cursor::MoveTo(0, 1))?;
            let open_btn = format!(" \x1b[2m[+] Open...\x1b[0m");
            write!(stdout, "{open_btn}")?;
            let btn_vis = crate::renderer::ansi::visible_len(&open_btn);
            write!(stdout, "{}", " ".repeat(content_w.saturating_sub(btn_vis)))?;

            for (i, &name) in doc_names.iter().enumerate() {
                let row = i as u16 + 2;
                if row >= viewport_rows {
                    break;
                }
                crossterm::execute!(stdout, cursor::MoveTo(0, row))?;

                let max_name = content_w.saturating_sub(2);
                let display: String = if name.len() > max_name && max_name > 2 {
                    format!("{}…", &name[..max_name - 1])
                } else {
                    name.to_string()
                };

                if i == sidebar.active_doc {
                    write!(stdout, " \x1b[4m{display}\x1b[0m")?;
                } else {
                    write!(stdout, " \x1b[2m{display}\x1b[0m")?;
                }
                let name_len = display.len() + 1;
                write!(stdout, "{}", " ".repeat(content_w.saturating_sub(name_len)))?;
            }

            let doc_end = doc_names.len() as u16 + 2;
            for row in doc_end..viewport_rows {
                crossterm::execute!(stdout, cursor::MoveTo(0, row))?;
                write!(stdout, "{}", " ".repeat(content_w))?;
            }

            for row in 0..viewport_rows {
                crossterm::execute!(stdout, cursor::MoveTo(w - 1, row))?;
                write!(stdout, "\x1b[2m│\x1b[0m")?;
            }

            if sidebar.mode == SidebarMode::FileDialog {
                draw_file_dialog(stdout, sidebar, viewport_rows)?;
            }
        }
    }
    Ok(())
}

/// Draw the file-open dialog as an overlay.
fn draw_file_dialog(
    stdout: &mut io::Stdout,
    sidebar: &SidebarState,
    viewport_rows: u16,
) -> io::Result<()> {
    let dialog = &sidebar.dialog;
    let dialog_width = sidebar.open_width.max(40) as usize;

    crossterm::execute!(stdout, cursor::MoveTo(1, 0))?;
    write!(stdout, "\x1b[7m Open file: \x1b[0m")?;

    crossterm::execute!(stdout, cursor::MoveTo(1, 1))?;
    let input_display: String = if dialog.input.len() > dialog_width - 3 {
        let start = dialog.input.len() - (dialog_width - 3);
        format!("…{}", &dialog.input[start..])
    } else {
        dialog.input.clone()
    };
    write!(stdout, " {input_display}\x1b[7m \x1b[0m")?;
    write!(
        stdout,
        "{}",
        " ".repeat(dialog_width.saturating_sub(input_display.len() + 2))
    )?;

    let max_candidates = (viewport_rows as usize).saturating_sub(3);
    for (i, candidate) in dialog.candidates.iter().enumerate().take(max_candidates) {
        let row = i as u16 + 2;
        crossterm::execute!(stdout, cursor::MoveTo(1, row))?;

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
            )?;
        } else {
            write!(stdout, " {truncated:<width$}", width = dialog_width - 2)?;
        }
    }
    Ok(())
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
    render_fn: &dyn Fn(&Path, u16) -> RenderOutput,
    theme: &crate::theme::Theme,
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

    let has_pending = documents[sidebar.active_doc].lines.iter().any(|l| {
        matches!(
            l,
            Line::ImageRow {
                group: ImageGroup::PendingImage(_),
                ..
            }
        )
    });

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
        wikilink_rows: Vec::new(),
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
                reflatten(&mut documents[i]);
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
                let _ = draw_sidebar(&mut stdout, &sidebar, &names, viewport_rows);
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
                apply_output(
                    &mut documents[i],
                    render_fn(&p, term_cols.saturating_sub(sidebar.width())),
                );
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
                apply_output(
                    &mut documents[i],
                    render_fn(&p, term_cols.saturating_sub(sidebar.width())),
                );
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
                                recompute_candidates(&mut sidebar.dialog);
                            } else {
                                // Open the file
                                let new_output =
                                    render_fn(&selected, term_cols.saturating_sub(sidebar.width()));
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
                            recompute_candidates(&mut sidebar.dialog);
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
                            recompute_candidates(&mut sidebar.dialog);
                        }
                        needs_redraw = true;
                    }
                    KeyCode::Char(c) => {
                        sidebar.dialog.input.push(c);
                        sidebar.dialog.cursor = sidebar.dialog.input.len();
                        recompute_candidates(&mut sidebar.dialog);
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
                        let cw = term_cols.saturating_sub(sidebar.width());
                        rewrap_document(&mut documents[i], cw, theme);
                        needs_redraw = true;
                        continue;
                    }
                    SidebarMode::Open => {
                        if row == 0 {
                            sidebar.mode = SidebarMode::Collapsed;
                            let cw = term_cols.saturating_sub(sidebar.width());
                            rewrap_document(&mut documents[i], cw, theme);
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
            recompute_visible(&mut documents[i]);
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

        // Handle click on wikilink — open linked document
        if let Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row,
            column,
            ..
        }) = ev
        {
            if let Some((_, _, _, target)) = last_draw
                .wikilink_rows
                .iter()
                .find(|(r, c0, c1, _)| *r == row && column >= *c0 && column < *c1)
            {
                // Resolve wikilink target relative to the current document's directory
                let target_path = resolve_wikilink(target, &documents[i].path);
                if let Some(path) = target_path {
                    if path.exists() {
                        let cw = term_cols.saturating_sub(sidebar.width());
                        let new_output = render_fn(&path, cw);
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "untitled".into());
                        documents.push(DocumentState::new(name, Some(path), new_output));
                        sidebar.active_doc = documents.len() - 1;
                        if sidebar.mode == SidebarMode::Collapsed {
                            sidebar.mode = SidebarMode::Open;
                        }
                        needs_redraw = true;
                        continue;
                    }
                }
            }
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
                    // Re-wrap text at new content width (cheap, no image re-encoding)
                    let cw = term_cols.saturating_sub(sidebar.width());
                    rewrap_document(&mut documents[i], cw, theme);
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
                        apply_output(
                            &mut documents[i],
                            render_fn(&p, term_cols.saturating_sub(sidebar.width())),
                        );
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
                        recompute_visible(&mut documents[i]);
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

/// Resolve a wikilink target to a filesystem path.
///
/// Wikilinks like `[[notes]]` resolve to `notes.md` (or `notes.markdown`)
/// relative to the current document's directory.
fn resolve_wikilink(
    target: &str,
    current_doc_path: &Option<PathBuf>,
) -> Option<PathBuf> {
    let base_dir = current_doc_path
        .as_ref()
        .and_then(|p| p.parent())
        .unwrap_or(Path::new("."));

    // Try exact path first (e.g. [[subdir/file.md]])
    let exact = base_dir.join(target);
    if exact.exists() {
        return Some(exact);
    }

    // Try adding common markdown extensions
    for ext in &["md", "markdown", "mdx"] {
        let with_ext = base_dir.join(format!("{target}.{ext}"));
        if with_ext.exists() {
            return Some(with_ext);
        }
    }

    None
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
            | Line::FootnoteDefEnd
            | Line::WikilinkRef { .. } => {}
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
    /// footnote navigation.
    footnote_rows: Vec<(u16, u16, u16, String, bool)>,
    /// Maps (terminal row, col_start, col_end, target) for wikilink clicks.
    wikilink_rows: Vec<(u16, u16, u16, String)>,
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
    let mut pending_wikilinks: Vec<(String, usize)> = Vec::new();
    let mut wikilink_rows: Vec<(u16, u16, u16, String)> = Vec::new();

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
                    footnote_rows.push((rows_used, 0, 10, label, true));
                }
                // Associate pending wikilink markers with this row
                for (target, col_start) in pending_wikilinks.drain(..) {
                    // Use the full line width from col_start to end as clickable
                    // region — the link text extends from col_start and we don't
                    // know its exact width, so be generous.
                    let line_width = crate::renderer::ansi::visible_len(text);
                    let col_end = line_width;
                    wikilink_rows.push((rows_used, col_start as u16, col_end as u16, target));
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
            Line::WikilinkRef { target, col } => {
                pending_wikilinks.push((target.clone(), *col));
            }
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
        wikilink_rows,
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
