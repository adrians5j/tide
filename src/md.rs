//! A very small markdown reader for agent replies — enough to make a thread
//! readable, not a spec-complete parser. The agent writes prose with `code`,
//! **emphasis**, bullets, and the occasional fenced block; that's the whole
//! target. Parsing lives here so it can be tested without a window.

/// An inline run of text inside a line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Span {
    Text(String),
    Code(String),
    Bold(String),
    Italic(String),
}

/// One rendered line: its bullet (if any), how deep it's nested, and its spans.
/// Lines inside a fenced block carry `code: true` and a single `Text` span, so
/// the renderer can lay them out monospaced and unwrapped — plus `lang`, the
/// fence's info string, so it can be syntax-highlighted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub indent: usize,
    pub bullet: Option<String>,
    pub spans: Vec<Span>,
    pub code: bool,
    /// 1-6 for `#`..`######`, 0 for ordinary text. A chip-sized renderer can
    /// ignore it and just embolden; a document view sizes by it.
    pub heading: u8,
    /// The opening fence's language tag (`ts`, `rust`, …); empty when untagged.
    /// Only meaningful on `code` lines.
    pub lang: String,
    /// A pipe-table row's cells, already split and span-parsed. `None` for every
    /// other line. The `|---|---|` separator is consumed, not emitted.
    pub cells: Option<Vec<Vec<Span>>>,
    /// True on a table row the separator marked as the header.
    pub header: bool,
}

/// Split a line's text into inline spans. Unclosed markers stay literal — a
/// stray `*` is far more common in prose than a formatting mistake.
pub fn spans(text: &str) -> Vec<Span> {
    let b: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut plain = String::new();
    let mut i = 0;
    let flush = |plain: &mut String, out: &mut Vec<Span>| {
        if !plain.is_empty() {
            out.push(Span::Text(std::mem::take(plain)));
        }
    };
    // find the next occurrence of `close` starting at `from`
    let find = |from: usize, close: &[char]| -> Option<usize> {
        (from..b.len()).find(|&j| b[j..].starts_with(close))
    };
    while i < b.len() {
        let rest = &b[i..];
        if rest.starts_with(&['`']) {
            if let Some(end) = find(i + 1, &['`']) {
                flush(&mut plain, &mut out);
                out.push(Span::Code(b[i + 1..end].iter().collect()));
                i = end + 1;
                continue;
            }
        }
        if rest.starts_with(&['*', '*']) {
            if let Some(end) = find(i + 2, &['*', '*']) {
                flush(&mut plain, &mut out);
                out.push(Span::Bold(b[i + 2..end].iter().collect()));
                i = end + 2;
                continue;
            }
        }
        // single * or _ around a run with no space right after the marker, so
        // "a * b" and snake_case_names aren't mistaken for emphasis
        if (rest.starts_with(&['*']) || rest.starts_with(&['_']))
            && b.get(i + 1).is_some_and(|c| !c.is_whitespace())
        {
            let m = b[i];
            let word_char_before = i > 0 && (b[i - 1].is_alphanumeric() || b[i - 1] == '_');
            if !(m == '_' && word_char_before) {
                if let Some(end) = find(i + 1, &[m]) {
                    if end > i + 1 && !b[end - 1].is_whitespace() {
                        flush(&mut plain, &mut out);
                        out.push(Span::Italic(b[i + 1..end].iter().collect()));
                        i = end + 1;
                        continue;
                    }
                }
            }
        }
        plain.push(b[i]);
        i += 1;
    }
    if !plain.is_empty() {
        out.push(Span::Text(plain));
    }
    out
}

/// Parse a reply into renderable lines. Blank lines survive as empty ones, so
/// paragraph breaks the agent wrote are still visible.
pub fn parse(text: &str) -> Vec<Line> {
    let mut out = Vec::new();
    let mut fenced = false;
    let mut lang = String::new();
    for raw in text.split('\n') {
        let trimmed = raw.trim_start();
        if let Some(info) = trimmed.strip_prefix("```") {
            // the opening fence names the language; the closing one clears it
            lang = if fenced { String::new() } else { info.trim().to_string() };
            fenced = !fenced;
            continue; // the fence itself isn't content
        }
        if fenced {
            out.push(Line {
                indent: 0,
                bullet: None,
                spans: vec![Span::Text(raw.to_string())],
                code: true,
                heading: 0,
                lang: lang.clone(),
                cells: None,
                header: false,
            });
            continue;
        }
        // pipe table: `| a | b |`, with `|---|---|` marking the row above as the
        // header. Rendered as a grid, so the raw pipes never reach the screen.
        if let Some(cols) = table_row(trimmed) {
            if is_separator(&cols) {
                if let Some(prev) = out.last_mut().filter(|l| l.cells.is_some()) {
                    prev.header = true;
                }
                continue;
            }
            out.push(Line {
                indent: 0,
                bullet: None,
                spans: Vec::new(),
                code: false,
                heading: 0,
                lang: String::new(),
                cells: Some(cols.iter().map(|c| spans(c)).collect()),
                header: false,
            });
            continue;
        }
        let indent = (raw.len() - trimmed.len()) / 2;
        // "- x", "* x", "1. x" → a bullet line; the marker is rendered, not parsed
        let (bullet, body) = if let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
            (Some("•".to_string()), rest)
        } else if let Some((n, rest)) = numbered(trimmed) {
            (Some(n), rest)
        } else if let Some((level, rest)) = heading(trimmed) {
            // spans still parse inside a heading (`code` in a title is common)
            out.push(Line {
                indent: 0,
                bullet: None,
                spans: spans(rest),
                code: false,
                heading: level,
                lang: String::new(),
                cells: None,
                header: false,
            });
            continue;
        } else {
            (None, trimmed)
        };
        out.push(Line {
            indent,
            bullet,
            spans: spans(body),
            code: false,
            heading: 0,
            lang: String::new(),
            cells: None,
            header: false,
        });
    }
    out
}

