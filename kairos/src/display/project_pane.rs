//! Right-side project pane: a "文件" (Files) tab browsing the project's file tree and a "更改"
//! (Changes) tab listing git-changed files. Clicking a file (or a changed file) opens the center
//! viewer — an overlay covering the terminal area — showing the file's content or its unified
//! diff in one consistent presentation.
//!
//! The pane follows the chrome's architecture: per-project *data* ([`ProjectPaneData`]) is cached
//! on the window's `Project` and passed into layout by reference each frame, while transient *UI
//! state* ([`ProjectPaneState`]: active tab, scroll offsets, open viewer) lives inside `Chrome`
//! next to the other chrome state. Layout emits the same absolute-pixel rects/cells as the rest of
//! the chrome and registers `Hit` regions for clicks.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use unicode_width::UnicodeWidthChar;

use crate::display::color::Rgb;
use crate::git_worker::{ChangedFile, DiffLine, DiffLineKind, GitFileStatus};
use crate::highlight::{self, SpanLine};
use crate::markdown;
use crate::renderer::rects::RenderRect;

use super::chrome::{
    self, ChromeDraw, Hit, PixelRect, baseline, push_text_px, push_text_px_styled, rect,
    str_width, truncate,
};

/// Cap on entries listed per directory, guarding against e.g. huge build output folders.
const MAX_DIR_ENTRIES: usize = 500;

/// Cap on lines loaded for a file preview; longer files are truncated with a marker line.
const MAX_PREVIEW_LINES: usize = 4000;

/// Height multipliers (× chrome cell height) for the pane's row kinds.
const TREE_ROW_MULT: f32 = 1.3;
const DIFF_ROW_MULT: f32 = 1.2;

// Diff / status colors, chosen to read on the zinc chrome background.
fn add_fg() -> Rgb {
    Rgb::new(0x7e, 0xd9, 0x9b)
}
fn del_fg() -> Rgb {
    Rgb::new(0xf2, 0x8b, 0x82)
}
fn hunk_fg() -> Rgb {
    Rgb::new(0x7a, 0xa2, 0xf7)
}
fn add_bg() -> Rgb {
    Rgb::new(0x1b, 0x42, 0x29)
}
fn del_bg() -> Rgb {
    Rgb::new(0x4c, 0x20, 0x26)
}
/// Muted fill for the empty side of a changed split row (no counterpart line), so the gap reads as
/// part of the comparison rather than blank surface.
fn absent_bg() -> Rgb {
    Rgb::new(0x14, 0x14, 0x17)
}

/// Pending / in-progress amber.
fn pending_fg() -> Rgb {
    Rgb::new(0xe5, 0xc0, 0x7b)
}

/// Text-selection highlight in the center viewer: the chrome's macOS-style blue, drawn
/// translucent so the glyphs on top stay readable.
fn sel_bg() -> Rgb {
    Rgb::new(0x2f, 0x6f, 0xed)
}
const SEL_ALPHA: f32 = 0.35;

/// Color for a changed file's status letter.
fn status_color(status: GitFileStatus) -> Rgb {
    match status {
        GitFileStatus::Modified | GitFileStatus::Renamed => pending_fg(),
        GitFileStatus::Added | GitFileStatus::Untracked => add_fg(),
        GitFileStatus::Deleted | GitFileStatus::Conflicted => del_fg(),
    }
}

/// Which tab of the project pane is active.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum PaneTab {
    #[default]
    Files,
    Changes,
}

/// What the center viewer shows: a file's content or a changed file's diff, both keyed by the
/// root-relative path.
#[derive(Clone, PartialEq, Eq)]
pub enum ViewerContent {
    File(String),
    Diff(String),
}

/// The two shared viewer tabs living in the tab bar: every file preview reuses the 预览 tab and
/// every diff view reuses the diff tab.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ViewerKind {
    Preview,
    Diff,
}

/// A position within the center viewer's flattened content: a row index into the laid-out rows
/// and a display column (cell), with column 0 at the first text cell of every row.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ViewerPos {
    row: usize,
    col: usize,
}

/// A text selection in the center viewer. Anchored where the drag began; `focus` tracks the
/// pointer. It is empty (no highlight, nothing to copy) until the drag reaches another cell.
#[derive(Clone, Copy)]
struct ViewerSelection {
    anchor: ViewerPos,
    focus: ViewerPos,
}

impl ViewerSelection {
    /// `(start, end)` ordered so `start` precedes `end` in reading order.
    fn ordered(&self) -> (ViewerPos, ViewerPos) {
        let (a, f) = (self.anchor, self.focus);
        if (a.row, a.col) <= (f.row, f.col) { (a, f) } else { (f, a) }
    }

    fn is_empty(&self) -> bool {
        self.anchor == self.focus
    }
}

/// Geometry of the last viewer layout, mapping pointer pixels to a content `(row, col)` and
/// anchoring the selection highlight. Column 0 sits at `text_x`; each cell is `cw` wide.
#[derive(Clone, Copy)]
struct ViewerGeom {
    text_x: f32,
    content_y: f32,
    line_h: f32,
    cw: f32,
    /// First content row drawn (the scroll offset), so a pointer pixel maps to an absolute row.
    first: usize,
}

/// Cache key for [`ProjectPaneState::viewer_lines`]: the plain text is rebuilt only when the
/// viewer's content identity (tab, path, layout/engine, line count) changes, so dragging a
/// selection across a large file doesn't re-render it every frame.
#[derive(Clone, PartialEq, Eq)]
struct ViewerLinesKey {
    kind: ViewerKind,
    path: String,
    split: bool,
    difft: bool,
    md_source: bool,
    rows: usize,
}

/// Scroll offset of one pane view, in rows. Stored as `f32` so pixel-precise touchpad deltas
/// accumulate; the layout pass clamps it against the actual row count.
#[derive(Clone, Copy, Default)]
pub struct ScrollState {
    offset: f32,
}

impl ScrollState {
    pub(super) fn scroll_by(&mut self, rows: f32) {
        self.offset = (self.offset + rows).max(0.);
    }

    /// Clamp so at least `keep` rows stay visible at the end of an `total`-row list.
    fn clamp(&mut self, total: usize, keep: usize) {
        self.offset = self.offset.min(total.saturating_sub(keep) as f32).max(0.);
    }

    /// Index of the first row to draw.
    fn first_row(&self) -> usize {
        self.offset as usize
    }
}

