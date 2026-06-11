//! Discovery of Claude Code session transcripts for a project folder.
//!
//! Claude Code stores each conversation as a line-delimited JSON transcript at
//! `~/.claude/projects/<encoded-cwd>/<session-uuid>.jsonl`, where `<encoded-cwd>` is the project's
//! absolute path with every path separator, the drive-letter colon and dots flattened to `-` (so
//! `D:\work\alacritty` becomes `D--work-alacritty` and `/home/me/proj` becomes `-home-me-proj`).
//! This module enumerates those transcripts for a given project root and derives a short label (the
//! first user prompt) for each, so the in-app sidebar can list them and resume one in a new tab.
//!
//! Transcripts can be large (megabytes), but the label lives near the top, so only the first few
//! lines of each file are read.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Maximum number of sessions surfaced per project.
const MAX_SESSIONS_SHOWN: usize = 12;
/// Sessions whose transcript hasn't been touched within this window are hidden.
const MAX_SESSION_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);
/// Lines read from the head of a transcript while searching for the first user prompt.
const HEAD_LINES: usize = 40;

/// A Claude Code session transcript that can be resumed.
#[derive(Debug, Clone)]
pub struct ClaudeSession {
    /// Session UUID (the transcript's file stem). Validated to a UUID-ish charset.
    pub id: String,
    /// Human label: the first user prompt, collapsed to one line, or `"(no prompt)"`.
    pub label: String,
}

/// Enumerate the Claude Code sessions for `root`, newest first, capped at [`MAX_SESSIONS_SHOWN`]
/// and limited to transcripts modified within the last [`MAX_SESSION_AGE`].
///
/// Returns an empty list when there is no home directory, no `~/.claude/projects/<root>` directory,
/// or the directory can't be read.
pub fn sessions_for(root: &Path) -> Vec<ClaudeSession> {
    let Some(dir) = project_dir(root) else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let cutoff = SystemTime::now().checked_sub(MAX_SESSION_AGE);

    // Collect valid transcripts with their mtime, then sort newest-first and cap before parsing
    // labels — so we never read the head of more files than we display.
    let mut found: Vec<(String, PathBuf, SystemTime)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        if !is_session_id(stem) {
            continue;
        }
        let mtime =
            entry.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
        if cutoff.is_some_and(|cutoff| mtime < cutoff) {
            continue;
        }
        found.push((stem.to_owned(), path, mtime));
    }
    found.sort_by_key(|entry| std::cmp::Reverse(entry.2));
    found.truncate(MAX_SESSIONS_SHOWN);

    found
        .into_iter()
        .map(|(id, path, _)| {
            let label = read_label(&path).unwrap_or_else(|| "(no prompt)".to_owned());
            ClaudeSession { id, label }
        })
        .collect()
}

/// `~/.claude/projects/<encoded>`, where `<encoded>` mirrors Claude Code's cwd encoding.
fn project_dir(root: &Path) -> Option<PathBuf> {
    Some(home::home_dir()?.join(".claude").join("projects").join(encode_root(root)))
}

