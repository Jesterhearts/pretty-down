# pretty-down

> **Fair warning:** This was vibe-coded for my own personal use. It works for me,
> but your mileage may vary.
> 
> I only test on linux as that is my current environment. If anyone is aware of similar
> projects/prior art, please let me know so I can link them here. The goal of this was to have
> in-terminal pretty rendering of markdown files which goes beyond syntax highlighting and is closer
> to what you would get rendering with a browser or with e.g. vscode's markdown preview.

A CLI tool that renders markdown in the terminal using [Sixel](https://en.wikipedia.org/wiki/Sixel)
graphics. Headings are rendered as actual rasterized text with larger fonts, and
images are displayed inline — all without leaving your terminal.

## What it does

- **Headings (h1–h6)** — Rendered as Sixel images with scaled font sizes using
  the embedded [Fairfax](https://www.kreativekorp.com/software/fonts/fairfax/)
  font (or a custom font via `--font`)
- **Bold, italic, strikethrough** — ANSI terminal escape codes
- **Links** — Clickable via [OSC 8](https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cb5feda)
  hyperlinks (in supported terminals)
- **Images** — Loaded from local paths and displayed inline as Sixel
- **Code blocks, lists, blockquotes, horizontal rules** — ANSI styling

HTML content is not supported.

## Requirements

- A Sixel-capable terminal (e.g. WezTerm, foot, mlterm, xterm with `-ti vt340`)
- Rust 2024 edition (1.85+)

## Usage

```
pretty-down [OPTIONS] [FILE]
```

Reads from stdin if no file is given.

```sh
# Render a file
pretty-down README.md

# Pipe from stdin
cat notes.md | pretty-down

# Use a custom font for headings
pretty-down --font /path/to/font.ttf document.md
```

## Building

```sh
cargo build --release
```

## Dependencies

- [pulldown-cmark](https://crates.io/crates/pulldown-cmark) — Markdown parsing
- [a-sixel](https://crates.io/crates/a-sixel) — Sixel encoding
- [rustybuzz](https://crates.io/crates/rustybuzz) — Text shaping
- [raqote](https://crates.io/crates/raqote) — 2D rasterization

## License

The Fairfax font is included under the [SIL Open Font License](fonts/FairfaxOFL.txt).
