mod font;
mod highlight;
mod math;
mod pager;
mod renderer;
mod sixel;
mod theme;

use std::io::IsTerminal;
use std::io::Read;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use clap::Parser;

/// Render markdown with sixel graphics for headings and inline images.
#[derive(Parser)]
#[command(name = "pretty-down", version)]
struct Args {
    /// Markdown file to render (reads stdin if omitted)
    file: Option<PathBuf>,

    /// Path to a TTF/OTF font for heading rendering
    #[arg(short, long)]
    font: Option<PathBuf>,

    /// Path to a theme JSON file
    #[arg(short, long)]
    theme: Option<PathBuf>,

    /// Syntax highlighting theme name or .tmTheme file path
    #[arg(long)]
    syntax_theme: Option<String>,

    /// Print output directly without the pager
    #[arg(long)]
    no_pager: bool,

    /// Watch the file for changes and reload automatically
    #[arg(short, long)]
    watch: bool,
}

/// Fairfax font embedded in the binary (OFL licensed).
const EMBEDDED_FONT: &[u8] = include_bytes!("../fonts/Fairfax.ttf");

/// Find a heading font: user-specified → system serif → embedded Fairfax.
fn load_font(user_font: Option<&PathBuf>) -> Vec<u8> {
    // 1. User-specified font takes priority
    if let Some(p) = user_font {
        match std::fs::read(p) {
            Ok(data) => return data,
            Err(e) => {
                eprintln!("warning: cannot read font {}: {e}", p.display());
            }
        }
    }

    // 2. Try to find a system serif font via fontdb
    let mut db = fontdb::Database::new();
    db.load_system_fonts();

    let serif_query = fontdb::Query {
        families: &[fontdb::Family::Serif],
        ..fontdb::Query::default()
    };

    if let Some(id) = db.query(&serif_query) {
        let mut data = Vec::new();
        db.with_face_data(id, |bytes, _| {
            data = bytes.to_vec();
        });
        if !data.is_empty() {
            return data;
        }
    }

    // 3. Embedded Fairfax as last resort
    EMBEDDED_FONT.to_vec()
}

fn main() -> Result<()> {
    let args = Args::parse();

    let font_data = load_font(args.font.as_ref());
    let font = font::Font::new(&font_data).context("failed to parse font file")?;

    let theme = match &args.theme {
        Some(p) => theme::Theme::from_file(p)
            .with_context(|| format!("loading theme from {}", p.display()))?,
        None => theme::Theme::default(),
    };

    let mut highlighter = highlight::Highlighter::new();
    if let Some(ref st) = args.syntax_theme {
        let path = std::path::Path::new(st);
        if path.exists() {
            highlighter.load_theme_file(path)?;
        } else {
            highlighter.set_theme(st);
        }
    }

    let (markdown, base_path) = match &args.file {
        Some(p) => {
            let md = std::fs::read_to_string(p)
                .with_context(|| format!("cannot read {}", p.display()))?;
            let base = p.parent().map(|p| p.to_path_buf());
            (md, base)
        }
        None => {
            anyhow::ensure!(!args.watch, "--watch requires a file argument");
            let mut md = String::new();
            std::io::stdin()
                .read_to_string(&mut md)
                .context("cannot read stdin")?;
            (md, None)
        }
    };

    let term_width = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80);
    let output = renderer::render(
        &markdown,
        &font,
        base_path.as_deref(),
        &theme,
        &highlighter,
        term_width,
    );

    if args.no_pager || !std::io::stdout().is_terminal() {
        for p in &output.pending_images {
            p.wait();
        }
        pager::print_output(&output);
    } else {
        let watch_path = if args.watch {
            args.file.as_deref()
        } else {
            None
        };

        let file_name = args
            .file
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned());

        let render_fn = {
            let theme = theme.clone();
            move |path: &std::path::Path, content_width: u16| {
                let md = std::fs::read_to_string(path).unwrap_or_default();
                let base = path.parent().map(|p| p.to_path_buf());
                renderer::render(
                    &md,
                    &font,
                    base.as_deref(),
                    &theme,
                    &highlighter,
                    content_width,
                )
            }
        };

        pager::run(
            output,
            file_name.as_deref(),
            args.file.as_deref(),
            watch_path,
            &render_fn,
            &theme,
        );
    }

    Ok(())
}
