//! Syntax highlighting for the center viewer's file preview, backed by tree-sitter.
//!
//! Grammars are compiled into the binary as C parsers, so highlighting stays fast (milliseconds)
//! even in debug builds and on large files — unlike the regex-driven highlighters whose per-line
//! matching cost seconds there. Each language's highlight configuration (compiled queries) is
//! built once on first use and cached process-wide; the caller caps the line count and the text
//! is sliced to that cap *before* parsing, so a huge file only ever costs what is shown.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use tree_sitter::Language;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

use crate::display::color::Rgb;
use crate::git_worker::{DiffLine, DiffLineKind};

/// One styled fragment of a rendered line.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub text: String,
    pub fg: Rgb,
    pub bold: bool,
    pub italic: bool,
}

impl Span {
    pub fn new(text: impl Into<String>, fg: Rgb) -> Self {
        Self { text: text.into(), fg, bold: false, italic: false }
    }
}

/// One line of highlighted text: styled spans in display order, tabs already expanded.
pub type SpanLine = Vec<Span>;

/// Fallback foreground for unhighlighted text, matching the chrome's primary foreground.
fn plain_fg() -> Rgb {
    Rgb::new(0xfa, 0xfa, 0xfa)
}

/// Recognized highlight capture names and their colors (Tokyo Night-ish, chosen to read on the
/// near-black viewer surface). Query captures resolve to the longest dot-separated prefix in
/// this list (e.g. `function.method` → `function`); unlisted captures (plain variables) keep the
/// default foreground.
const THEME: &[(&str, Rgb)] = &[
    ("attribute", Rgb::new(0xe0, 0xaf, 0x68)),
    ("comment", Rgb::new(0x6b, 0x74, 0x99)),
    ("constant", Rgb::new(0xff, 0x9e, 0x64)),
    ("constructor", Rgb::new(0x2a, 0xc3, 0xde)),
    ("escape", Rgb::new(0x89, 0xdd, 0xff)),
    ("function", Rgb::new(0x7a, 0xa2, 0xf7)),
    ("keyword", Rgb::new(0xbb, 0x9a, 0xf7)),
    ("label", Rgb::new(0x7a, 0xa2, 0xf7)),
    ("module", Rgb::new(0x2a, 0xc3, 0xde)),
    ("number", Rgb::new(0xff, 0x9e, 0x64)),
    ("operator", Rgb::new(0x89, 0xdd, 0xff)),
    ("property", Rgb::new(0x73, 0xda, 0xca)),
    ("punctuation", Rgb::new(0x9a, 0xa5, 0xce)),
    ("string", Rgb::new(0x9e, 0xce, 0x6a)),
    ("tag", Rgb::new(0xf7, 0x76, 0x8e)),
    ("text.literal", Rgb::new(0x9e, 0xce, 0x6a)),
    ("text.reference", Rgb::new(0x73, 0xda, 0xca)),
    ("text.title", Rgb::new(0x7a, 0xa2, 0xf7)),
    ("text.uri", Rgb::new(0x89, 0xdd, 0xff)),
    ("type", Rgb::new(0x2a, 0xc3, 0xde)),
    ("variable.builtin", Rgb::new(0xf7, 0x76, 0x8e)),
];

/// A language with a bundled grammar.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Lang {
    Rust,
    C,
    Cpp,
    Go,
    Python,
    Js,
    Ts,
    Tsx,
    Json,
    Toml,
    Yaml,
    Html,
    Css,
    Bash,
    Markdown,
}

/// Pick the grammar for `path` (by extension, special filenames, then a shebang sniff).
fn lang_for(path: &str, text: &str) -> Option<Lang> {
    let file = Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or(path);
    if file.eq_ignore_ascii_case("cargo.lock") {
        return Some(Lang::Toml);
    }

    let ext = Path::new(file).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    if let Some(ext) = ext.as_deref() {
        return match ext {
            "rs" => Some(Lang::Rust),
            "c" | "h" => Some(Lang::C),
            "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Some(Lang::Cpp),
            "go" => Some(Lang::Go),
            "py" | "pyi" => Some(Lang::Python),
            "js" | "mjs" | "cjs" | "jsx" => Some(Lang::Js),
            "ts" | "mts" | "cts" => Some(Lang::Ts),
            "tsx" => Some(Lang::Tsx),
            "json" | "jsonc" => Some(Lang::Json),
            "toml" => Some(Lang::Toml),
            "yaml" | "yml" => Some(Lang::Yaml),
            "html" | "htm" => Some(Lang::Html),
            "css" => Some(Lang::Css),
            "sh" | "bash" | "zsh" => Some(Lang::Bash),
            "md" | "markdown" => Some(Lang::Markdown),
            _ => None,
        };
    }

    // No extension: sniff the shebang.
    let first = text.lines().next().unwrap_or("");
    if first.starts_with("#!") {
        if first.contains("python") {
            return Some(Lang::Python);
        }
        if first.contains("sh") {
            return Some(Lang::Bash);
        }
    }
    None
}

