mod font;
mod pager;
mod renderer;
mod sixel;

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

    /// Print output directly without the pager
    #[arg(long)]
    no_pager: bool,
}

/// Fairfax font embedded in the binary (OFL licensed).
const EMBEDDED_FONT: &[u8] = include_bytes!("../fonts/Fairfax.ttf");

fn main() {
    let args = Args::parse();

    // Load font: use --font if specified, otherwise the embedded Fairfax
    let font_data_owned;
    let font_data: &[u8] = match &args.font {
        Some(p) => {
            font_data_owned = std::fs::read(p).unwrap_or_else(|e| {
                eprintln!("error: cannot read font {}: {e}", p.display());
                std::process::exit(1);
            });
            &font_data_owned
        }
        None => EMBEDDED_FONT,
    };

    let font = font::Font::new(font_data).unwrap_or_else(|| {
        eprintln!("error: failed to parse font file");
        std::process::exit(1);
    });

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

    let output = renderer::render(&markdown, &font, base_path.as_deref());

    if args.no_pager || !std::io::stdout().is_terminal() {
        print!("{output}");
    } else {
        pager::run(&output);
    }
}
