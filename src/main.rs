mod font;
mod highlight;
mod pager;
mod renderer;
mod sixel;
mod theme;

use std::io::IsTerminal;
use std::io::Read;
use std::path::PathBuf;

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

fn main() {
    let args = Args::parse();

    let font_data = load_font(args.font.as_ref());
    let font = font::Font::new(&font_data).unwrap_or_else(|| {
        eprintln!("error: failed to parse font file");
        std::process::exit(1);
    });

    // Load theme
    let theme = match &args.theme {
        Some(p) => theme::Theme::from_file(p).unwrap_or_else(|e| {
            eprintln!("error: {e}");
            std::process::exit(1);
        }),
        None => theme::Theme::default(),
    };

    // Set up syntax highlighter
    let mut highlighter = highlight::Highlighter::new();
    if let Some(ref st) = args.syntax_theme {
        let path = std::path::Path::new(st);
        if path.exists() {
            highlighter.load_theme_file(path).unwrap_or_else(|e| {
                eprintln!("error: {e}");
                std::process::exit(1);
            });
        } else {
            highlighter.set_theme(st);
        }
    }

    // Read markdown
    let (markdown, base_path) = match &args.file {
        Some(p) => {
            let md = std::fs::read_to_string(p).unwrap_or_else(|e| {
                eprintln!("error: cannot read {}: {e}", p.display());
                std::process::exit(1);
            });
            let base = p.parent().map(|p| p.to_path_buf());
            (md, base)
        }
        None => {
            if args.watch {
                eprintln!("error: --watch requires a file argument");
                std::process::exit(1);
            }
            let mut md = String::new();
            std::io::stdin()
                .read_to_string(&mut md)
                .unwrap_or_else(|e| {
                    eprintln!("error: cannot read stdin: {e}");
                    std::process::exit(1);
                });
            (md, None)
        }
    };

    let output = renderer::render(&markdown, &font, base_path.as_deref(), &theme, &highlighter);

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

        let render_fn: Box<dyn Fn() -> renderer::RenderOutput> = if let Some(file) = &args.file {
            let file = file.clone();
            let base = base_path.clone();
            let theme = theme.clone();
            Box::new(move || {
                let md = std::fs::read_to_string(&file).unwrap_or_default();
                renderer::render(&md, &font, base.as_deref(), &theme, &highlighter)
            })
        } else {
            Box::new(|| renderer::RenderOutput {
                blocks: Vec::new(),
                pending_images: Vec::new(),
                pending_gifs: Vec::new(),
                code_blocks: Vec::new(),
            })
        };

        pager::run(&output, watch_path, &render_fn);
    }
}
