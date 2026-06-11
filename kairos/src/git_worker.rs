//! Background git queries for the right-side project pane.
//!
//! Each window owns one long-lived [`GitWorker`] thread. Requests are queued over an mpsc
//! channel; every result is sent back to the main thread through the winit `EventLoopProxy` as an
//! [`EventType::PaneGitData`] event tagged with the project root it was computed for. The
//! receiving side applies a result only to the project whose root still matches, so responses
//! arriving after a project switch are dropped. A single worker processes requests in order, so
//! same-root results always apply newest-last and no generation counter is needed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Sender};
use std::thread;

use log::{debug, error};
use winit::event_loop::EventLoopProxy;
use winit::window::WindowId;

use crate::event::{Event, EventType};
use crate::highlight::{self, SpanLine};

/// Cap on entries parsed from `git status`, so a pathological repo can't bloat the event.
const MAX_CHANGED_FILES: usize = 500;
/// Cap on entries parsed from the gitignored listing.
const MAX_IGNORED: usize = 2000;
/// Cap on parsed diff lines per file; longer diffs are truncated with a marker line.
const MAX_DIFF_LINES: usize = 4000;

/// `CREATE_NO_WINDOW`: keep git subprocesses from flashing a console window on Windows.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Work queued to the git thread.
enum GitRequest {
    /// Re-run `git status` (changed files + gitignored set) for `root`.
    Refresh { root: PathBuf },
    /// Produce the unified diff of one changed file.
    Diff { root: PathBuf, file: ChangedFile },
    /// Render one changed file's diff with the external `difft` (difftastic) tool.
    Difft { root: PathBuf, path: String, split: bool, width: usize },
}

/// Simplified status of a single changed file, derived from porcelain `XY` codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitFileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}

impl GitFileStatus {
    /// One-letter marker shown in the Changes list.
    pub fn letter(self) -> char {
        match self {
            Self::Modified => 'M',
            Self::Added => 'A',
            Self::Deleted => 'D',
            Self::Renamed => 'R',
            Self::Untracked => 'U',
            Self::Conflicted => '!',
        }
    }
}

/// One entry of the Changes list.
#[derive(Debug, Clone)]
pub struct ChangedFile {
    /// Path relative to the project root, `/`-separated as git prints it.
    pub path: String,
    pub status: GitFileStatus,
    /// `(added, removed)` line counts from `git diff --numstat`, when known (None for binary
    /// files, untracked files and repos without a commit).
    pub counts: Option<(u32, u32)>,
}

/// Kind of a parsed unified-diff line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Add,
    Del,
    /// `@@ … @@` hunk headers and informational lines (binary notice, truncation marker).
    Hunk,
}

/// One renderable line of a unified diff, with its `+`/`-`/` ` marker still in `text`.
#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
    /// Syntax-highlighted body (marker stripped), filled by [`highlight::highlight_diff`].
    /// Empty for hunk headers, which render from `text`.
    pub spans: SpanLine,
}

/// A result produced by the worker.
#[derive(Debug, Clone)]
pub enum GitData {
    /// Changed files and the gitignored set for the file tree. `is_repo` is `false` when the
    /// project root isn't inside a git repository (the Changes tab then shows a note).
    Status { changed: Vec<ChangedFile>, ignored: Vec<String>, is_repo: bool },
    /// Parsed unified diff for one changed file (keyed by its status-relative path).
    Diff { file: String, lines: Vec<DiffLine> },
    /// A difftastic rendering of one changed file's diff, parsed into colored spans. `None`
    /// means `difft` couldn't be run (not installed); the window falls back to the builtin diff.
    Difft { file: String, split: bool, lines: Option<Vec<SpanLine>> },
}

/// Handle to a window's git worker thread. Dropping it closes the channel, which ends the thread.
pub struct GitWorker {
    sender: Sender<GitRequest>,
}

impl GitWorker {
    /// Spawn the worker thread; results are delivered to `proxy` tagged with `window_id`.
    pub fn spawn(window_id: WindowId, proxy: EventLoopProxy<Event>) -> Self {
        let (sender, receiver) = mpsc::channel();
        let spawned = thread::Builder::new().name("git-worker".into()).spawn(move || {
            while let Ok(request) = receiver.recv() {
                let (root, data) = match request {
                    GitRequest::Refresh { root } => {
                        let data = run_status(&root);
                        (root, data)
                    },
                    GitRequest::Diff { root, file } => {
                        let data = run_diff(&root, &file);
                        (root, data)
                    },
                    GitRequest::Difft { root, path, split, width } => {
                        let data = run_difft(&root, &path, split, width);
                        (root, data)
                    },
                };
                let event = Event::new(EventType::PaneGitData { root, data }, window_id);
                if proxy.send_event(event).is_err() {
                    break;
                }
            }
        });
        // Best-effort like the rest of the pane: without the thread the pane just stays empty.
        if let Err(err) = spawned {
            error!("Failed to spawn the project pane's git worker: {err}");
        }
        Self { sender }
    }

    pub fn request_refresh(&self, root: PathBuf) {
        let _ = self.sender.send(GitRequest::Refresh { root });
    }