/// Transient UI state of the pane (lives inside `Chrome`).
#[derive(Default)]
pub struct ProjectPaneState {
    pub(super) tab: PaneTab,
    pub(super) files_scroll: ScrollState,
    pub(super) changes_scroll: ScrollState,
    /// Path shown in the shared 预览 (file preview) tab, when it is open.
    pub(super) preview_tab: Option<String>,
    /// Path shown in the shared diff tab, when it is open.
    pub(super) diff_tab: Option<String>,
    /// The focused viewer tab; `None` means a terminal tab is active.
    pub(super) viewer_focus: Option<ViewerKind>,
    preview_scroll: ScrollState,
    diff_scroll: ScrollState,
    /// Whether the viewer renders diffs side-by-side (分栏) instead of unified (行间).
    pub(super) diff_split: bool,
    /// Whether diffs are rendered by the external difftastic tool instead of the builtin diff.
    pub(super) difft_mode: bool,
    /// Whether the markdown preview shows source text instead of rendered output.
    pub(super) md_source: bool,
    /// `(visible index, rel_path)` of the directory rows registered as hits by the last layout.
    /// Click hits carry the index; resolving it against this snapshot (rebuilt together with the
    /// hit regions) keeps clicks correct even when a git refresh replaces the tree in between.
    pub(super) dir_rows: Vec<(usize, String)>,
    /// `(visible index, rel_path)` of the plain-file rows registered by the last layout, for the
    /// same reason as [`Self::dir_rows`].
    pub(super) file_rows: Vec<(usize, String)>,
    /// `(changed-list index, rel_path)` of the changed-file rows registered by the last layout.
    pub(super) change_rows: Vec<(usize, String)>,
    /// The center viewer's text selection (file preview / diff), if any.
    viewer_sel: Option<ViewerSelection>,
    /// Whether a viewer selection drag is currently in progress.
    viewer_sel_dragging: bool,
    /// Plain text of every viewer content row from the last layout, indexed by row. Used to clamp
    /// selection columns and extract the selected text; rebuilt per [`ViewerLinesKey`].
    viewer_lines: Vec<String>,
    viewer_lines_key: Option<ViewerLinesKey>,
    /// Pixel/row geometry from the last viewer layout, for pointer↔cell mapping and the highlight.
    viewer_geom: Option<ViewerGeom>,
}

impl ProjectPaneState {
    /// Open (or retarget) the shared viewer tab of `kind` to `path`, focus it, and rewind its
    /// scroll for the new content.
    pub(super) fn open_viewer(&mut self, kind: ViewerKind, path: &str) {
        match kind {
            ViewerKind::Preview => {
                self.preview_tab = Some(path.to_owned());
                self.preview_scroll = ScrollState::default();
            },
            ViewerKind::Diff => {
                self.diff_tab = Some(path.to_owned());
                self.diff_scroll = ScrollState::default();
            },
        }
        self.viewer_focus = Some(kind);
        self.clear_viewer_selection();
    }

    /// Focus the viewer tab of `kind`, when it is open (keeps its scroll position).
    pub(super) fn focus_viewer(&mut self, kind: ViewerKind) {
        let open = match kind {
            ViewerKind::Preview => self.preview_tab.is_some(),
            ViewerKind::Diff => self.diff_tab.is_some(),
        };
        if open {
            self.viewer_focus = Some(kind);
            self.clear_viewer_selection();
        }
    }

    /// Close the viewer tab of `kind`, dropping focus back to the terminal when it was focused.
    pub(super) fn close_viewer_tab(&mut self, kind: ViewerKind) {
        match kind {
            ViewerKind::Preview => self.preview_tab = None,
            ViewerKind::Diff => self.diff_tab = None,
        }
        if self.viewer_focus == Some(kind) {
            self.viewer_focus = None;
        }
        self.clear_viewer_selection();
    }

    /// Scroll the focused viewer tab by `rows`.
    pub(super) fn scroll_viewer(&mut self, rows: f32) {
        match self.viewer_focus {
            Some(ViewerKind::Preview) => self.preview_scroll.scroll_by(rows),
            Some(ViewerKind::Diff) => self.diff_scroll.scroll_by(rows),
            None => {},
        }
    }

    /// Content of the focused viewer tab, if any.
    fn focused_content(&self) -> Option<ViewerContent> {
        match self.viewer_focus? {
            ViewerKind::Preview => self.preview_tab.clone().map(ViewerContent::File),
            ViewerKind::Diff => self.diff_tab.clone().map(ViewerContent::Diff),
        }
    }

    /// Reset the per-view state (scroll offsets, viewer tabs) when the project the pane shows
    /// changes, so one project's view doesn't bleed into another's.
    pub(super) fn reset_view(&mut self) {
        self.files_scroll = ScrollState::default();
        self.changes_scroll = ScrollState::default();
        self.preview_scroll = ScrollState::default();
        self.diff_scroll = ScrollState::default();
        self.preview_tab = None;
        self.diff_tab = None;
        self.viewer_focus = None;
        self.clear_viewer_selection();
    }

    /// Begin a viewer text selection at window-pixel `(x, y)` (mapped against the last layout's
    /// geometry). Replaces any prior selection.
    pub(super) fn begin_viewer_selection(&mut self, x: f32, y: f32) {
        if let Some(pos) = self.viewer_pos_at(x, y) {
            self.viewer_sel = Some(ViewerSelection { anchor: pos, focus: pos });
            self.viewer_sel_dragging = true;
        }
    }

    /// Extend the in-progress selection so its focus tracks pointer `(x, y)`.
    pub(super) fn update_viewer_selection(&mut self, x: f32, y: f32) {
        if !self.viewer_sel_dragging {
            return;
        }
        if let (Some(pos), Some(sel)) = (self.viewer_pos_at(x, y), self.viewer_sel.as_mut()) {
            sel.focus = pos;
        }
    }

    /// End an in-progress selection drag, returning whether one was active (the selection itself
    /// is kept for copying).
    pub(super) fn end_viewer_selection_drag(&mut self) -> bool {
        std::mem::take(&mut self.viewer_sel_dragging)
    }

    pub(super) fn is_dragging_viewer_selection(&self) -> bool {
        self.viewer_sel_dragging
    }

    pub(super) fn clear_viewer_selection(&mut self) {
        self.viewer_sel = None;
        self.viewer_sel_dragging = false;
    }

    /// Map a window pixel to a content `(row, col)`, clamped to the laid-out rows and the target
    /// row's display width. `None` when no viewer was laid out.
    fn viewer_pos_at(&self, x: f32, y: f32) -> Option<ViewerPos> {
        let g = self.viewer_geom.as_ref()?;
        if self.viewer_lines.is_empty() {
            return None;
        }
        let rel_row = ((y - g.content_y) / g.line_h).max(0.) as usize;
        let row = (g.first + rel_row).min(self.viewer_lines.len() - 1);
        let width = str_width(&self.viewer_lines[row]);
        // Round to the nearest cell boundary so the caret sits between glyphs, like a text editor.
        let col = (((x - g.text_x) / g.cw).round().max(0.) as usize).min(width);
        Some(ViewerPos { row, col })
    }

