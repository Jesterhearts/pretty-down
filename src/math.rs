//! LaTeX math rendering via mitex (LaTeX→Typst) + typst (Typst→SVG).

use typst::Library;
use typst::LibraryExt;
use typst::diag::FileResult;
use typst::foundations::Bytes;
use typst::layout::PagedDocument;
use typst::syntax::FileId;
use typst::syntax::Source;
use typst::text::Font;
use typst::text::FontBook;
use typst::utils::LazyHash;

/// Minimal typst World implementation for rendering math expressions.
struct MathWorld {
    library: LazyHash<Library>,
    book: LazyHash<FontBook>,
    source: Source,
    fonts: Vec<Font>,
}

impl MathWorld {
    fn new(typst_source: &str) -> Self {
        let fonts: Vec<Font> = typst_assets::fonts()
            .flat_map(|data| Font::iter(Bytes::new(data.to_vec())))
            .collect();

        let mut book = FontBook::new();
        for font in &fonts {
            book.push(font.info().clone());
        }

        Self {
            library: LazyHash::new(Library::default()),
            book: LazyHash::new(book),
            source: Source::detached(typst_source),
            fonts,
        }
    }
}

impl typst::World for MathWorld {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &self.book
    }

    fn main(&self) -> FileId {
        self.source.id()
    }

    fn source(
        &self,
        id: FileId,
    ) -> FileResult<Source> {
        if id == self.source.id() {
            Ok(self.source.clone())
        } else {
            Err(typst::diag::FileError::NotFound(std::path::PathBuf::from(
                "<unknown>",
            )))
        }
    }

    fn file(
        &self,
        _id: FileId,
    ) -> FileResult<Bytes> {
        Err(typst::diag::FileError::NotFound(std::path::PathBuf::from(
            "<unknown>",
        )))
    }

    fn font(
        &self,
        index: usize,
    ) -> Option<Font> {
        self.fonts.get(index).cloned()
    }

    fn today(
        &self,
        _offset: Option<i64>,
    ) -> Option<typst::foundations::Datetime> {
        None
    }
}

/// Render a LaTeX math expression to SVG.
/// `fg` is the text color as [r, g, b]. Background is transparent.
/// Returns None if conversion or rendering fails.
pub fn render_latex_to_svg(
    latex: &str,
    display: bool,
    fg: [u8; 3],
) -> Option<String> {
    // Convert LaTeX → Typst math markup
    let typst_math = mitex::convert_math(latex, None).ok()?;

    let margin = if display { 5 } else { 2 };
    let source = format!(
        "#set page(width: auto, height: auto, margin: {margin}pt, fill: none)\n#set text(fill: \
         rgb({}, {}, {}))\n$ {typst_math} $",
        fg[0], fg[1], fg[2]
    );

    // Compile
    let world = MathWorld::new(&source);
    let document: PagedDocument = typst::compile(&world).output.ok()?;

    // Render first page to SVG
    let page = document.pages.first()?;
    Some(typst_svg::svg(page))
}
