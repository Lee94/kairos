//! Render Markdown into styled viewer lines — the file preview shows the formatted result
//! (headings, lists, code blocks) instead of source text.
//!
//! The pulldown-cmark event stream drives a small line builder: inline content is word-wrapped
//! to the viewer width with hanging indents for lists and quote bars, fenced code blocks are
//! syntax-highlighted through the tree-sitter pipeline, and inline styles map to span colors
//! plus bold/italic font variants.

use std::path::Path;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::display::chrome::str_width;
use crate::display::color::Rgb;
use crate::highlight::{self, Span, SpanLine};

fn text_fg() -> Rgb {
    Rgb::new(0xfa, 0xfa, 0xfa)
}
fn dim_fg() -> Rgb {
    Rgb::new(0xa1, 0xa1, 0xaa)
}
fn code_fg() -> Rgb {
    Rgb::new(0x73, 0xda, 0xca)
}
fn link_fg() -> Rgb {
    Rgb::new(0x7d, 0xcf, 0xff)
}
fn heading_fg(level: u8) -> Rgb {
    match level {
        1 => Rgb::new(0x7a, 0xa2, 0xf7),
        2 => Rgb::new(0x7d, 0xcf, 0xff),
        3 => Rgb::new(0xbb, 0x9a, 0xf7),
        _ => Rgb::new(0x9a, 0xa5, 0xce),
    }
}

/// Whether `path` names a markdown file (by extension).
pub fn is_markdown(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown"))
}

/// Render `text` as formatted markdown lines, word-wrapped to `width` display cells.
pub fn render(text: &str, width: usize) -> Vec<SpanLine> {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut renderer = Renderer::new(width.max(20));
    for event in Parser::new_ext(text, options) {
        renderer.event(event);
    }
    renderer.finish()
}

struct Renderer {
    lines: Vec<SpanLine>,
    line: SpanLine,
    /// Display cells already used on the current line.
    col: usize,
    width: usize,
    /// Continuation prefix (quote bars + list hanging indent), re-emitted on wraps.
    prefix: SpanLine,
    prefix_cols: usize,
    /// A space is owed before the next word (collapsed whitespace / soft break).
    pending_space: bool,
    /// The next block starts a list item or quote, so it skips its blank separator.
    fresh_block: bool,

    // Inline style state.
    heading: Option<u8>,
    bold: usize,
    italic: usize,
    strike: usize,
    code_inline: bool,
    link: usize,
    /// Forced color (raw HTML passthrough).
    force_fg: Option<Rgb>,

    // Block state.
    quote: usize,
    /// One entry per open list: the next ordinal for ordered lists, `None` for bullets.
    lists: Vec<Option<u64>>,
    /// Hanging indent of the current list item's marker.
    hang: usize,
    /// Fence token while buffering a code block's text.
    code_block: Option<String>,
    code_buf: String,
}

impl Renderer {
    fn new(width: usize) -> Self {
        Self {
            lines: Vec::new(),
            line: Vec::new(),
            col: 0,
            width,
            prefix: Vec::new(),
            prefix_cols: 0,
            pending_space: false,
            fresh_block: false,
            heading: None,
            bold: 0,
            italic: 0,
            strike: 0,
            code_inline: false,
            link: 0,
            force_fg: None,
            quote: 0,
            lists: Vec::new(),
            hang: 0,
            code_block: None,
            code_buf: String::new(),
        }
    }

    fn finish(mut self) -> Vec<SpanLine> {
        self.flush_line();
        self.lines
    }

    /// The style a span appended right now would carry.
    fn style(&self) -> (Rgb, bool, bool) {
        let fg = if self.code_inline {
            code_fg()
        } else if let Some(fg) = self.force_fg {
            fg
        } else if self.link > 0 {
            link_fg()
        } else if let Some(level) = self.heading {
            heading_fg(level)
        } else if self.strike > 0 || self.quote > 0 {
            dim_fg()
        } else {
            text_fg()
        };
        (fg, self.heading.is_some() || self.bold > 0, self.italic > 0)
    }

    /// Append `text` to the current line, merging into the last span when the style matches.
    fn append(&mut self, text: &str, (fg, bold, italic): (Rgb, bool, bool)) {
        if text.is_empty() {
            return;
        }
        self.col += str_width(text);
        if let Some(last) = self.line.last_mut() {
            if last.fg == fg && last.bold == bold && last.italic == italic {
                last.text.push_str(text);
                return;
            }
        }
        self.line.push(Span { text: text.to_owned(), fg, bold, italic });
    }