    /// The selected viewer text, or `None` when no viewer is focused or the selection is empty.
    /// Uses terminal-style line slicing (first row from its column to the line end, whole middle
    /// rows, last row up to its column), trimming trailing whitespace per line.
    pub(super) fn viewer_selection_text(&self) -> Option<String> {
        self.viewer_focus?;
        let sel = self.viewer_sel?;
        if sel.is_empty() || self.viewer_lines.is_empty() {
            return None;
        }
        let (start, end) = sel.ordered();
        let last = self.viewer_lines.len() - 1;
        let mut out = String::new();
        for row in start.row.min(last)..=end.row.min(last) {
            let line = &self.viewer_lines[row];
            let width = str_width(line);
            let c0 = if row == start.row { start.col } else { 0 };
            let c1 = if row == end.row { end.col } else { width };
            if row != start.row {
                out.push('\n');
            }
            out.push_str(slice_cols(line, c0, c1).trim_end());
        }
        Some(out).filter(|text| !text.is_empty())
    }

    /// The selection's `[c0, c1)` column range on `row`, or `None` when the row is outside the
    /// selection. The first row runs to `width` (its content end), middle rows span the full line.
    fn selection_cols(&self, row: usize, width: usize) -> Option<(usize, usize)> {
        let sel = self.viewer_sel.filter(|s| !s.is_empty())?;
        let (start, end) = sel.ordered();
        if row < start.row || row > end.row {
            return None;
        }
        let c0 = if row == start.row { start.col } else { 0 }.min(width);
        let c1 = if row == end.row { end.col } else { width }.min(width).max(c0);
        Some((c0, c1))
    }

    /// Path of the directory row registered at `index` by the last layout.
    pub(super) fn dir_path(&self, index: usize) -> Option<&str> {
        self.dir_rows.iter().find(|(i, _)| *i == index).map(|(_, p)| p.as_str())
    }

    /// Path of the plain-file row registered at `index` by the last layout.
    pub(super) fn file_path(&self, index: usize) -> Option<&str> {
        self.file_rows.iter().find(|(i, _)| *i == index).map(|(_, p)| p.as_str())
    }

    /// Path of the changed-file row registered at `index` by the last layout.
    pub(super) fn change_path(&self, index: usize) -> Option<&str> {
        self.change_rows.iter().find(|(i, _)| *i == index).map(|(_, p)| p.as_str())
    }
}

/// One lazily-loaded node of the file tree.
struct FileNode {
    /// File or directory name (the path's last segment).
    name: String,
    /// Path relative to the project root, `/`-separated to match git output.
    rel_path: String,
    is_dir: bool,
    /// Whether the entry is gitignored (rendered dimmed).
    ignored: bool,
    expanded: bool,
    children_loaded: bool,
    children: Vec<FileNode>,
}

/// Content of one file loaded for the center viewer.
pub struct FilePreview {
    /// Root-relative path the content belongs to.
    path: String,
    /// Syntax-highlighted content lines, capped at [`MAX_PREVIEW_LINES`].
    lines: Vec<SpanLine>,
    /// Whether the file had more lines than the cap.
    truncated: bool,
    /// Status note shown instead of content (unreadable / binary / empty file).
    note: Option<&'static str>,
}

/// Cached pane data for one project, fed by the background git worker and lazy `read_dir` calls.
pub struct ProjectPaneData {
    /// Project root the data describes. The pane is hidden for root-less projects.
    root: Option<PathBuf>,
    /// Whether the root is inside a git repository (from the last refresh).
    is_repo: bool,
    /// Whether a first git status result has arrived.
    loaded: bool,
    /// Changed files from the last `git status`.
    changed: Vec<ChangedFile>,
    /// Gitignored paths (relative, `/`-separated, collapsed dirs with a trailing slash).
    ignored: HashSet<String>,
    /// Parsed diffs keyed by the changed file's relative path.
    diffs: HashMap<String, Vec<DiffLine>>,
    /// Difftastic renderings keyed by `(relative path, side-by-side)`.
    difft: HashMap<(String, bool), Vec<SpanLine>>,
    /// Whether running `difft` failed (not installed); difft mode then falls back to the
    /// builtin diff with a note. Cleared when a difftastic result arrives or the mode is
    /// re-enabled (so toggling retries the tool).
    difft_missing: bool,
    /// Top-level file tree nodes (lazily expanded).
    tree: Vec<FileNode>,
    /// File content loaded for the center viewer, if any.
    preview: Option<FilePreview>,
}

impl ProjectPaneData {
    pub fn new(root: Option<PathBuf>) -> Self {
        Self {
            root,
            is_repo: false,
            loaded: false,
            changed: Vec::new(),
            ignored: HashSet::new(),
            diffs: HashMap::new(),
            difft: HashMap::new(),
            difft_missing: false,
            tree: Vec::new(),
            preview: None,
        }
    }

    /// Apply a git status result: replace the Changes list and the gitignored set, drop the (now
    /// possibly stale) diff cache, and re-list the already-expanded directories so new files show
    /// up. The diff of `keep_diff` (the file currently shown in the viewer) is retained so the
    /// view doesn't flash "loading" and lose its scroll position on every periodic refresh; the
    /// caller re-requests it and the fresh result replaces the entry in place.
    pub fn apply_status(
        &mut self,
        changed: Vec<ChangedFile>,
        ignored: Vec<String>,
        is_repo: bool,
        keep_diff: Option<&str>,
    ) {
        self.changed = changed;
        self.ignored = ignored.into_iter().map(|p| p.replace('\\', "/")).collect();
        self.is_repo = is_repo;
        self.loaded = true;
        self.diffs.retain(|path, _| Some(path.as_str()) == keep_diff);
        self.difft.retain(|(path, _), _| Some(path.as_str()) == keep_diff);
        self.refresh_tree();
    }

    /// Store the parsed diff for `file`.
    pub fn apply_diff(&mut self, file: String, lines: Vec<DiffLine>) {
        self.diffs.insert(file, lines);
    }

    /// Store a difftastic result; `None` marks the tool as unavailable.
    pub fn apply_difft(&mut self, file: String, split: bool, lines: Option<Vec<SpanLine>>) {
        match lines {
            Some(lines) => {
                self.difft_missing = false;
                self.difft.insert((file, split), lines);
            },
            None => self.difft_missing = true,
        }
    }

    /// Whether a difftastic rendering for `(path, split)` is cached.
    pub fn has_difft(&self, path: &str, split: bool) -> bool {
        self.difft_for(path, split).is_some()
    }

    /// The cached difftastic rendering for `(path, split)`, if any.
    fn difft_for(&self, path: &str, split: bool) -> Option<&Vec<SpanLine>> {
        self.difft.iter().find(|((p, s), _)| p == path && *s == split).map(|(_, lines)| lines)
    }

    /// Whether running `difft` failed (tool not installed).
    pub fn difft_missing(&self) -> bool {
        self.difft_missing
    }

