use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;

/// Cached syntax highlighting state.
pub struct Highlighter {
    syntax_set: SyntaxSet,
    theme_set: ThemeSet,
    theme_name: String,
}

impl Highlighter {
    /// Create a new highlighter with the default syntax and theme sets.
    pub fn new() -> Self {
        Self {
            syntax_set: SyntaxSet::load_defaults_newlines(),
            theme_set: ThemeSet::load_defaults(),
            theme_name: "base16-ocean.dark".to_string(),
        }
    }

    /// Set the syntect theme by name (e.g. "base16-ocean.dark", "Solarized
    /// (dark)").
    pub fn set_theme(
        &mut self,
        name: &str,
    ) {
        if self.theme_set.themes.contains_key(name) {
            self.theme_name = name.to_string();
        }
    }

    /// Load a custom .tmTheme file.
    pub fn load_theme_file(
        &mut self,
        path: &std::path::Path,
    ) -> Result<(), String> {
        let theme = ThemeSet::get_theme(path).map_err(|e| format!("cannot load theme: {e}"))?;
        self.theme_name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "custom".into());
        self.theme_set.themes.insert(self.theme_name.clone(), theme);
        Ok(())
    }

    /// Highlight a code block and return ANSI-colored text.
    ///
    /// `lang` is the language identifier (e.g. "rust", "python", "js").
    /// If the language is unknown, returns `None` (caller should fall back
    /// to unhighlighted rendering).
    pub fn highlight(
        &self,
        code: &str,
        lang: &str,
    ) -> Option<String> {
        use syntect::easy::HighlightLines;
        use syntect::util::LinesWithEndings;

        let syntax = self
            .syntax_set
            .find_syntax_by_token(lang)
            .or_else(|| self.syntax_set.find_syntax_by_extension(lang))?;

        let theme = self.theme_set.themes.get(&self.theme_name)?;
        let mut h = HighlightLines::new(syntax, theme);

        let mut result = String::new();

        // Apply background color from theme if present
        if let Some(bg) = theme.settings.background {
            result.push_str(&format!("\x1b[48;2;{};{};{}m", bg.r, bg.g, bg.b));
        }

        for line in LinesWithEndings::from(code) {
            let regions = h.highlight_line(line, &self.syntax_set).ok()?;
            for (style, text) in regions {
                let fg = style.foreground;
                result.push_str(&format!("\x1b[38;2;{};{};{}m", fg.r, fg.g, fg.b));
                result.push_str(text);
            }
        }

        result.push_str("\x1b[0m");
        Some(result)
    }
}