/// Compile `lang`'s highlight configuration. Grammars whose highlight queries extend another
/// (C++ → C, TS/TSX → JS) get the base grammar's query appended, with the more specific
/// patterns first so they take precedence.
fn build_config(lang: Lang) -> Option<HighlightConfiguration> {
    let (language, name, highlights): (Language, &str, String) = match lang {
        Lang::Rust => (
            tree_sitter_rust::LANGUAGE.into(),
            "rust",
            tree_sitter_rust::HIGHLIGHTS_QUERY.into(),
        ),
        Lang::C => (tree_sitter_c::LANGUAGE.into(), "c", tree_sitter_c::HIGHLIGHT_QUERY.into()),
        Lang::Cpp => (
            tree_sitter_cpp::LANGUAGE.into(),
            "cpp",
            format!("{}\n{}", tree_sitter_cpp::HIGHLIGHT_QUERY, tree_sitter_c::HIGHLIGHT_QUERY),
        ),
        Lang::Go => {
            (tree_sitter_go::LANGUAGE.into(), "go", tree_sitter_go::HIGHLIGHTS_QUERY.into())
        },
        Lang::Python => (
            tree_sitter_python::LANGUAGE.into(),
            "python",
            tree_sitter_python::HIGHLIGHTS_QUERY.into(),
        ),
        Lang::Js => (
            tree_sitter_javascript::LANGUAGE.into(),
            "javascript",
            format!(
                "{}\n{}",
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                tree_sitter_javascript::HIGHLIGHT_QUERY
            ),
        ),
        Lang::Ts => (
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            "typescript",
            format!(
                "{}\n{}",
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
                tree_sitter_javascript::HIGHLIGHT_QUERY
            ),
        ),
        Lang::Tsx => (
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            "tsx",
            format!(
                "{}\n{}\n{}",
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                tree_sitter_javascript::HIGHLIGHT_QUERY
            ),
        ),
        Lang::Json => {
            (tree_sitter_json::LANGUAGE.into(), "json", tree_sitter_json::HIGHLIGHTS_QUERY.into())
        },
        Lang::Toml => (
            tree_sitter_toml_ng::LANGUAGE.into(),
            "toml",
            tree_sitter_toml_ng::HIGHLIGHTS_QUERY.into(),
        ),
        Lang::Yaml => {
            (tree_sitter_yaml::LANGUAGE.into(), "yaml", tree_sitter_yaml::HIGHLIGHTS_QUERY.into())
        },
        Lang::Html => {
            (tree_sitter_html::LANGUAGE.into(), "html", tree_sitter_html::HIGHLIGHTS_QUERY.into())
        },
        Lang::Css => {
            (tree_sitter_css::LANGUAGE.into(), "css", tree_sitter_css::HIGHLIGHTS_QUERY.into())
        },
        Lang::Bash => {
            (tree_sitter_bash::LANGUAGE.into(), "bash", tree_sitter_bash::HIGHLIGHT_QUERY.into())
        },
        Lang::Markdown => (
            tree_sitter_md::LANGUAGE.into(),
            "markdown",
            tree_sitter_md::HIGHLIGHT_QUERY_BLOCK.into(),
        ),
    };

    let mut config = HighlightConfiguration::new(language, name, &highlights, "", "").ok()?;
    let names: Vec<&str> = THEME.iter().map(|(name, _)| *name).collect();
    config.configure(&names);
    Some(config)
}

/// The cached highlight configuration for `lang`, built (and leaked — one per language per
/// process) on first use. Failed builds are cached too, so a broken query doesn't retry per
/// click.
fn config_for(lang: Lang) -> Option<&'static HighlightConfiguration> {
    static CONFIGS: OnceLock<Mutex<HashMap<Lang, Option<&'static HighlightConfiguration>>>> =
        OnceLock::new();
    let mut configs = CONFIGS.get_or_init(Mutex::default).lock().unwrap();
    *configs
        .entry(lang)
        .or_insert_with(|| build_config(lang).map(|config| &*Box::leak(Box::new(config))))
}