    /// Forget a recorded `difft` failure so the next request retries the tool.
    pub fn clear_difft_missing(&mut self) {
        self.difft_missing = false;
    }

    pub fn changed_by_path(&self, path: &str) -> Option<&ChangedFile> {
        self.changed.iter().find(|file| file.path == path)
    }

    pub fn has_diff(&self, path: &str) -> bool {
        self.diffs.contains_key(path)
    }

    /// Load `rel_path`'s content into the preview slot for the center viewer. Local reads are
    /// cheap enough to do synchronously on click, like the tree's `read_dir` listing. Markdown
    /// files render as formatted output (wrapped to `width` cells) unless `md_source` asks for
    /// the plain source view.
    pub fn load_preview(&mut self, rel_path: &str, width: usize, md_source: bool) {
        let Some(root) = &self.root else { return };

        let mut preview = FilePreview {
            path: rel_path.to_owned(),
            lines: Vec::new(),
            truncated: false,
            note: None,
        };

        match fs::read(root.join(rel_path)) {
            Err(_) => preview.note = Some("(无法读取文件)"),
            // NUL within the head of the file marks it binary, like git's heuristic.
            Ok(bytes) if bytes.iter().take(8192).any(|b| *b == 0) => {
                preview.note = Some("(二进制文件)");
            },
            Ok(bytes) if bytes.is_empty() => preview.note = Some("(空文件)"),
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                let (src, truncated) = highlight::cap_lines(&text, MAX_PREVIEW_LINES);
                preview.lines = if markdown::is_markdown(rel_path) && !md_source {
                    markdown::render(src, width)
                } else {
                    highlight::highlight_file(rel_path, src, MAX_PREVIEW_LINES).0
                };
                preview.truncated = truncated;
            },
        }

        self.preview = Some(preview);
    }

    /// The loaded preview, when it belongs to `path`.
    fn preview_for(&self, path: &str) -> Option<&FilePreview> {
        self.preview.as_ref().filter(|preview| preview.path == path)
    }

    /// Whether `rel_path` is gitignored, directly or via an ignored ancestor directory.
    fn is_ignored(&self, rel_path: &str, is_dir: bool) -> bool {
        if self.ignored.contains(rel_path) || (is_dir && self.ignored.contains(&format!("{rel_path}/")))
        {
            return true;
        }
        // Collapsed ignored directories are listed with a trailing slash; check the ancestors.
        let mut prefix = String::new();
        for segment in rel_path.split('/') {
            prefix.push_str(segment);
            prefix.push('/');
            if self.ignored.contains(&prefix) {
                return true;
            }
        }
        false
    }

    /// List one directory level, sorted directories-first then case-insensitively by name.
    fn list_dir(root: &Path, rel: &str, data: &Self) -> Vec<FileNode> {
        let dir = if rel.is_empty() { root.to_owned() } else { root.join(rel) };
        let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };

        let mut nodes: Vec<FileNode> = entries
            .flatten()
            .take(MAX_DIR_ENTRIES)
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name == ".git" {
                    return None;
                }
                let is_dir = entry.file_type().ok()?.is_dir();
                let rel_path =
                    if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
                let ignored = data.is_ignored(&rel_path, is_dir);
                Some(FileNode {
                    name,
                    rel_path,
                    is_dir,
                    ignored,
                    expanded: false,
                    children_loaded: false,
                    children: Vec::new(),
                })
            })
            .collect();

        nodes.sort_unstable_by(|a, b| {
            b.is_dir.cmp(&a.is_dir).then_with(|| {
                a.name.to_lowercase().cmp(&b.name.to_lowercase())
            })
        });
        nodes
    }

    /// (Re)build the tree: list the root level and every directory that was expanded before,
    /// preserving the expansion state.
    pub fn refresh_tree(&mut self) {
        let Some(root) = self.root.clone() else { return };
        let mut fresh = Self::list_dir(&root, "", self);
        let old = std::mem::take(&mut self.tree);
        Self::carry_expansion(&root, self, &mut fresh, &old);
        self.tree = fresh;
    }

    /// Recursively re-expand `fresh` nodes that were expanded in `old`, re-listing their children.
    fn carry_expansion(root: &Path, data: &Self, fresh: &mut [FileNode], old: &[FileNode]) {
        for node in fresh.iter_mut().filter(|n| n.is_dir) {
            let Some(prev) = old.iter().find(|o| o.is_dir && o.rel_path == node.rel_path) else {
                continue;
            };
            if prev.expanded {
                node.expanded = true;
                node.children = Self::list_dir(root, &node.rel_path, data);
                node.children_loaded = true;
                Self::carry_expansion(root, data, &mut node.children, &prev.children);
            }
        }
    }

    /// Toggle the directory `rel_path`, lazily listing its children on first expansion.
    pub fn toggle_dir(&mut self, rel_path: &str) {
        let Some(root) = self.root.clone() else { return };

        // Two phases to satisfy the borrow checker: listing children needs `&self` (the ignored
        // set), so do it before borrowing the node mutably.
        let needs_children = matches!(
            Self::node(&self.tree, rel_path),
            Some(node) if node.is_dir && !node.expanded && !node.children_loaded
        );
        let children = needs_children.then(|| Self::list_dir(&root, rel_path, self));

        if let Some(node) = Self::node_mut(&mut self.tree, rel_path) {
            if !node.is_dir {
                return;
            }
            node.expanded = !node.expanded;
            if let Some(children) = children {
                node.children = children;
                node.children_loaded = true;
            }
        }
    }

    /// Whether `child` is `parent` itself or lies beneath the directory `parent`.
    fn is_under(parent: &str, child: &str) -> bool {
        child.len() > parent.len()
            && child.starts_with(parent)
            && child.as_bytes().get(parent.len()) == Some(&b'/')
    }

    /// Find the node with `rel_path`, descending through its ancestors.
    fn node<'n>(nodes: &'n [FileNode], rel_path: &str) -> Option<&'n FileNode> {
        for node in nodes {
            if node.rel_path == rel_path {
                return Some(node);
            }
            if node.is_dir && Self::is_under(&node.rel_path, rel_path) {
                return Self::node(&node.children, rel_path);
            }
        }
        None
    }

    /// Like [`Self::node`], but mutable.
    fn node_mut<'n>(nodes: &'n mut [FileNode], rel_path: &str) -> Option<&'n mut FileNode> {
        for node in nodes {
            if node.rel_path == rel_path {
                return Some(node);
            }
            if node.is_dir && Self::is_under(&node.rel_path, rel_path) {
                return Self::node_mut(&mut node.children, rel_path);
            }
        }
        None
    }
}

/// One flattened, visible file-tree row.
struct TreeRow<'d> {
    node: &'d FileNode,
    depth: usize,
}