    /// Emit one word, wrapping to a fresh prefixed line when it would overflow the width.
    fn push_word(&mut self, word: &str) {
        let style = self.style();
        let space = usize::from(self.pending_space && self.col > self.prefix_cols);
        if self.col > self.prefix_cols && self.col + space + str_width(word) > self.width {
            self.flush_line();
        } else if space == 1 {
            self.append(" ", style);
        }
        self.pending_space = false;
        self.append(word, style);
    }

    /// Emit inline text word by word (collapsing whitespace, like rendered markdown).
    fn push_text(&mut self, text: &str) {
        let mut word = String::new();
        for ch in text.chars() {
            if ch.is_whitespace() {
                if !word.is_empty() {
                    self.push_word(&word);
                    word.clear();
                }
                self.pending_space = true;
            } else {
                word.push(ch);
            }
        }
        if !word.is_empty() {
            self.push_word(&word);
        }
    }

    /// Finish the current line (if it has content beyond the prefix) and start a prefixed one.
    fn flush_line(&mut self) {
        if self.col > self.prefix_cols {
            self.lines.push(std::mem::take(&mut self.line));
        }
        self.line = self.prefix.clone();
        self.col = self.prefix_cols;
        self.pending_space = false;
    }

    /// Append a pre-built line (code block row, rule) below the current content.
    fn push_raw(&mut self, spans: SpanLine) {
        self.flush_line();
        let mut line = self.prefix.clone();
        line.extend(spans);
        self.lines.push(line);
    }

    /// Separate blocks with one blank line (suppressed right after a list item / quote opens).
    fn block_break(&mut self) {
        if std::mem::take(&mut self.fresh_block) {
            return;
        }
        self.flush_line();
        if self.lines.last().is_some_and(|line| !line.is_empty()) {
            // Carry the quote bars onto the separator so quoted blocks stay connected.
            self.lines.push(self.prefix.clone());
        }
    }

    /// Rebuild the continuation prefix from the quote depth and list indentation.
    fn rebuild_prefix(&mut self) {
        let mut prefix: SpanLine = Vec::new();
        if self.quote > 0 {
            prefix.push(Span::new("▎ ".repeat(self.quote), dim_fg()));
        }
        let depth = self.lists.len().saturating_sub(1);
        let indent = " ".repeat(2 * depth + self.hang);
        if !indent.is_empty() {
            prefix.push(Span::new(indent, text_fg()));
        }
        self.prefix_cols = prefix.iter().map(|span| str_width(&span.text)).sum();
        self.prefix = prefix;
    }

    /// Begin a list item: marker line plus hanging indent for its continuation.
    fn start_item(&mut self) {
        self.hang = 0;
        self.rebuild_prefix();
        self.flush_line();
        let marker = match self.lists.last_mut() {
            Some(Some(n)) => {
                let marker = format!("{n}. ");
                *n += 1;
                marker
            },
            _ => "• ".to_owned(),
        };
        self.append(&marker, (dim_fg(), false, false));
        self.hang = str_width(&marker);
        self.rebuild_prefix();
        self.fresh_block = true;
    }

    /// Flush the buffered fenced code block through the syntax highlighter, indented.
    fn finish_code_block(&mut self) {
        let token = self.code_block.take().unwrap_or_default();
        let code = std::mem::take(&mut self.code_buf);
        for mut spans in highlight::highlight_fenced(&token, &code) {
            let mut line = vec![Span::new("  ", text_fg())];
            line.append(&mut spans);
            self.push_raw(line);
        }
        self.fresh_block = false;
    }