/// `| a | b |` → the trimmed cell texts. `None` when the line isn't a table row
/// (it needs a leading pipe and at least two of them, so prose containing a
/// single `|` is left alone).
fn table_row(line: &str) -> Option<Vec<String>> {
    let inner = line.strip_prefix('|')?;
    if !inner.contains('|') {
        return None;
    }
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    Some(inner.split('|').map(|c| c.trim().to_string()).collect())
}

/// `---`, `:---`, `---:`, `:---:` in every cell — markdown's header separator.
fn is_separator(cols: &[String]) -> bool {
    !cols.is_empty()
        && cols.iter().all(|c| {
            let c = c.trim_start_matches(':').trim_end_matches(':');
            c.len() >= 3 && c.chars().all(|ch| ch == '-')
        })
}

/// `### Title` → `(3, "Title")`. Six is the deepest level markdown defines.
fn heading(line: &str) -> Option<(u8, &str)> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    Some((hashes as u8, line[hashes..].strip_prefix(' ')?))
}

/// `12. rest` → `("12.", "rest")`.
fn numbered(line: &str) -> Option<(String, &str)> {
    let digits: String = line.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 3 {
        return None;
    }
    let rest = line[digits.len()..].strip_prefix(". ")?;
    Some((format!("{digits}."), rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_code_and_emphasis_become_spans() {
        assert_eq!(
            spans("set `originReadTimeout: 60` and **note** it"),
            vec![
                Span::Text("set ".into()),
                Span::Code("originReadTimeout: 60".into()),
                Span::Text(" and ".into()),
                Span::Bold("note".into()),
                Span::Text(" it".into()),
            ]
        );
    }

    /// Prose is full of lone markers and snake_case; treating those as emphasis
    /// would mangle far more replies than it would prettify.
    #[test]
    fn unpaired_markers_and_snake_case_stay_literal() {
        assert_eq!(spans("2 * 3 = 6"), vec![Span::Text("2 * 3 = 6".into())]);
        assert_eq!(spans("a `b"), vec![Span::Text("a `b".into())]);
        assert_eq!(spans("read_server_events"), vec![Span::Text("read_server_events".into())]);
    }

    #[test]
    fn bullets_and_numbers_carry_their_marker() {
        let lines = parse("- first\n2. second\nplain");
        assert_eq!(lines[0].bullet.as_deref(), Some("•"));
        assert_eq!(lines[0].spans, vec![Span::Text("first".into())]);
        assert_eq!(lines[1].bullet.as_deref(), Some("2."));
        assert_eq!(lines[2].bullet, None);
    }

    #[test]
    fn a_fenced_block_keeps_its_lines_verbatim() {
        let lines = parse("before\n```ts\nconst a = *b*;\n```\nafter");
        assert_eq!(lines.len(), 3);
        assert!(lines[1].code);
        // the info string rides along so the renderer can highlight it
        assert_eq!(lines[1].lang, "ts");
        // no inline parsing inside the fence
        assert_eq!(lines[1].spans, vec![Span::Text("const a = *b*;".into())]);
        assert!(!lines[2].code);
    }

    /// A table is the one construct where raw markdown is unreadable on screen,
    /// so the pipes have to become a grid.
    #[test]
    fn a_pipe_table_becomes_cells_with_a_header() {
        let lines = parse("| Rule | Shows |\n|---|---|\n| `a` | subtree |\nafter");
        assert_eq!(lines.len(), 3);
        let head = lines[0].cells.as_ref().expect("header cells");
        assert_eq!(head.len(), 2);
        assert!(lines[0].header, "the separator marks the row above");
        let row = lines[1].cells.as_ref().expect("body cells");
        assert_eq!(row[0], vec![Span::Code("a".into())]);
        assert!(!lines[1].header);
        // the line after the table is ordinary prose again
        assert!(lines[2].cells.is_none());
    }

    /// Prose with one pipe in it isn't a table.
    #[test]
    fn a_stray_pipe_is_not_a_table() {
        assert!(parse("a | b").pop().unwrap().cells.is_none());
        assert!(parse("| just one").pop().unwrap().cells.is_none());
    }

    #[test]
    fn headings_carry_their_level() {
        let l = parse("## Why `this`").pop().unwrap();
        assert_eq!(l.heading, 2);
        assert_eq!(l.spans, vec![Span::Text("Why ".into()), Span::Code("this".into())]);
        // a bare # or a 7-deep run isn't a heading
        assert_eq!(parse("####### deep").pop().unwrap().heading, 0);
        assert_eq!(parse("#nope").pop().unwrap().heading, 0);
    }
}