fn flatten_tree<'d>(nodes: &'d [FileNode], depth: usize, out: &mut Vec<TreeRow<'d>>) {
    for node in nodes {
        out.push(TreeRow { node, depth });
        if node.is_dir && node.expanded {
            flatten_tree(&node.children, depth + 1, out);
        }
    }
}

/// Lay out the project pane within `[win_w - pane_w, win_w] × [bar_h, win_h - status_h]`,
/// emitting into `draw` and registering hit regions. Mirrors `Chrome::layout_sidebar`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn layout(
    state: &mut ProjectPaneState,
    data: &ProjectPaneData,
    hover: Option<Hit>,
    hits: &mut Vec<(PixelRect, Hit)>,
    draw: &mut ChromeDraw,
    cw: f32,
    ch: f32,
    win_w: f32,
    win_h: f32,
    bar_h: f32,
    status_h: f32,
    pane_w: f32,
    pad_x: f32,
    row_h: f32,
) {
    let x0 = win_w - pane_w;
    let y0 = bar_h;
    let y1 = win_h - status_h;

    // The row-path snapshots are rebuilt together with the hit regions they resolve.
    state.dir_rows.clear();
    state.file_rows.clear();
    state.change_rows.clear();

    // Background strip and left border.
    draw.rects.push(rect(x0, y0, pane_w, y1 - y0, chrome::bar_bg()));
    draw.rects.push(rect(x0, y0, 1., y1 - y0, chrome::border()));

    // Header: the "文件" / "更改 N" tab buttons.
    let changes_label = if data.changed.is_empty() {
        "更改".to_owned()
    } else {
        format!("更改 {}", data.changed.len())
    };
    let header_base = baseline(y0, row_h, ch);
    let hl_margin = ((row_h - ch) * 0.3).max(2.);
    let hl_h = row_h - 2. * hl_margin;
    let mut x = x0 + pad_x;
    for (label, tab, hit) in [
        ("文件", PaneTab::Files, Hit::PaneShowFiles),
        (changes_label.as_str(), PaneTab::Changes, Hit::PaneShowChanges),
    ] {
        let label_w = str_width(label) as f32 * cw;
        let region = PixelRect { x: x - pad_x * 0.5, y: y0, w: label_w + pad_x, h: row_h };
        if state.tab == tab {
            draw.rects.push(rect(region.x, y0 + hl_margin, region.w, hl_h, chrome::accent()));
        } else if hover == Some(hit) {
            draw.rects.push(rect(region.x, y0 + hl_margin, region.w, hl_h, chrome::hover_bg()));
        }
        let fg = if state.tab == tab { chrome::fg() } else { chrome::dim() };
        push_text_px(&mut draw.cells, x, header_base, label, fg, cw);
        hits.push((region, hit));
        x += label_w + pad_x * 1.5;
    }

    // Refresh affordance at the header's right edge: re-reads the directory tree and git status so
    // files added/removed on disk and new changes show up.
    let refresh = "刷新";
    let refresh_w = str_width(refresh) as f32 * cw;
    let refresh_x = x0 + pane_w - pad_x - refresh_w;
    let refresh_region =
        PixelRect { x: refresh_x - pad_x * 0.5, y: y0, w: refresh_w + pad_x, h: row_h };
    if hover == Some(Hit::PaneRefresh) {
        draw.rects.push(rect(refresh_region.x, y0 + hl_margin, refresh_region.w, hl_h,
            chrome::hover_bg()));
    }
    push_text_px(&mut draw.cells, refresh_x, header_base, refresh, chrome::dim(), cw);
    hits.push((refresh_region, Hit::PaneRefresh));

    draw.rects.push(rect(x0, y0 + row_h, pane_w, 1., chrome::border()));

    let content_y = y0 + row_h + 1.;
    match state.tab {
        PaneTab::Files => layout_files(state, data, hover, hits, draw, cw, ch, x0, content_y, y1,
            pane_w, pad_x),
        PaneTab::Changes => layout_changes(state, data, hover, hits, draw, cw, ch, x0, content_y,
            y1, pane_w, pad_x),
    }
}

/// Draw a thin scrollbar thumb along the pane's right edge when the list overflows.
fn push_scrollbar(
    draw: &mut ChromeDraw,
    pane_right: f32,
    y0: f32,
    y1: f32,
    first: usize,
    visible: usize,
    total: usize,
) {
    if total <= visible || total == 0 {
        return;
    }
    let track_h = y1 - y0;
    let thumb_h = (track_h * visible as f32 / total as f32).max(16.).min(track_h);
    let max_first = total - visible;
    let progress = (first as f32 / max_first as f32).clamp(0., 1.);
    let thumb_y = y0 + (track_h - thumb_h) * progress;
    draw.rects.push(RenderRect::new(pane_right - 4., thumb_y, 3., thumb_h, chrome::dim(), 0.45));
}

/// A dim, non-interactive note row (loading / empty states).
#[allow(clippy::too_many_arguments)]
fn push_note(draw: &mut ChromeDraw, text: &str, x: f32, y: f32, row_h: f32, ch: f32, cw: f32) {
    push_text_px(&mut draw.cells, x, baseline(y, row_h, ch), text, chrome::dim(), cw);
}