/// Clean a chunk for the chrome's glyph emitter: strip the line ending and expand tabs (which
/// would otherwise render zero-width).
fn clean(chunk: &str) -> String {
    chunk.trim_end_matches(['\n', '\r']).replace('\t', "    ")
}

/// One full-bright span per line, for files without a known grammar.
fn plain_lines(text: &str) -> Vec<SpanLine> {
    text.lines().map(|line| vec![Span::new(clean(line), plain_fg())]).collect()
}

/// Run the tree-sitter highlighter over `src`, splitting the event stream into per-line spans.
/// `None` (any parse/query error) falls back to plain text.
fn highlight_with(config: &HighlightConfiguration, src: &str) -> Option<Vec<SpanLine>> {
    let mut highlighter = Highlighter::new();
    let events = highlighter.highlight(config, src.as_bytes(), None, |_| None).ok()?;

    let mut lines: Vec<SpanLine> = Vec::new();
    let mut current: SpanLine = Vec::new();
    // Innermost active capture wins; an empty stack renders with the default foreground.
    let mut stack: Vec<usize> = Vec::new();
    for event in events {
        match event.ok()? {
            HighlightEvent::HighlightStart(highlight) => stack.push(highlight.0),
            HighlightEvent::HighlightEnd => {
                stack.pop();
            },
            HighlightEvent::Source { start, end } => {
                let color = stack.last().map_or_else(plain_fg, |&i| THEME[i].1);
                for (i, part) in src[start..end].split('\n').enumerate() {
                    if i > 0 {
                        lines.push(std::mem::take(&mut current));
                    }
                    let part = clean(part);
                    if !part.is_empty() {
                        current.push(Span::new(part, color));
                    }
                }
            },
        }
    }
    lines.push(current);
    // A trailing newline otherwise yields one phantom empty line at the end.
    if src.ends_with('\n') && lines.last().is_some_and(Vec::is_empty) {
        lines.pop();
    }
    Some(lines)
}

/// Slice `text` to at most `max_lines` lines, returning whether anything was cut off.
pub fn cap_lines(text: &str, max_lines: usize) -> (&str, bool) {
    match text.match_indices('\n').nth(max_lines.saturating_sub(1)) {
        Some((offset, _)) if offset + 1 < text.len() => (&text[..offset], true),
        _ => (text, false),
    }
}

/// Highlight `text` as the language inferred from `path`, returning up to `max_lines` lines of
/// colored spans plus whether the text was truncated. Unknown file types come back as single
/// full-bright spans.
pub fn highlight_file(path: &str, text: &str, max_lines: usize) -> (Vec<SpanLine>, bool) {
    // Slice to the line cap before parsing, so a huge file only costs what is shown.
    let (src, truncated) = cap_lines(text, max_lines);

    let lines = lang_for(path, src)
        .and_then(config_for)
        .and_then(|config| highlight_with(config, src))
        .unwrap_or_else(|| plain_lines(src));
    (lines, truncated)
}

/// Syntax-highlight the body column of a parsed unified diff in one pass, storing per-line
/// colored spans on each entry (markers stripped). All bodies are joined into one pseudo-source
/// — deletions and additions sit next to each other, which tree-sitter's error tolerance absorbs
/// — so the resulting lines stay aligned with the diff rows. Hunk headers get no spans.
pub fn highlight_diff(path: &str, lines: &mut [DiffLine]) {
    let bodies: Vec<&str> = lines
        .iter()
        .map(|line| match line.kind {
            DiffLineKind::Hunk => "",
            _ => line.text.get(1..).unwrap_or(""),
        })
        .collect();
    let joined = bodies.join("\n");

    let highlighted = lang_for(path, &joined)
        .and_then(config_for)
        .and_then(|config| highlight_with(config, &joined));
    let span_lines: Vec<SpanLine> = highlighted.unwrap_or_else(|| {
        bodies
            .iter()
            .map(|body| {
                if body.is_empty() {
                    Vec::new()
                } else {
                    vec![Span::new(clean(body), plain_fg())]
                }
            })
            .collect()
    });

    for (i, line) in lines.iter_mut().enumerate() {
        if line.kind != DiffLineKind::Hunk {
            line.spans = span_lines.get(i).cloned().unwrap_or_default();
        }
    }
}