/// Encode a project root the way Claude Code names its `~/.claude/projects/<name>` directory: every
/// path separator (`/` and `\`), the Windows drive-letter colon and dots are flattened to `-`.
///
/// Encoding the backslash and colon is essential on Windows — without it `D:\work\alacritty` stays
/// an absolute path, and `home_dir().join(<that>)` would silently discard the `~/.claude/projects`
/// prefix and point at the project folder itself, which never contains transcripts. Any `\\?\`
/// verbatim prefix is stripped first so the name matches what Claude Code actually wrote on disk.
fn encode_root(root: &Path) -> String {
    let raw = root.to_string_lossy();
    let raw = raw.strip_prefix(r"\\?\").unwrap_or(raw.as_ref());
    raw.chars().map(|c| if matches!(c, '/' | '\\' | '.' | ':') { '-' } else { c }).collect()
}

/// Whether `s` looks like a session UUID (hex digits and hyphens, plausible length). Used to reject
/// stray filenames before they ever reach a `claude --resume <id>` command.
fn is_session_id(s: &str) -> bool {
    (10..=64).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Derive a one-line label from the head of a transcript: the first genuinely typed user prompt,
/// cleaned of injected markup.
///
/// Claude Code prepends system-injected user messages — a `<local-command-caveat>` banner (flagged
/// `isMeta`), slash-command wrappers (`<command-name>…`), local-command output — before the real
/// prompt. We skip meta messages and prefer the first message tagged `promptSource: "typed"`,
/// falling back to the first usable line for older transcripts that predate that field.
fn read_label(path: &Path) -> Option<String> {
    let reader = BufReader::new(File::open(path).ok()?);
    let mut fallback: Option<String> = None;
    for line in reader.lines().take(HEAD_LINES) {
        let Ok(line) = line else { continue };
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        if value.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        // Skip system-injected meta messages, e.g. the `<local-command-caveat>` banner.
        if value.get("isMeta").and_then(|m| m.as_bool()) == Some(true) {
            continue;
        }
        let Some(text) = value.get("message").and_then(|m| m.get("content")).and_then(content_text)
        else {
            continue;
        };
        let label = clean_label(&text);
        if label.is_empty() {
            continue;
        }
        // A genuinely typed prompt is the ideal label; take the first one we encounter.
        if value.get("promptSource").and_then(|s| s.as_str()) == Some("typed") {
            return Some(label);
        }
        // Otherwise remember the first usable line (covers transcripts that predate `promptSource`
        // or open with a slash command) while continuing to look for a typed prompt.
        fallback.get_or_insert(label);
    }
    fallback
}

/// Extract text from a message `content`, which is either a plain string or an array of content
/// blocks (`[{ "type": "text", "text": "…" }, …]`).
fn content_text(content: &serde_json::Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    let blocks = content.as_array()?;
    for block in blocks {
        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
            return Some(text.to_owned());
        }
    }
    None
}

/// Collapse all runs of whitespace (including newlines) to single spaces and trim, so a multi-line
/// prompt becomes a single tidy label.
fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Turn a raw message into a tidy one-line label: strip injected tag markers, then collapse
/// whitespace.
fn clean_label(s: &str) -> String {
    collapse_whitespace(&strip_wrapper_tags(s))
}

/// Remove Claude Code's injected wrapper tags — `<local-command-caveat>`, `<command-name>`,
/// `<system-reminder>`, `<bash-input>`, etc. — keeping any text between them.
///
/// Only kebab-case tags (names containing a `-`) are treated as wrappers, so code-like angle
/// brackets in a real prompt (`Vec<String>`, `<T>`, `x < 3`) and unterminated `<` are left intact.
fn strip_wrapper_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(lt) = rest.find('<') {
        let after = &rest[lt + 1..];
        // A tag name follows an optional closing slash; injected wrappers are kebab-case.
        let name_start = after.strip_prefix('/').unwrap_or(after);
        let name_len = name_start
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
            .unwrap_or(name_start.len());
        let name = &name_start[..name_len];
        if name.contains('-') {
            if let Some(gt) = after.find('>') {
                out.push_str(&rest[..lt]);
                rest = &after[gt + 1..];
                continue;
            }
        }
        // Not a wrapper tag (or no closing `>`): keep the `<` literally and move on.
        out.push_str(&rest[..=lt]);
        rest = &rest[lt + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_roots_like_claude_code() {
        // Windows: backslashes and the drive colon flatten to '-' (the case that was broken).
        assert_eq!(encode_root(Path::new(r"D:\work\alacritty")), "D--work-alacritty");
        // A '\\?\' verbatim prefix is stripped before encoding.
        assert_eq!(encode_root(Path::new(r"\\?\D:\work\alacritty")), "D--work-alacritty");
        // Unix: slashes and dots flatten to '-'.
        assert_eq!(encode_root(Path::new("/home/me/project.x")), "-home-me-project-x");
    }

    #[test]
    fn session_id_validation() {
        assert!(is_session_id("f152b8af-a2b7-4fa8-8992-33ebcbc22e16"));
        assert!(!is_session_id("not a uuid"));
        assert!(!is_session_id("../evil"));
        assert!(!is_session_id("rm -rf /"));
        assert!(!is_session_id("short"));
    }

    #[test]
    fn content_as_string() {
        let v: serde_json::Value = serde_json::json!(" 运行下看看现在的效果 ");
        assert_eq!(content_text(&v).as_deref(), Some(" 运行下看看现在的效果 "));
    }

    #[test]
    fn content_as_blocks() {
        let v: serde_json::Value =
            serde_json::json!([{ "type": "text", "text": "hello there" }]);
        assert_eq!(content_text(&v).as_deref(), Some("hello there"));
    }

    #[test]
    fn whitespace_is_collapsed() {
        assert_eq!(collapse_whitespace("  a\n  b\t c  "), "a b c");
    }

    #[test]
    fn strips_injected_wrapper_tags() {
        // Kebab-case wrapper tags are removed, inner text kept.
        assert_eq!(clean_label("<command-name>/effort</command-name>"), "/effort");
        assert_eq!(
            clean_label("<local-command-caveat>Caveat: stuff</local-command-caveat>"),
            "Caveat: stuff"
        );
        assert_eq!(clean_label("<system-reminder>note</system-reminder> hi"), "note hi");
    }

    #[test]
    fn keeps_code_like_angle_brackets() {
        // Non-kebab tags and bare comparisons in real prompts survive untouched.
        assert_eq!(clean_label("wrap Vec<String> nicely"), "wrap Vec<String> nicely");
        assert_eq!(clean_label("compare x < 3 and y > 2"), "compare x < 3 and y > 2");
        assert_eq!(clean_label("an <oops unterminated"), "an <oops unterminated");
    }
}