/// Push `label` (bright) truncated against `right_edge`, reserving room for an optional dim
/// `suffix` appended one cell after it (the `+a −d` tally). The suffix is dropped when it doesn't
/// fit.
fn push_label_suffix(
    draw: &mut ChromeDraw,
    text_x: f32,
    base: f32,
    right_edge: f32,
    label: &str,
    suffix: Option<&str>,
    cw: f32,
) {
    let suffix_w = suffix.map(str_width).unwrap_or(0);
    let budget = ((right_edge - text_x) / cw).floor().max(1.) as usize;
    let label = truncate(label, budget.saturating_sub(suffix_w + 1).max(4));
    let end = push_text_px(&mut draw.cells, text_x, base, &label, chrome::fg(), cw);
    if let Some(suffix) = suffix {
        if suffix_w > 0 && str_width(&label) + suffix_w < budget {
            push_text_px(&mut draw.cells, end + cw, base, suffix, chrome::dim(), cw);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn layout_files(
    state: &mut ProjectPaneState,
    data: &ProjectPaneData,
    hover: Option<Hit>,
    hits: &mut Vec<(PixelRect, Hit)>,
    draw: &mut ChromeDraw,
    cw: f32,
    ch: f32,
    x0: f32,
    y0: f32,
    y1: f32,
    pane_w: f32,
    pad_x: f32,
) {
    let row_h = ch * TREE_ROW_MULT;

    let mut rows = Vec::new();
    flatten_tree(&data.tree, 0, &mut rows);
    if rows.is_empty() {
        let note = if data.loaded { "(空目录)" } else { "加载中…" };
        push_note(draw, note, x0 + pad_x, y0, row_h, ch, cw);
        return;
    }

    let visible = ((y1 - y0) / row_h).floor().max(1.) as usize;
    state.files_scroll.clamp(rows.len(), visible.min(rows.len()));
    let first = state.files_scroll.first_row();

    let mut y = y0;
    for (i, row) in rows.iter().enumerate().skip(first) {
        if y + row_h > y1 {
            break;
        }
        let node = row.node;
        let indent = x0 + pad_x + row.depth as f32 * cw;

        // Both row kinds are clickable: directories expand/collapse, files open the viewer.
        let previewed =
            !node.is_dir && state.preview_tab.as_deref() == Some(node.rel_path.as_str());
        let region = PixelRect { x: x0, y, w: pane_w, h: row_h };
        if previewed {
            draw.rects.push(rect(x0 + 1., y, pane_w - 1., row_h, chrome::accent()));
        } else if hover == Some(Hit::PaneFileRow(i)) {
            draw.rects.push(rect(x0 + 1., y, pane_w - 1., row_h, chrome::hover_bg()));
        }
        hits.push((region, Hit::PaneFileRow(i)));
        if node.is_dir {
            state.dir_rows.push((i, node.rel_path.clone()));
        } else {
            state.file_rows.push((i, node.rel_path.clone()));
        }

        let base = baseline(y, row_h, ch);
        let mut text_x = indent;
        if node.is_dir {
            let arrow = if node.expanded { "▼" } else { "▶" };
            push_text_px(&mut draw.cells, text_x, base, arrow, chrome::dim(), cw);
        }
        text_x += cw * 1.5;

        let budget = (((x0 + pane_w - pad_x) - text_x) / cw).floor().max(1.) as usize;
        let label = truncate(&node.name, budget);
        let fg = if node.ignored { chrome::dim() } else { chrome::fg() };
        push_text_px(&mut draw.cells, text_x, base, &label, fg, cw);

        y += row_h;
    }

    push_scrollbar(draw, x0 + pane_w, y0, y1, first, visible, rows.len());
}

#[allow(clippy::too_many_arguments)]
fn layout_changes(
    state: &mut ProjectPaneState,
    data: &ProjectPaneData,
    hover: Option<Hit>,
    hits: &mut Vec<(PixelRect, Hit)>,
    draw: &mut ChromeDraw,
    cw: f32,
    ch: f32,
    x0: f32,
    y0: f32,
    y1: f32,
    pane_w: f32,
    pad_x: f32,
) {
    let row_h = ch * TREE_ROW_MULT;

    if !data.is_repo {
        let note = if data.loaded { "非 git 仓库" } else { "加载中…" };
        push_note(draw, note, x0 + pad_x, y0, row_h, ch, cw);
        return;
    }
    if data.changed.is_empty() {
        let note = if data.loaded { "无改动" } else { "加载中…" };
        push_note(draw, note, x0 + pad_x, y0, row_h, ch, cw);
        return;
    }

    let visible = ((y1 - y0) / row_h).floor().max(1.) as usize;
    state.changes_scroll.clamp(data.changed.len(), visible.min(data.changed.len()));
    let first = state.changes_scroll.first_row();

    let mut y = y0;
    for (index, file) in data.changed.iter().enumerate().skip(first) {
        if y + row_h > y1 {
            break;
        }
        let selected = state.diff_tab.as_deref() == Some(file.path.as_str());
        let region = PixelRect { x: x0, y, w: pane_w, h: row_h };
        if selected {
            draw.rects.push(rect(x0 + 1., y, pane_w - 1., row_h, chrome::accent()));
        } else if hover == Some(Hit::PaneChangeRow(index)) {
            draw.rects.push(rect(x0 + 1., y, pane_w - 1., row_h, chrome::hover_bg()));
        }
        hits.push((region, Hit::PaneChangeRow(index)));
        state.change_rows.push((index, file.path.clone()));

        let base = baseline(y, row_h, ch);
        push_text_px(
            &mut draw.cells,
            x0 + pad_x,
            base,
            &file.status.letter().to_string(),
            status_color(file.status),
            cw,
        );

        // Path, with the +adds -dels tally (when known) appended dimmed.
        let counts = file.counts.map(|(a, d)| format!("+{a} −{d}"));
        push_label_suffix(draw, x0 + pad_x + cw * 2., base, x0 + pane_w - pad_x,
            &file.path, counts.as_deref(), cw);
        y += row_h;
    }

    push_scrollbar(draw, x0 + pane_w, y0, y1, first, visible, data.changed.len());
}

/// One renderable row of the center viewer.
enum ViewerRow<'d> {
    /// Styled content line (file preview, rendered markdown, difftastic output).
    Text(&'d SpanLine),
    /// Colored full-width diff line (unified mode; hunk headers in both modes).
    Diff(&'d DiffLine),
    /// One side-by-side diff row: old file on the left, new file on the right (split mode).
    /// `None` leaves that side blank; the spans carry no `+`/`-` marker.
    Split {
        left: Option<(&'d SpanLine, DiffLineKind)>,
        right: Option<(&'d SpanLine, DiffLineKind)>,
    },
    /// Dim status line (loading / binary / empty / truncated).
    Note(&'d str),
}

/// Flatten a unified diff into side-by-side rows: within a change block, deletions pair up with
/// the additions that follow them row by row (leftovers keep their side, the other stays blank);
/// context lines occupy both sides and hunk headers span the full width.
fn build_split_rows<'d>(lines: &'d [DiffLine], rows: &mut Vec<ViewerRow<'d>>) {
    let mut pending: VecDeque<&'d DiffLine> = VecDeque::new();
    fn flush<'d>(pending: &mut VecDeque<&'d DiffLine>, rows: &mut Vec<ViewerRow<'d>>) {
        for del in pending.drain(..) {
            rows.push(ViewerRow::Split {
                left: Some((&del.spans, DiffLineKind::Del)),
                right: None,
            });
        }
    }

    for line in lines {
        match line.kind {
            DiffLineKind::Hunk => {
                flush(&mut pending, rows);
                rows.push(ViewerRow::Diff(line));
            },
            DiffLineKind::Del => pending.push_back(line),
            DiffLineKind::Add => {
                let left = pending
                    .pop_front()
                    .map(|del| (&del.spans, DiffLineKind::Del));
                rows.push(ViewerRow::Split {
                    left,
                    right: Some((&line.spans, DiffLineKind::Add)),
                });
            },
            DiffLineKind::Context => {
                flush(&mut pending, rows);
                rows.push(ViewerRow::Split {
                    left: Some((&line.spans, DiffLineKind::Context)),
                    right: Some((&line.spans, DiffLineKind::Context)),
                });
            },
        }
    }
    flush(&mut pending, rows);
}

/// Emit one styled span line at `(x, base)`, truncated to `budget` display cells.
fn push_spans(draw: &mut ChromeDraw, spans: &SpanLine, x: f32, base: f32, budget: usize, cw: f32) {
    let mut x = x;
    let mut budget = budget;
    for span in spans {
        if budget == 0 {
            break;
        }
        let span_w = str_width(&span.text);
        if span_w <= budget {
            x = push_text_px_styled(&mut draw.cells, x, base, &span.text, span.fg, cw, span.bold,
                span.italic);
            budget -= span_w;
        } else {
            push_text_px_styled(&mut draw.cells, x, base, &truncate(&span.text, budget), span.fg,
                cw, span.bold, span.italic);
            budget = 0;
        }
    }
}

/// The concatenated plain text of a styled span line.
fn spans_text(spans: &SpanLine) -> String {
    spans.iter().map(|span| span.text.as_str()).collect()
}

/// The full plain text of one viewer row, laid out in the same display columns it is drawn at so a
/// selection column maps to the same glyph. `right_col` is where a split row's right side begins.
/// Diff body lines carry their `+`/`-`/space gutter (two columns) so a copied diff reads as one.
fn row_text(row: &ViewerRow<'_>, right_col: usize) -> String {
    match row {
        ViewerRow::Text(spans) => spans_text(spans),
        ViewerRow::Diff(line) => match line.kind {
            DiffLineKind::Hunk => line.text.clone(),
            DiffLineKind::Add => format!("+ {}", spans_text(&line.spans)),
            DiffLineKind::Del => format!("- {}", spans_text(&line.spans)),
            _ => format!("  {}", spans_text(&line.spans)),
        },
        ViewerRow::Split { left, right } => {
            let l = left.map(|(spans, _)| spans_text(spans)).unwrap_or_default();
            let r = right.map(|(spans, _)| spans_text(spans)).unwrap_or_default();
            if r.is_empty() {
                return l;
            }
            // Pad the gap between the two sides so the right text lands on `right_col`.
            let mut out = l;
            let pad = right_col.saturating_sub(str_width(&out)).max(1);
            out.push_str(&" ".repeat(pad));
            out.push_str(&r);
            out
        },
        ViewerRow::Note(note) => note.to_string(),
    }
}

/// The substring of `line` spanning display columns `[c0, c1)`. A wide glyph is included when its
/// starting column falls in range.
fn slice_cols(line: &str, c0: usize, c1: usize) -> String {
    let mut col = 0;
    let mut out = String::new();
    for ch in line.chars() {
        if col >= c1 {
            break;
        }
        if col >= c0 {
            out.push(ch);
        }
        col += ch.width().unwrap_or(0);
    }
    out
}

/// Lay out the center viewer over the terminal area `[x0, x1] × [y0, y1]` when one of the
/// shared viewer tabs is focused, showing its file preview or diff. The whole region is
/// registered as a hit so clicks never fall through to the terminal beneath.
#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_viewer(
    state: &mut ProjectPaneState,
    data: &ProjectPaneData,
    hover: Option<Hit>,
    hits: &mut Vec<(PixelRect, Hit)>,
    draw: &mut ChromeDraw,
    cw: f32,
    ch: f32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    pad_x: f32,
    row_h: f32,
) {
    // Cleared up front so a not-focused / too-small frame leaves no stale geometry for the
    // pointer mapping to hit.
    state.viewer_geom = None;

    let Some(viewer) = state.focused_content() else { return };
    let w = x1 - x0;
    if w < cw * 8. || y1 - y0 < row_h * 2. {
        return;
    }

    // Opaque surface over the terminal, swallowing every click that hits no inner control.
    draw.rects.push(rect(x0, y0, w, y1 - y0, chrome::menu_bg()));
    hits.push((PixelRect { x: x0, y: y0, w, h: y1 - y0 }, Hit::ViewerBackground));

    // Header: dim kind tag, the file's path, diff-mode buttons (diffs only) and a close button
    // at the right edge.
    let base = baseline(y0, row_h, ch);
    let hl_margin = ((row_h - ch) * 0.3).max(2.);
    let hl_h = row_h - 2. * hl_margin;
    let (tag, path) = match &viewer {
        ViewerContent::File(path) => ("预览", path),
        ViewerContent::Diff(path) => ("Diff", path),
    };
    let close_x = x1 - pad_x - cw;
    let close_region = PixelRect { x: close_x - pad_x * 0.5, y: y0, w: cw + pad_x, h: row_h };
    if hover == Some(Hit::ViewerClose) {
        draw.rects.push(rect(close_region.x, y0, close_region.w, row_h, chrome::hover_bg()));
    }
    push_text_px(&mut draw.cells, close_x, base, "×", chrome::dim(), cw);
    hits.push((close_region, Hit::ViewerClose));

    // The header's mode toggles, laid out right-to-left before the close button: diff layout /
    // engine for diffs, the rendered-vs-source switch for markdown previews.
    let mut buttons: Vec<(&str, bool, Hit)> = Vec::new();
    match &viewer {
        ViewerContent::Diff(_) => buttons.extend([
            ("分栏", state.diff_split, Hit::ViewerSplit),
            ("行间", !state.diff_split, Hit::ViewerUnified),
            ("difft", state.difft_mode, Hit::ViewerDifft),
        ]),
        ViewerContent::File(path) if markdown::is_markdown(path) => {
            buttons.push(("源码", state.md_source, Hit::ViewerMdSource));
        },
        ViewerContent::File(_) => {},
    }
    let mut right_edge = close_region.x;
    {
        for (label, active, hit) in buttons {
            let label_w = str_width(label) as f32 * cw;
            let region =
                PixelRect { x: right_edge - label_w - pad_x, y: y0, w: label_w + pad_x, h: row_h };
            if active {
                draw.rects.push(rect(region.x, y0 + hl_margin, region.w, hl_h, chrome::accent()));
            } else if hover == Some(hit) {
                draw.rects.push(rect(region.x, y0 + hl_margin, region.w, hl_h, chrome::hover_bg()));
            }
            let fg = if active { chrome::fg() } else { chrome::dim() };
            push_text_px(&mut draw.cells, region.x + pad_x * 0.5, base, label, fg, cw);
            hits.push((region, hit));
            right_edge = region.x - pad_x * 0.5;
        }
    }

    let tag_end = push_text_px(&mut draw.cells, x0 + pad_x, base, tag, chrome::dim(), cw);
    let path_x = tag_end + cw;
    let budget = ((right_edge - pad_x * 0.5 - path_x) / cw).floor().max(1.) as usize;
    push_text_px(&mut draw.cells, path_x, base, &truncate(path, budget), chrome::fg(), cw);
    draw.rects.push(rect(x0, y0 + row_h, w, 1., chrome::border()));

    // Flatten the content into rows (one dim note row for the special states).
    let mut rows: Vec<ViewerRow<'_>> = Vec::new();
    match &viewer {
        ViewerContent::File(path) => match data.preview_for(path) {
            Some(preview) => match preview.note {
                Some(note) => rows.push(ViewerRow::Note(note)),
                None => {
                    rows.extend(preview.lines.iter().map(ViewerRow::Text));
                    if preview.truncated {
                        rows.push(ViewerRow::Note("… (已截断)"));
                    }
                },
            },
            None => rows.push(ViewerRow::Note("加载中…")),
        },
        ViewerContent::Diff(path) => {
            if state.difft_mode && !data.difft_missing {
                match data.difft_for(path, state.diff_split) {
                    Some(lines) if lines.is_empty() => {
                        rows.push(ViewerRow::Note("(无差异内容)"));
                    },
                    Some(lines) => rows.extend(lines.iter().map(ViewerRow::Text)),
                    None => rows.push(ViewerRow::Note("difft 运行中…")),
                }
            } else {
                if state.difft_mode {
                    rows.push(ViewerRow::Note("(未找到 difft,已回退内置 diff)"));
                }
                match data.diffs.get(path) {
                    Some(lines) if lines.is_empty() => {
                        rows.push(ViewerRow::Note("(无差异内容)"));
                    },
                    Some(lines) if state.diff_split => build_split_rows(lines, &mut rows),
                    Some(lines) => rows.extend(lines.iter().map(ViewerRow::Diff)),
                    None => rows.push(ViewerRow::Note("加载中…")),
                }
            }
        },
    }

    let line_h = ch * DIFF_ROW_MULT;
    let content_y = y0 + row_h + 1.;
    let visible = ((y1 - content_y) / line_h).floor().max(1.) as usize;
    let scroll = match state.viewer_focus {
        Some(ViewerKind::Diff) => &mut state.diff_scroll,
        _ => &mut state.preview_scroll,
    };
    scroll.clamp(rows.len(), visible.min(rows.len()));
    let first = scroll.first_row();

    let text_x = x0 + pad_x;
    let text_budget = (((x1 - pad_x) - text_x) / cw).floor().max(1.) as usize;
    // Split rows divide the content at the midline: old file left, new file right.
    let mid = (x0 + w * 0.5).floor();

    // Snapshot the geometry for pointer→cell mapping, and (re)build the per-row plain text the
    // selection indexes into. The text is keyed so a drag across a large file doesn't re-render it.
    state.viewer_geom = Some(ViewerGeom { text_x, content_y, line_h, cw, first });
    if let Some(kind) = state.viewer_focus {
        let key = ViewerLinesKey {
            kind,
            path: path.clone(),
            split: state.diff_split,
            difft: state.difft_mode,
            md_source: state.md_source,
            rows: rows.len(),
        };
        if state.viewer_lines_key.as_ref() != Some(&key) {
            let right_col = ((mid + 1. - x0) / cw).round().max(0.) as usize;
            state.viewer_lines = rows.iter().map(|row| row_text(row, right_col)).collect();
            state.viewer_lines_key = Some(key);
        }
    }

    let mut y = content_y;
    for (i, row) in rows.iter().enumerate().skip(first) {
        if y + line_h > y1 {
            break;
        }
        // The selection highlight for this row is computed now but pushed after the row's body
        // (below), so it layers over any diff tint while staying under the glyphs.
        let sel_cols = state
            .viewer_lines
            .get(i)
            .map(|line| str_width(line))
            .and_then(|width| state.selection_cols(i, width));

        let base = baseline(y, line_h, ch);
        match row {
            ViewerRow::Text(spans) => {
                push_spans(draw, spans, text_x, base, text_budget, cw);
            },
            ViewerRow::Diff(line) => {
                if line.kind == DiffLineKind::Hunk {
                    push_text_px(&mut draw.cells, text_x, base,
                        &truncate(&line.text, text_budget), hunk_fg(), cw);
                } else {
                    // Background tint + `+`/`-` marker carry the change kind; the body renders
                    // with its syntax colors.
                    let (marker, bg) = match line.kind {
                        DiffLineKind::Add => (Some(("+", add_fg())), Some(add_bg())),
                        DiffLineKind::Del => (Some(("-", del_fg())), Some(del_bg())),
                        _ => (None, None),
                    };
                    if let Some(bg) = bg {
                        draw.rects.push(rect(x0, y, w, line_h, bg));
                    }
                    if let Some((glyph, fg)) = marker {
                        push_text_px(&mut draw.cells, text_x, base, glyph, fg, cw);
                    }
                    // Body at column 2 (marker + one-cell gap), matching the "+ "/"- " gutter the
                    // selection text carries, so a copied diff line reads correctly.
                    let body_x = text_x + cw * 2.;
                    let budget = (((x1 - pad_x) - body_x) / cw).floor().max(1.) as usize;
                    push_spans(draw, &line.spans, body_x, base, budget, cw);
                }
            },
            ViewerRow::Split { left, right } => {
                // A change row tints the present side by its kind and the absent side with a muted
                // fill, so each modified line reads as old-on-left versus new-on-right.
                let is_change = matches!(left, Some((_, DiffLineKind::Add | DiffLineKind::Del)))
                    || matches!(right, Some((_, DiffLineKind::Add | DiffLineKind::Del)));
                for (side_x0, side_x1, side) in [(x0, mid, left), (mid + 1., x1, right)] {
                    let bg = match side {
                        Some((_, DiffLineKind::Add)) => Some(add_bg()),
                        Some((_, DiffLineKind::Del)) => Some(del_bg()),
                        None if is_change => Some(absent_bg()),
                        _ => None,
                    };
                    if let Some(bg) = bg {
                        draw.rects.push(rect(side_x0, y, side_x1 - side_x0, line_h, bg));
                    }
                    if let Some((spans, _)) = side {
                        let tx = side_x0 + pad_x;
                        let budget = (((side_x1 - pad_x * 0.5) - tx) / cw).floor().max(1.) as usize;
                        push_spans(draw, spans, tx, base, budget, cw);
                    }
                }
                // The midline divider, over the side tints.
                draw.rects.push(rect(mid, y, 1., line_h, chrome::border()));
            },
            ViewerRow::Note(note) => {
                push_note(draw, note, text_x, y, line_h, ch, cw);
            },
        }

        // Selection highlight: over the row's tint, under its glyphs (cells paint after all rects).
        if let Some((c0, c1)) = sel_cols {
            if c1 > c0 {
                let hx = (text_x + c0 as f32 * cw).max(x0);
                let hw = ((c1 - c0) as f32 * cw).min(x1 - hx).max(0.);
                if hw > 0. {
                    draw.rects.push(RenderRect::new(hx, y, hw, line_h, sel_bg(), SEL_ALPHA));
                }
            }
        }
        y += line_h;
    }

    push_scrollbar(draw, x1, content_y, y1, first, visible, rows.len());
}