/// Highlight a fenced code block's contents as the language named by the fence info `token`
/// (e.g. ```` ```rust ````); unknown tokens come back as plain spans.
pub fn highlight_fenced(token: &str, code: &str) -> Vec<SpanLine> {
    let lang = match token.trim().to_ascii_lowercase().as_str() {
        "rust" | "rs" => Some(Lang::Rust),
        "c" => Some(Lang::C),
        "cpp" | "c++" => Some(Lang::Cpp),
        "go" | "golang" => Some(Lang::Go),
        "python" | "py" => Some(Lang::Python),
        "javascript" | "js" | "jsx" => Some(Lang::Js),
        "typescript" | "ts" => Some(Lang::Ts),
        "tsx" => Some(Lang::Tsx),
        "json" | "jsonc" => Some(Lang::Json),
        "toml" => Some(Lang::Toml),
        "yaml" | "yml" => Some(Lang::Yaml),
        "html" => Some(Lang::Html),
        "css" => Some(Lang::Css),
        "bash" | "sh" | "shell" | "zsh" | "console" => Some(Lang::Bash),
        _ => None,
    };
    lang.and_then(config_for)
        .and_then(|config| highlight_with(config, code))
        .unwrap_or_else(|| plain_lines(code))
}

/// 16-color ANSI palette, tuned to read on the near-black viewer surface.
fn ansi16(index: u8) -> Rgb {
    match index {
        0 => Rgb::new(0x56, 0x5f, 0x89), // black, lifted to a visible gray
        1 => Rgb::new(0xf7, 0x76, 0x8e),
        2 => Rgb::new(0x9e, 0xce, 0x6a),
        3 => Rgb::new(0xe0, 0xaf, 0x68),
        4 => Rgb::new(0x7a, 0xa2, 0xf7),
        5 => Rgb::new(0xbb, 0x9a, 0xf7),
        6 => Rgb::new(0x7d, 0xcf, 0xff),
        7 => Rgb::new(0xc0, 0xca, 0xf5),
        8 => Rgb::new(0x73, 0x7a, 0xa2),
        9 => Rgb::new(0xff, 0x89, 0x9d),
        10 => Rgb::new(0xb9, 0xf2, 0x7c),
        11 => Rgb::new(0xff, 0x9e, 0x64),
        12 => Rgb::new(0x8d, 0xb0, 0xff),
        13 => Rgb::new(0xc7, 0xa9, 0xff),
        14 => Rgb::new(0xa4, 0xda, 0xff),
        _ => Rgb::new(0xfa, 0xfa, 0xfa),
    }
}

/// The xterm 256-color palette: the 16 base colors, a 6×6×6 color cube, then a gray ramp.
fn ansi256(index: u8) -> Rgb {
    match index {
        0..=15 => ansi16(index),
        16..=231 => {
            let index = index - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            Rgb::new(level(index / 36), level((index / 6) % 6), level(index % 6))
        },
        _ => {
            let v = 8 + 10 * (index - 232);
            Rgb::new(v, v, v)
        },
    }
}

/// Current SGR drawing attributes while parsing ANSI output.
#[derive(Default)]
struct Pen {
    /// Base 16-color index, when set (bold picks its bright variant).
    base: Option<u8>,
    /// Direct color from a 256-color or truecolor sequence.
    rgb: Option<Rgb>,
    bold: bool,
    dim: bool,
    italic: bool,
}

impl Pen {
    fn color(&self) -> Rgb {
        if let Some(rgb) = self.rgb {
            return rgb;
        }
        match self.base {
            Some(i) => ansi16(if self.bold && i < 8 { i + 8 } else { i }),
            None if self.dim => ansi16(8),
            None => plain_fg(),
        }
    }

    /// Snapshot of the attributes a span emitted now would carry.
    fn style(&self) -> (Rgb, bool, bool) {
        (self.color(), self.bold, self.italic)
    }
}

/// Apply one SGR sequence's parameters to the pen. Background and unsupported attributes are
/// ignored — the viewer only renders foreground colors.
fn apply_sgr(params: &str, pen: &mut Pen) {
    let mut codes = params.split(';').map(|code| code.parse::<u8>().unwrap_or(0));
    while let Some(code) = codes.next() {
        match code {
            0 => *pen = Pen::default(),
            1 => pen.bold = true,
            2 => pen.dim = true,
            3 => pen.italic = true,
            21 | 22 => {
                pen.bold = false;
                pen.dim = false;
            },
            23 => pen.italic = false,
            30..=37 => {
                pen.base = Some(code - 30);
                pen.rgb = None;
            },
            39 => {
                pen.base = None;
                pen.rgb = None;
            },
            90..=97 => {
                pen.base = Some(code - 90 + 8);
                pen.rgb = None;
            },
            38 => match codes.next() {
                Some(5) => {
                    if let Some(n) = codes.next() {
                        pen.rgb = Some(ansi256(n));
                        pen.base = None;
                    }
                },
                Some(2) => {
                    if let (Some(r), Some(g), Some(b)) = (codes.next(), codes.next(), codes.next())
                    {
                        pen.rgb = Some(Rgb::new(r, g, b));
                        pen.base = None;
                    }
                },
                _ => {},
            },
            _ => {},
        }
    }
}