    pub fn request_diff(&self, root: PathBuf, file: ChangedFile) {
        let _ = self.sender.send(GitRequest::Diff { root, file });
    }

    pub fn request_difft(&self, root: PathBuf, path: String, split: bool, width: usize) {
        let _ = self.sender.send(GitRequest::Difft { root, path, split, width });
    }
}

/// Build a `program` invocation rooted at `root`, hidden from the console on Windows.
fn tool_command(program: &str, root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(program);
    command.args(args).current_dir(root);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Run `git` with `args` in `root`, returning stdout on success. `None` covers both a missing git
/// binary and a non-zero exit (e.g. not a repository).
fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    let output = match tool_command("git", root, args).output() {
        Ok(output) => output,
        Err(err) => {
            // Failing to even launch git (binary missing?) is worth a trace, unlike the routine
            // non-zero exit of running outside a repository.
            debug!("Failed to run git {args:?}: {err}");
            return None;
        },
    };
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Build the Changes list and gitignored set for `root`.
fn run_status(root: &Path) -> GitData {
    let Some(status) = git_output(root, &["status", "--porcelain"]) else {
        return GitData::Status { changed: Vec::new(), ignored: Vec::new(), is_repo: false };
    };

    let mut changed: Vec<ChangedFile> = status
        .lines()
        .take(MAX_CHANGED_FILES)
        .filter_map(parse_status_line)
        .collect();

    // Merge per-file added/removed line counts. Fails harmlessly on a repo without commits.
    if let Some(numstat) = git_output(root, &["diff", "--numstat", "HEAD"]) {
        for line in numstat.lines() {
            let mut parts = line.splitn(3, '\t');
            let (Some(adds), Some(dels), Some(path)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            // Binary files report "-\t-" and carry no line counts.
            let (Ok(adds), Ok(dels)) = (adds.parse::<u32>(), dels.parse::<u32>()) else { continue };
            let path = numstat_path(&unquote(path));
            if let Some(file) = changed.iter_mut().find(|f| f.path == path) {
                file.counts = Some((adds, dels));
            }
        }
    }

    // Gitignored files and fully-ignored directories (collapsed with a trailing `/`), used to dim
    // entries in the file tree.
    let ignored = git_output(root, &["ls-files", "--others", "--ignored", "--exclude-standard",
        "--directory"])
        .map(|out| out.lines().take(MAX_IGNORED).map(unquote).collect())
        .unwrap_or_default();

    GitData::Status { changed, ignored, is_repo: true }
}

/// Parse one `git status --porcelain` line (`XY path`, with `R`-entries as `old -> new`).
fn parse_status_line(line: &str) -> Option<ChangedFile> {
    if line.len() < 4 {
        return None;
    }
    let (code, rest) = line.split_at(2);
    let rest = rest.strip_prefix(' ')?;
    let path = match rest.split_once(" -> ") {
        Some((_, new)) => new,
        None => rest,
    };

    let (x, y) = {
        let mut chars = code.chars();
        (chars.next()?, chars.next()?)
    };
    let status = if x == '?' || y == '?' {
        GitFileStatus::Untracked
    } else if x == 'U' || y == 'U' || (x == 'D' && y == 'D') || (x == 'A' && y == 'A') {
        GitFileStatus::Conflicted
    } else if x == 'R' || y == 'R' {
        GitFileStatus::Renamed
    } else if x == 'D' || y == 'D' {
        GitFileStatus::Deleted
    } else if x == 'A' {
        GitFileStatus::Added
    } else {
        GitFileStatus::Modified
    };

    Some(ChangedFile { path: unquote(path), status, counts: None })
}

/// Strip the quotes git puts around paths with unusual characters. Escapes inside are left as-is
/// (display-only; such paths just render their escaped form).
fn unquote(path: &str) -> String {
    path.strip_prefix('"').and_then(|p| p.strip_suffix('"')).unwrap_or(path).to_owned()
}

/// Resolve a `--numstat` rename path to its destination, so the counts merge matches the path
/// from `git status`. Renames appear as `old => new` or with a shared prefix/suffix as
/// `prefix/{old => new}/suffix`; plain paths pass through unchanged.
fn numstat_path(raw: &str) -> String {
    if let (Some(open), Some(close)) = (raw.find('{'), raw.find('}')) {
        if open < close {
            if let Some((_, new)) = raw[open + 1..close].split_once(" => ") {
                let mut path = format!("{}{}{}", &raw[..open], new, &raw[close + 1..]);
                // An empty side ("dir/{ => sub}/f") leaves a doubled separator behind.
                while let Some(i) = path.find("//") {
                    path.remove(i);
                }
                return path;
            }
        }
    }
    match raw.split_once(" => ") {
        Some((_, new)) => new.to_owned(),
        None => raw.to_owned(),
    }
}

/// Produce the parsed unified diff for one changed file.
///
/// `--no-ext-diff` is essential: a user with `diff.external = difft` (or any external diff driver)
/// in their git config would otherwise get the driver's pre-formatted side-by-side text instead of
/// a native unified diff, which [`parse_diff`] can't read (no `+`/`-` markers → every line parses
/// as context → the split view shows identical columns). The window has its own difftastic path
/// ([`run_difft`]) for that look.
fn run_diff(root: &Path, file: &ChangedFile) -> GitData {
    let output = match file.status {
        // Untracked files have no blob to diff against; `--no-index` against the null device
        // produces a pure-addition diff. It exits 1 when the files differ (always, here), so this
        // goes through a success-agnostic runner.
        GitFileStatus::Untracked => {
            let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
            any_status_output("git", root, &["diff", "--no-ext-diff", "--no-index", "--", null,
                &file.path])
        },
        // `HEAD` covers staged and unstaged changes alike; fall back to the index diff for repos
        // without any commit yet.
        _ => git_output(root, &["diff", "--no-ext-diff", "HEAD", "--", &file.path])
            .filter(|out| !out.is_empty())
            .or_else(|| git_output(root, &["diff", "--no-ext-diff", "--", &file.path])),
    };

    let mut lines = output.map(|out| parse_diff(&out)).unwrap_or_default();
    // Syntax-color the code column here on the worker thread, off the render path.
    highlight::highlight_diff(&file.path, &mut lines);
    GitData::Diff { file: file.path.clone(), lines }
}

/// Like [`git_output`], but ignores the exit status (`git diff --no-index` exits 1 on
/// difference).
fn any_status_output(program: &str, root: &Path, args: &[&str]) -> Option<String> {
    let output = tool_command(program, root, args).output().ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Render one changed file's diff with difftastic, parsing its ANSI output into colored spans.
///
/// Both sides are materialized as real files sharing the changed file's name (so difftastic's
/// language detection works): the old side from `HEAD`'s blob (empty for untracked/added files),
/// the new side from the working tree (an empty stand-in for deleted files). `lines: None`
/// reports difftastic as unavailable, letting the window fall back to the builtin diff.
fn run_difft(root: &Path, path: &str, split: bool, width: usize) -> GitData {
    let unavailable = || GitData::Difft { file: path.to_owned(), split, lines: None };
    let Ok(dir) = tempfile::tempdir() else { return unavailable() };
    let name = Path::new(path).file_name().map(PathBuf::from).unwrap_or_else(|| "file".into());

    let old_dir = dir.path().join("old");
    let old_path = old_dir.join(&name);
    let old_bytes = tool_command("git", root, &["show", &format!("HEAD:{path}")])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| out.stdout)
        .unwrap_or_default();
    if fs::create_dir_all(&old_dir).is_err() || fs::write(&old_path, old_bytes).is_err() {
        return unavailable();
    }

    let new_abs = root.join(path);
    let new_path = if new_abs.is_file() {
        new_abs
    } else {
        let new_dir = dir.path().join("new");
        let stand_in = new_dir.join(&name);
        if fs::create_dir_all(&new_dir).is_err() || fs::write(&stand_in, b"").is_err() {
            return unavailable();
        }
        stand_in
    };

    let display = if split { "side-by-side" } else { "inline" };
    let width = width.to_string();
    let old_arg = old_path.to_string_lossy().into_owned();
    let new_arg = new_path.to_string_lossy().into_owned();
    let output = tool_command("difft", root, &[
        "--color", "always", "--display", display, "--width", &width, &old_arg, &new_arg,
    ])
    .output();

    let lines = match output {
        Ok(out) if out.status.success() => {
            Some(highlight::parse_ansi(&String::from_utf8_lossy(&out.stdout)))
        },
        // Failed to launch (not installed) or exited with an error: report unavailable.
        _ => None,
    };
    GitData::Difft { file: path.to_owned(), split, lines }
}

/// Parse unified diff output into renderable lines, skipping the per-file header noise.
fn parse_diff(output: &str) -> Vec<DiffLine> {
    let mut lines = Vec::new();
    for raw in output.lines() {
        let line = raw.trim_end_matches('\r');
        // Tabs render as zero-width in the chrome's glyph emitter; expand them.
        let text = line.replace('\t', "    ");

        let kind = if line.starts_with("@@") || line.starts_with("Binary files") {
            DiffLineKind::Hunk
        } else if line.starts_with("+++")
            || line.starts_with("---")
            || line.starts_with("diff --git")
            || line.starts_with("index ")
            || line.starts_with("new file mode")
            || line.starts_with("deleted file mode")
            || line.starts_with("old mode")
            || line.starts_with("new mode")
            || line.starts_with("similarity index")
            || line.starts_with("rename from")
            || line.starts_with("rename to")
        {
            continue;
        } else if line.starts_with('+') {
            DiffLineKind::Add
        } else if line.starts_with('-') {
            DiffLineKind::Del
        } else {
            DiffLineKind::Context
        };

        lines.push(DiffLine { kind, text, spans: Vec::new() });
        if lines.len() >= MAX_DIFF_LINES {
            lines.push(DiffLine {
                kind: DiffLineKind::Hunk,
                text: "… (diff 已截断)".into(),
                spans: Vec::new(),
            });
            break;
        }
    }
    lines
}
