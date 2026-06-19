use syntect::easy::HighlightLines;
use syntect::highlighting::Theme;
use syntect::parsing::SyntaxSet;
use std::path::Path;
use two_face::theme::EmbeddedThemeName;

/// One styled run on a line: (byte length, packed 0xRRGGBB color).
pub type Run = (usize, u32);

pub struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

impl Highlighter {
    pub fn new() -> Self {
        // two-face bundles bat's curated syntaxes — TypeScript, TSX, and ~150 more
        let syntaxes = two_face::syntax::extra_no_newlines();
        // One Dark palette (Zed default) to match the UI theme
        let theme = two_face::theme::extra().get(EmbeddedThemeName::TwoDark).clone();
        Self { syntaxes, theme }
    }

    /// Highlight a whole file. Returns one Vec<Run> per line, parallel to the
    /// file's lines. Run byte-lengths sum to each line's byte length.
    pub fn highlight(&self, text: &str, path: &Path) -> Vec<Vec<Run>> {
        let syntax = path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(|ext| self.syntaxes.find_syntax_by_extension(ext))
            .or_else(|| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| self.syntaxes.find_syntax_by_extension(n))
            })
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text());

        let mut hl = HighlightLines::new(syntax, &self.theme);
        let mut out = Vec::new();

        for line in text.split('\n') {
            let runs = match hl.highlight_line(line, &self.syntaxes) {
                Ok(ranges) => ranges
                    .iter()
                    .map(|(style, piece)| {
                        let c = style.foreground;
                        let rgb = ((c.r as u32) << 16) | ((c.g as u32) << 8) | (c.b as u32);
                        (piece.len(), rgb)
                    })
                    .collect(),
                Err(_) => Vec::new(),
            };
            out.push(runs);
        }

        out
    }
}