/// Parse ANSI-colored terminal output (e.g. difftastic's) into per-line styled spans, expanding
/// tabs and dropping non-SGR escape sequences.
pub fn parse_ansi(text: &str) -> Vec<SpanLine> {
    fn flush(span: &mut String, style: (Rgb, bool, bool), line: &mut SpanLine) {
        if !span.is_empty() {
            let (fg, bold, italic) = style;
            line.push(Span { text: std::mem::take(span), fg, bold, italic });
        }
    }

    let mut lines = Vec::new();
    let mut line: SpanLine = Vec::new();
    let mut span = String::new();
    let mut pen = Pen::default();
    let mut style = pen.style();

    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => {
                if chars.peek() != Some(&'[') {
                    continue;
                }
                chars.next();
                let mut params = String::new();
                let mut terminator = None;
                for c in chars.by_ref() {
                    if c.is_ascii_digit() || c == ';' {
                        params.push(c);
                    } else {
                        terminator = Some(c);
                        break;
                    }
                }
                if terminator == Some('m') {
                    apply_sgr(&params, &mut pen);
                    let next = pen.style();
                    if next != style {
                        flush(&mut span, style, &mut line);
                        style = next;
                    }
                }
            },
            '\n' => {
                flush(&mut span, style, &mut line);
                lines.push(std::mem::take(&mut line));
            },
            '\r' => {},
            '\t' => span.push_str("    "),
            _ => span.push(ch),
        }
    }
    flush(&mut span, style, &mut line);
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_keywords_get_colored() {
        let (lines, truncated) =
            highlight_file("src/main.rs", "fn main() {\n    let x = \"hi\";\n}\n", 100);
        assert!(!truncated);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].iter().any(|span| span.text == "fn" && span.fg != plain_fg()));
    }

    #[test]
    fn large_file_highlights_quickly() {
        let mut src = String::new();
        for i in 0..4000 {
            src.push_str(&format!(
                "pub fn item_{i}(v: u32) -> Option<String> {{ (v > {i}).then(|| v.to_string()) }}\n"
            ));
        }
        let start = std::time::Instant::now();
        let (lines, truncated) = highlight_file("big.rs", &src, 4000);
        let elapsed = start.elapsed();
        assert!(!truncated);
        assert_eq!(lines.len(), 4000);
        // The point of tree-sitter over regex highlighting: this stays fast in debug builds.
        assert!(elapsed < std::time::Duration::from_secs(1), "highlight took {elapsed:?}");
    }

    #[test]
    fn truncation_slices_before_parsing() {
        let src = "line\n".repeat(10_000);
        let (lines, truncated) = highlight_file("notes.txt", &src, 100);
        assert!(truncated);
        assert_eq!(lines.len(), 100);
    }

    #[test]
    fn ansi_sgr_sequences_become_colored_spans() {
        // The shape difftastic emits: bright red+bold line number, reset, plain, bright green.
        let lines = parse_ansi("\x1b[91;1m2 \x1b[0mlet x = \x1b[92m2\x1b[0m;\n\x1b[2m5 \x1b[0m}\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), 4);
        assert_eq!(lines[0][0].text, "2 ");
        assert_eq!(lines[0][0].fg, ansi16(9)); // bold maps red to its bright variant
        assert!(lines[0][0].bold);
        assert_eq!(lines[0][1].fg, plain_fg());
        assert_eq!(lines[0][2].fg, ansi16(10));
        assert_eq!(lines[1][0].fg, ansi16(8)); // dim line number
    }

    #[test]
    fn ansi_truecolor_and_unknown_escapes() {
        let lines = parse_ansi("\x1b[38;2;1;2;3mx\x1b[0m\x1b[4my\x1b[0m");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0][0], Span::new("x", Rgb::new(1, 2, 3)));
        assert_eq!(lines[0][1].fg, plain_fg()); // underline alone keeps the default color
    }
}