    /// Width for rules / table separators: the remaining line width, within reason.
    fn rule_width(&self) -> usize {
        self.width.saturating_sub(self.prefix_cols).clamp(4, 40)
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if self.code_block.is_some() {
                    self.code_buf.push_str(&text);
                } else {
                    self.push_text(&text);
                }
            },
            Event::Code(text) => {
                self.code_inline = true;
                self.push_text(&text);
                self.code_inline = false;
            },
            Event::SoftBreak => self.pending_space = true,
            Event::HardBreak => self.flush_line(),
            Event::Rule => {
                self.block_break();
                self.push_raw(vec![Span::new("─".repeat(self.rule_width()), dim_fg())]);
                self.fresh_block = false;
            },
            Event::TaskListMarker(done) => {
                let marker = if done { "☑ " } else { "☐ " };
                self.append(marker, (dim_fg(), false, false));
            },
            Event::Html(text) | Event::InlineHtml(text) => {
                self.force_fg = Some(dim_fg());
                self.push_text(&text);
                self.force_fg = None;
            },
            Event::FootnoteReference(name) => {
                let style = (dim_fg(), false, false);
                self.append(&format!("[^{name}]"), style);
            },
            _ => {},
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.block_break(),
            Tag::Heading { level, .. } => {
                self.fresh_block = false;
                self.block_break();
                let level = heading_level(level);
                self.heading = Some(level);
                self.append("▌ ", (heading_fg(level), true, false));
            },
            Tag::BlockQuote(_) => {
                self.fresh_block = false;
                self.block_break();
                self.quote += 1;
                self.rebuild_prefix();
                self.line = self.prefix.clone();
                self.col = self.prefix_cols;
                self.fresh_block = true;
            },
            Tag::CodeBlock(kind) => {
                self.fresh_block = false;
                self.block_break();
                let token = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split([' ', ',']).next().unwrap_or_default().to_owned()
                    },
                    CodeBlockKind::Indented => String::new(),
                };
                self.code_block = Some(token);
                self.code_buf.clear();
            },
            Tag::List(start) => {
                if self.lists.is_empty() {
                    self.block_break();
                }
                self.lists.push(start);
            },
            Tag::Item => self.start_item(),
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { .. } => self.link += 1,
            Tag::Image { .. } => {
                self.link += 1;
                self.push_text("[图]");
            },
            Tag::Table(_) => {
                self.fresh_block = false;
                self.block_break();
            },
            Tag::TableHead => self.bold += 1,
            Tag::TableCell if self.col > self.prefix_cols => {
                self.pending_space = false;
                self.append(" │ ", (dim_fg(), false, false));
            },
            _ => {},
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_line(),
            TagEnd::Heading(_) => {
                self.flush_line();
                self.heading = None;
            },
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.quote = self.quote.saturating_sub(1);
                self.rebuild_prefix();
                self.line = self.prefix.clone();
                self.col = self.prefix_cols;
            },
            TagEnd::CodeBlock => self.finish_code_block(),
            TagEnd::List(_) => {
                self.lists.pop();
                self.hang = 0;
                self.rebuild_prefix();
            },
            TagEnd::Item => {
                self.flush_line();
                self.hang = 0;
                self.rebuild_prefix();
                self.line = self.prefix.clone();
                self.col = self.prefix_cols;
                self.fresh_block = false;
            },
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => self.link = self.link.saturating_sub(1),
            TagEnd::Image => self.link = self.link.saturating_sub(1),
            TagEnd::Table => self.flush_line(),
            TagEnd::TableHead => {
                self.bold = self.bold.saturating_sub(1);
                self.flush_line();
                self.push_raw(vec![Span::new("─".repeat(self.rule_width()), dim_fg())]);
            },
            TagEnd::TableRow => self.flush_line(),
            _ => {},
        }
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_lists_and_code_render() {
        let lines = render(
            "# Title\n\nSome *body* text.\n\n- first\n- second\n\n```rust\nfn main() {}\n```\n",
            60,
        );
        let text: Vec<String> = lines
            .iter()
            .map(|line| line.iter().map(|span| span.text.as_str()).collect())
            .collect();

        // Heading rendered with its marker, not the `#` source syntax.
        assert!(text[0].starts_with("▌ Title"));
        assert!(lines[0].iter().any(|span| span.bold));
        // Bullets replace the `-` markers.
        assert!(text.iter().any(|line| line.starts_with("• first")));
        // The code block kept its content and gained syntax colors.
        let code_line = lines
            .iter()
            .find(|line| line.iter().any(|span| span.text.contains("main")))
            .expect("code line rendered");
        assert!(code_line.iter().any(|span| span.fg != text_fg() && span.fg != dim_fg()));
    }

    #[test]
    fn paragraphs_wrap_to_width() {
        // 20 is the renderer's minimum wrap width.
        let lines = render("aaaa bbbb cccc dddd eeee ffff gggg hhhh\n", 20);
        let text: Vec<String> = lines
            .iter()
            .map(|line| line.iter().map(|span| span.text.as_str()).collect())
            .collect();
        assert!(text.iter().all(|line| str_width(line) <= 20), "lines: {text:?}");
        assert!(text.len() >= 2);
    }
}
