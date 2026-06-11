//! Native window chrome: a terminal-styled tab bar, project sidebar and right-click context menu
//! drawn with Kairos's own GL renderer (solid rects + cell-aligned text), replacing the
//! previous egui-based chrome.
//!
//! The chrome is laid out on a window-origin cell grid (the same `cell_width`/`cell_height` as the
//! terminal): the tab bar occupies the top row, the sidebar the leftmost columns. Each frame
//! [`Chrome::layout`] produces a list of [`RenderRect`]s (backgrounds, highlights, borders) in
//! absolute window pixels and a list of [`RenderableCell`]s (labels) in window-cell coordinates,
//! plus the hot regions used to hit-test mouse input.

use std::path::Path;

use unicode_width::UnicodeWidthChar;

use kairos_terminal::index::{Column, Point};
use kairos_terminal::term::cell::Flags;

use crate::display::color::Rgb;
use crate::display::content::RenderableCell;
use crate::renderer::rects::RenderRect;

use super::TabBarInfo;
use super::project_pane::{self, PaneTab, ProjectPaneData, ProjectPaneState, ViewerKind};

/// Fixed logical size (points) of the chrome UI font, like Zed's `ui_font_size`: the chrome is
/// fully decoupled from the terminal font and scales only with the window scale factor, so a large
/// terminal font never inflates the tab bar or sidebar. 10.5pt ≈ 14 logical px, Zed's tab/panel
/// label size.
pub const CHROME_FONT_PT: f32 = 10.5;

/// Default width (in chrome cells) of the project sidebar (~200 logical px, near Zed's panel).
const DEFAULT_SIDEBAR_COLS: f32 = 26.;
/// Clamp range (in chrome cells) the sidebar may be dragged to.
const MIN_SIDEBAR_COLS: f32 = 12.;
const MAX_SIDEBAR_COLS: f32 = 80.;
/// Default width (in chrome cells) of the right-side project pane.
const DEFAULT_PANE_COLS: f32 = 34.;
/// Clamp range (in chrome cells) the project pane may be dragged to.
const MIN_PANE_COLS: f32 = 16.;
const MAX_PANE_COLS: f32 = 100.;
/// Half-width (window pixels) of the grab zone straddling the sidebar's right divider.
const DIVIDER_GRAB_PX: f32 = 4.;
/// Clamp range (in chrome cells) of a tab segment's width: tabs auto-size to their label between
/// these bounds (the segment holds the label plus padding and the close glyph).
const MIN_TAB_COLS: f32 = 8.;
const MAX_TAB_COLS: f32 = 24.;
/// Width (in chrome cells) of the right-click context menu.
const MENU_COLS: usize = 14;

// Spacing, expressed as multiples of the chrome cell so it scales with the font.
/// Tab bar height = chrome cell height × this.
const BAR_H_MULT: f32 = 2.0;
/// Sidebar / menu row height = chrome cell height × this.
const ROW_H_MULT: f32 = 1.45;
/// Git status bar height = chrome cell height × this.
const STATUS_H_MULT: f32 = 1.35;
/// Horizontal inset before text = chrome cell width × this.
const PAD_X_MULT: f32 = 0.8;
/// Gap between adjacent tabs = chrome cell width × this.
const TAB_GAP_MULT: f32 = 1.1;

#[inline]
fn c(r: u8, g: u8, b: u8) -> Rgb {
    Rgb::new(r, g, b)
}

// zinc palette (shadcn-inspired), matching the previous egui theme. Shared with the project
// pane module, hence the `pub(crate)` visibility on the surface colors it reuses.
pub(crate) fn bar_bg() -> Rgb {
    c(0x0c, 0x0c, 0x0e)
}
pub(crate) fn menu_bg() -> Rgb {
    c(0x09, 0x09, 0x0b)
}
pub(crate) fn border() -> Rgb {
    c(0x27, 0x27, 0x2a)
}
/// Selected item background.
pub(crate) fn accent() -> Rgb {
    c(0x3f, 0x3f, 0x46)
}
/// Hovered item background.
pub(crate) fn hover_bg() -> Rgb {
    c(0x27, 0x27, 0x2a)
}
/// Primary foreground.
pub(crate) fn fg() -> Rgb {
    c(0xfa, 0xfa, 0xfa)
}
/// Muted foreground (headers, idle affordances).
pub(crate) fn dim() -> Rgb {
    c(0xa1, 0xa1, 0xaa)
}
/// Command-palette search-field background, a hair lighter than the menu surface.
fn field_bg() -> Rgb {
    c(0x18, 0x18, 0x1b)
}
/// macOS-style blue selection used for the highlighted palette row.
fn sel_bg() -> Rgb {
    c(0x2f, 0x6f, 0xed)
}

/// Font families offered by the settings panel, on top of whatever the config currently uses.
const FAMILY_PRESETS: &[&str] =
    &["Consolas", "Cascadia Code", "Cascadia Mono", "Courier New", "Lucida Console"];

/// Clamp range (points) for the settings panel's font size.
const MIN_FONT_PT: f32 = 6.;
const MAX_FONT_PT: f32 = 72.;

/// A command available in the `Ctrl+Shift+P` command palette.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PaletteCmd {
    Settings,
    NewTab,
    NewProject,
    ToggleSidebar,
    ToggleProjectPane,
    SplitRight,
    SplitDown,
    ClosePane,
    TogglePaneZoom,
    FocusNextPane,
}

/// The command palette's entries: `(label, search keywords, command)`. The keywords (lowercase,
/// including pinyin/English aliases) let the search box match a command without typing its glyphs.
pub const PALETTE_COMMANDS: &[(&str, &str, PaletteCmd)] = &[
    ("设置", "settings preferences config shezhi peizhi", PaletteCmd::Settings),
    ("新建标签页", "new tab xinjian biaoqianye", PaletteCmd::NewTab),
    ("新建项目", "new project folder xinjian xiangmu", PaletteCmd::NewProject),
    ("切换侧边栏", "toggle sidebar qiehuan cebianlan", PaletteCmd::ToggleSidebar),
    (
        "切换项目面板",
        "toggle project pane panel files changes qiehuan xiangmu mianban",
        PaletteCmd::ToggleProjectPane,
    ),
    ("向右分屏", "split right pane fenping xiangyou", PaletteCmd::SplitRight),
    ("向下分屏", "split down pane fenping xiangxia", PaletteCmd::SplitDown),
    ("关闭当前 Pane", "close pane guanbi dangqian", PaletteCmd::ClosePane),
    (
        "最大化/还原 Pane",
        "zoom maximize restore pane zuidahua huanyuan",
        PaletteCmd::TogglePaneZoom,
    ),
    (
        "焦点切换到下一个 Pane",
        "focus next pane jiaodian qiehuan xiayige",
        PaletteCmd::FocusNextPane,
    ),
];

/// State of the open command palette: the committed query, the in-flight IME composition
/// (`preedit`), and the highlighted row (an index into the *filtered* results, not into
/// [`PALETTE_COMMANDS`]).
#[derive(Default)]
pub struct PaletteState {
    query: String,
    /// In-progress IME composition (e.g. pinyin being typed), shown after the query but not yet
    /// committed. Included in the filter so results update live while composing.
    preedit: String,
    selected: usize,
}

impl PaletteState {
    /// The committed query plus any in-flight composition — what the filter matches against.
    fn effective(&self) -> String {
        let mut s = self.query.clone();
        s.push_str(&self.preedit);
        s
    }

    /// Indices into [`PALETTE_COMMANDS`] matching the current text (case-insensitive substring on
    /// either the label or the keywords). Empty text matches everything.
    fn filtered(&self) -> Vec<usize> {
        let q = self.effective().trim().to_lowercase();
        PALETTE_COMMANDS
            .iter()
            .enumerate()
            .filter(|(_, (label, keys, _))| {
                q.is_empty() || label.to_lowercase().contains(&q) || keys.contains(q.as_str())
            })
            .map(|(i, _)| i)
            .collect()
    }
}

/// A selectable shell in the settings panel.
#[derive(Clone)]
pub struct ShellPreset {
    pub label: String,
    pub program: String,
    pub args: Vec<String>,
}

/// Which settings dropdown is currently expanded.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingsDropdown {
    Family,
    Shell,
}

/// Editable state backing the settings panel. Built from the live config when the panel opens and
/// mutated in place as the user clicks; the window applies and persists each snapshot.
#[derive(Clone)]
pub struct SettingsState {
    pub families: Vec<String>,
    pub family_idx: usize,
    pub size_pt: f32,
    pub shells: Vec<ShellPreset>,
    pub shell_idx: usize,
    /// The expanded dropdown, if any.
    dropdown: Option<SettingsDropdown>,
    /// Text being edited in the font-size input box (only meaningful while it has focus).
    size_text: String,
    /// Whether the font-size input box has keyboard focus.
    size_focus: bool,
}

impl SettingsState {
    /// Build panel state from current config values. `current_family`/`current_program` come from
    /// the live `UiConfig`; the shell list is probed on this machine.
    pub fn build(current_family: &str, size_pt: f32, current_program: Option<&str>) -> Self {
        let mut families: Vec<String> = FAMILY_PRESETS.iter().map(|s| (*s).to_owned()).collect();
        if !families.iter().any(|f| f.eq_ignore_ascii_case(current_family)) {
            families.insert(0, current_family.to_owned());
        }
        let family_idx =
            families.iter().position(|f| f.eq_ignore_ascii_case(current_family)).unwrap_or(0);

        let shells = available_shells();
        let shell_idx = current_program
            .and_then(|p| shells.iter().position(|s| program_matches(&s.program, p)))
            .unwrap_or(0);

        Self {
            families,
            family_idx,
            size_pt,
            shells,
            shell_idx,
            dropdown: None,
            size_text: String::new(),
            size_focus: false,
        }
    }

    pub fn family(&self) -> &str {
        self.families.get(self.family_idx).map(String::as_str).unwrap_or_default()
    }

    pub fn shell(&self) -> Option<&ShellPreset> {
        self.shells.get(self.shell_idx)
    }

    /// Commit the size input box: parse its text and apply it (clamped); invalid text reverts to
    /// the previous size. Always clears focus. Returns whether the committed size differs.
    fn commit_size(&mut self) -> bool {
        if !std::mem::take(&mut self.size_focus) {
            return false;
        }
        match self.size_text.trim().parse::<f32>() {
            Ok(pt) if pt.is_finite() => {
                let pt = pt.clamp(MIN_FONT_PT, MAX_FONT_PT);
                let changed = (pt - self.size_pt).abs() > f32::EPSILON;
                self.size_pt = pt;
                changed
            },
            _ => false,
        }
    }
}

/// Whether a configured shell `program` is the same as a preset's, comparing case-insensitively on
/// the file stem so "powershell" matches "powershell.exe" and a full path to bash matches "bash".
fn program_matches(preset_program: &str, configured: &str) -> bool {
    fn stem(s: &str) -> String {
        Path::new(s).file_stem().and_then(|s| s.to_str()).unwrap_or(s).to_ascii_lowercase()
    }
    stem(preset_program) == stem(configured)
}

/// Shells offered on this machine. cmd / PowerShell are always present on Windows; pwsh and Git
/// Bash are only listed when found, so selecting one never spawns a missing program.
#[cfg(windows)]
fn available_shells() -> Vec<ShellPreset> {
    let mut shells = vec![
        ShellPreset { label: "PowerShell".into(), program: "powershell".into(), args: Vec::new() },
        ShellPreset { label: "cmd".into(), program: "cmd".into(), args: Vec::new() },
    ];
    if let Some(pwsh) = find_windows_shell(&["PowerShell\\7\\pwsh.exe"]) {
        shells.push(ShellPreset { label: "pwsh".into(), program: pwsh, args: Vec::new() });
    }
    if let Some(bash) = find_windows_shell(&["Git\\bin\\bash.exe"]) {
        shells.push(ShellPreset {
            label: "Git Bash".into(),
            program: bash,
            args: vec!["--login".into(), "-i".into()],
        });
    }
    shells
}

#[cfg(not(windows))]
fn available_shells() -> Vec<ShellPreset> {
    vec![
        ShellPreset { label: "bash".into(), program: "bash".into(), args: Vec::new() },
        ShellPreset { label: "zsh".into(), program: "zsh".into(), args: Vec::new() },
    ]
}

/// Probe `%ProgramFiles%` (and the well-known 32/64-bit roots) for the first of `suffixes` that
/// exists, returning its full path.
#[cfg(windows)]
fn find_windows_shell(suffixes: &[&str]) -> Option<String> {
    let mut roots: Vec<String> = Vec::new();
    for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Ok(root) = std::env::var(var) {
            roots.push(root);
        }
    }
    roots.push("C:\\Program Files".to_owned());
    roots.push("C:\\Program Files (x86)".to_owned());
    for root in &roots {
        for suffix in suffixes {
            let path = format!("{root}\\{suffix}");
            if Path::new(&path).exists() {
                return Some(path);
            }
        }
    }
    None
}

/// A rectangle in window pixels, used for chrome hit regions and pane geometry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PixelRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl PixelRect {
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
}

/// An actionable target in the chrome, produced by hit-testing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    SelectTab(u64),
    CloseTab(u64),
    CreateTab,
    SelectProject(usize),
    CloseProject(usize),
    CreateProject,
    /// Resume the active project's Claude session at this index into `TabBarInfo::project_sessions`.
    OpenClaudeSession(usize),
    /// Switch the project pane to its Files tab.
    PaneShowFiles,
    /// Switch the project pane to its Changes tab.
    PaneShowChanges,
    /// A file-tree row (directory or file) at this index into the pane's flattened visible tree.
    PaneFileRow(usize),
    /// A changed-file row at this index into the pane's Changes list.
    PaneChangeRow(usize),
    /// A shared viewer tab (预览 / diff) in the tab bar; focuses it.
    ViewerTab(ViewerKind),
    /// A shared viewer tab's close button.
    ViewerTabClose(ViewerKind),
    /// The center viewer's header close button (closes the focused viewer tab).
    ViewerClose,
    /// Switch the center viewer's diff to the unified (行间) layout.
    ViewerUnified,
    /// Switch the center viewer's diff to the side-by-side (分栏) layout.
    ViewerSplit,
    /// Toggle rendering the center viewer's diff with difftastic.
    ViewerDifft,
    /// Toggle the markdown preview between rendered output and source text.
    ViewerMdSource,
    /// A click landing on the center viewer's body but not a control; consumed without action so
    /// it never reaches the terminal beneath.
    ViewerBackground,
    Copy,
    Paste,
    /// Activate the command-palette entry at this index into [`PALETTE_COMMANDS`].
    PaletteSelect(usize),
    /// A click landing on an open modal's body but not a control; consumed without action.
    ModalBackground,
    /// The font-family dropdown field; toggles its option list.
    SettingsFamilyField,
    /// Select the family at this index into [`SettingsState::families`] from the open dropdown.
    SettingsFamilyOption(usize),
    /// The font-size input box; clicking focuses it for keyboard editing.
    SettingsSizeField,
    /// The shell dropdown field; toggles its option list.
    SettingsShellField,
    /// Select the shell at this index into [`SettingsState::shells`] from the open dropdown.
    SettingsShellOption(usize),
    SettingsClose,
}

/// Draw lists produced for a single frame of chrome.
#[derive(Default)]
pub struct ChromeDraw {
    /// Backgrounds, highlights and borders, in absolute window pixels.
    pub rects: Vec<RenderRect>,
    /// Label glyphs, positioned on the window-origin chrome cell grid.
    pub cells: Vec<RenderableCell>,
    /// Rects of popups (e.g. open dropdown lists) painted after `cells`, so they cover text
    /// already emitted for the surface beneath them.
    pub overlay_rects: Vec<RenderRect>,
    /// Glyphs of popups, painted last of all.
    pub overlay_cells: Vec<RenderableCell>,
    /// Pixel height reserved at the top for the tab bar (0 when hidden).
    pub bar_height: f32,
    /// Pixel width reserved at the left for the sidebar (0 when hidden).
    pub sidebar_width: f32,
    /// Pixel height reserved at the bottom for the git status bar (0 when hidden).
    pub status_height: f32,
    /// Pixel width reserved at the right for the project pane (0 when hidden).
    pub pane_width: f32,
}

/// Native chrome state: visibility, the open context menu, the last mouse position and the hot
/// regions from the most recent layout.
pub struct Chrome {
    pub sidebar_visible: bool,
    /// Whether the right-side project pane is shown (for projects with a root folder).
    pub pane_visible: bool,
    /// Window-pixel position the right-click context menu was opened at, if any.
    pub context_menu: Option<(f32, f32)>,
    /// Last observed mouse position in window pixels.
    mouse: (f32, f32),
    /// Hot regions from the most recent [`Self::layout`], topmost last.
    hits: Vec<(PixelRect, Hit)>,
    /// Currently hovered hot region, used to paint a hover highlight.
    hover: Option<Hit>,
    /// Sidebar width (px) from the most recent layout, for region tests.
    sidebar_w: f32,
    /// Project pane width (px) from the most recent layout, for region tests.
    pane_w: f32,
    /// Tab bar height (px) from the most recent layout, for region tests.
    bar_h: f32,
    /// Git status bar height (px) from the most recent layout, for region tests.
    status_h: f32,
    /// Window height (px) from the most recent layout, anchoring the bottom status bar.
    win_h: f32,
    /// Window width (px) from the most recent layout, anchoring the right project pane.
    win_w: f32,
    /// Sidebar width in chrome cells; adjusted by dragging its right-edge divider.
    sidebar_cols: f32,
    /// Project pane width in chrome cells; adjusted by dragging its left-edge divider.
    pane_cols: f32,
    /// Whether the sidebar divider is currently being dragged.
    dragging_divider: bool,
    /// Whether the project pane divider is currently being dragged.
    dragging_pane_divider: bool,
    /// Transient UI state of the project pane (active tab, scrolls, expanded diff).
    pane: ProjectPaneState,
    /// State of the open command palette (search query + selection), if any.
    command_palette: Option<PaletteState>,
    /// Window-pixel caret box `(x, y, w, h)` of the palette search field from the last layout, used
    /// to anchor the IME candidate window. `None` when the palette isn't drawn.
    palette_ime_rect: Option<(f32, f32, f32, f32)>,
    /// Editable state of the open settings panel, if any.
    settings: Option<SettingsState>,
}

impl Chrome {
    pub fn new() -> Self {
        Self {
            sidebar_visible: true,
            pane_visible: true,
            context_menu: None,
            mouse: (0., 0.),
            hits: Vec::new(),
            hover: None,
            sidebar_w: 0.,
            pane_w: 0.,
            bar_h: 0.,
            status_h: 0.,
            win_h: 0.,
            win_w: 0.,
            sidebar_cols: DEFAULT_SIDEBAR_COLS,
            pane_cols: DEFAULT_PANE_COLS,
            dragging_divider: false,
            dragging_pane_divider: false,
            pane: ProjectPaneState::default(),
            command_palette: None,
            palette_ime_rect: None,
            settings: None,
        }
    }

    pub fn toggle_sidebar(&mut self) {
        self.sidebar_visible = !self.sidebar_visible;
    }

    pub fn toggle_pane(&mut self) {
        self.pane_visible = !self.pane_visible;
    }

    /// Switch the project pane to `tab`.
    pub fn pane_set_tab(&mut self, tab: PaneTab) {
        self.pane.tab = tab;
    }

    /// Open (or retarget) the shared 预览 tab to `path` and focus it.
    pub fn open_file_preview(&mut self, path: &str) {
        self.pane.open_viewer(ViewerKind::Preview, path);
    }

    /// Open (or retarget) the shared diff tab to the changed file `path` and focus it.
    pub fn open_diff_viewer(&mut self, path: &str) {
        self.pane.open_viewer(ViewerKind::Diff, path);
    }

    /// Focus the viewer tab of `kind`, when it is open.
    pub fn focus_viewer(&mut self, kind: ViewerKind) {
        self.pane.focus_viewer(kind);
    }

    /// Close the viewer tab of `kind` (focus falls back to the terminal when it was focused).
    pub fn close_viewer_tab(&mut self, kind: ViewerKind) {
        self.pane.close_viewer_tab(kind);
    }

    /// Close the focused viewer tab, returning whether one was focused.
    pub fn close_focused_viewer(&mut self) -> bool {
        match self.pane.viewer_focus {
            Some(kind) => {
                self.pane.close_viewer_tab(kind);
                true
            },
            None => false,
        }
    }

    /// Drop viewer focus back to the terminal (the tabs stay open), returning whether a viewer
    /// tab was focused.
    pub fn unfocus_viewer(&mut self) -> bool {
        self.pane.viewer_focus.take().is_some()
    }

    /// Relative path held by the shared diff tab, if it is open.
    pub fn pane_selected(&self) -> Option<&str> {
        self.pane.diff_tab.as_deref()
    }

    /// Whether a viewer tab is focused (the center area shows it instead of the terminal).
    pub fn viewer_open(&self) -> bool {
        self.pane.viewer_focus.is_some()
    }

    /// Whether `(x, y)` (window pixels) lies within the open center viewer (the terminal area
    /// between the chrome surfaces).
    pub fn viewer_contains(&self, x: f32, y: f32) -> bool {
        self.viewer_open()
            && x >= self.sidebar_w
            && x < self.win_w - self.pane_w
            && y >= self.bar_h
            && y < self.win_h - self.status_h
    }

    /// Scroll the focused viewer tab's content by `rows` (positive scrolls down).
    pub fn scroll_viewer(&mut self, rows: f32) {
        self.pane.scroll_viewer(rows);
    }

    /// Switch the center viewer's diff layout between unified and side-by-side.
    pub fn set_viewer_split(&mut self, split: bool) {
        self.pane.diff_split = split;
    }

    /// Whether the center viewer's diff uses the side-by-side layout.
    pub fn viewer_split(&self) -> bool {
        self.pane.diff_split
    }

    /// Toggle difftastic rendering for the center viewer's diff, returning the new state.
    pub fn toggle_viewer_difft(&mut self) -> bool {
        self.pane.difft_mode = !self.pane.difft_mode;
        self.pane.difft_mode
    }

    /// Whether the center viewer renders diffs with difftastic.
    pub fn viewer_difft(&self) -> bool {
        self.pane.difft_mode
    }

    /// Toggle the markdown preview between rendered output and source, returning whether the
    /// source view is now active.
    pub fn toggle_md_source(&mut self) -> bool {
        self.pane.md_source = !self.pane.md_source;
        self.pane.md_source
    }

    /// Whether the markdown preview shows source text instead of rendered output.
    pub fn md_source(&self) -> bool {
        self.pane.md_source
    }

    /// Path held by the shared 预览 tab, if it is open.
    pub fn previewed_file(&self) -> Option<&str> {
        self.pane.preview_tab.as_deref()
    }

    /// Path of the pane's directory row registered at `index` by the last layout.
    pub fn pane_dir_path(&self, index: usize) -> Option<String> {
        self.pane.dir_path(index).map(str::to_owned)
    }

    /// Path of the pane's plain-file row registered at `index` by the last layout.
    pub fn pane_file_path(&self, index: usize) -> Option<String> {
        self.pane.file_path(index).map(str::to_owned)
    }

    /// Path of the pane's changed-file row registered at `index` by the last layout.
    pub fn pane_change_path(&self, index: usize) -> Option<String> {
        self.pane.change_path(index).map(str::to_owned)
    }

    /// Whether `(x, y)` (window pixels) lies within the project pane's content region.
    pub fn pane_contains(&self, x: f32, y: f32) -> bool {
        self.pane_w > 0.
            && x >= self.win_w - self.pane_w
            && y >= self.bar_h
            && y < self.win_h - self.status_h
    }

    /// Scroll the project pane's active view by `rows` (positive scrolls down).
    pub fn scroll_pane(&mut self, rows: f32) {
        match self.pane.tab {
            PaneTab::Files => self.pane.files_scroll.scroll_by(rows),
            PaneTab::Changes => self.pane.changes_scroll.scroll_by(rows),
        }
    }

    /// Reset the pane's scroll offsets and the center viewer (used on project switches).
    pub fn pane_reset_view(&mut self) {
        self.pane.reset_view();
    }

    /// The pane's width in chrome cells (DPI-independent), for session persistence.
    pub fn pane_cols(&self) -> f32 {
        self.pane_cols
    }

    /// Restore the pane's visibility and width from a saved session. A non-positive `cols` keeps
    /// the default width.
    pub fn set_pane_state(&mut self, visible: bool, cols: f32) {
        self.pane_visible = visible;
        if cols > 0. {
            self.pane_cols = cols.clamp(MIN_PANE_COLS, MAX_PANE_COLS);
        }
    }

    /// Color of the dividers drawn between split panes, from the chrome's border palette.
    pub fn divider_color(&self) -> Rgb {
        border()
    }

    /// Open the command palette, or close it if already open. Closes other overlays.
    pub fn toggle_command_palette(&mut self) {
        if self.command_palette.take().is_none() {
            self.command_palette = Some(PaletteState::default());
            self.settings = None;
            self.context_menu = None;
        }
    }

    /// Open the settings panel with the given state, closing the palette and context menu.
    pub fn open_settings(&mut self, state: SettingsState) {
        self.settings = Some(state);
        self.command_palette = None;
        self.context_menu = None;
    }

    /// Whether the command palette or settings panel is open.
    pub fn modal_open(&self) -> bool {
        self.command_palette.is_some() || self.settings.is_some()
    }

    /// Whether the command palette is currently open.
    pub fn palette_open(&self) -> bool {
        self.command_palette.is_some()
    }

    /// Whether the settings panel is currently open.
    pub fn settings_open(&self) -> bool {
        self.settings.is_some()
    }

    /// Close every overlay (palette, settings, context menu). Returns whether any was open.
    pub fn close_modals(&mut self) -> bool {
        let was_open = self.modal_open() || self.context_menu.is_some();
        self.command_palette = None;
        self.settings = None;
        self.context_menu = None;
        was_open
    }

    /// Append committed `text` to the search query (control characters ignored), clearing any IME
    /// composition, and reset the selection to the top of the new result set. Used for both direct
    /// keyboard text and IME commits.
    pub fn palette_input(&mut self, text: &str) {
        if let Some(p) = self.command_palette.as_mut() {
            for ch in text.chars() {
                if !ch.is_control() {
                    p.query.push(ch);
                }
            }
            p.preedit.clear();
            p.selected = 0;
        }
    }

    /// Set the in-flight IME composition (pinyin etc.) shown after the query.
    pub fn palette_preedit(&mut self, text: &str) {
        if let Some(p) = self.command_palette.as_mut() {
            p.preedit = text.to_owned();
            p.selected = 0;
        }
    }

    /// Delete the last character of the committed query (IME composition is edited by the IME).
    pub fn palette_backspace(&mut self) {
        if let Some(p) = self.command_palette.as_mut() {
            p.query.pop();
            p.selected = 0;
        }
    }

    /// Window-pixel caret box of the search field, for anchoring the IME candidate window.
    pub fn palette_ime_rect(&self) -> Option<(f32, f32, f32, f32)> {
        self.palette_ime_rect
    }

    /// Move the palette selection by `delta` within the filtered results, wrapping.
    pub fn palette_move(&mut self, delta: i32) {
        if let Some(p) = self.command_palette.as_mut() {
            let n = p.filtered().len() as i32;
            if n > 0 {
                p.selected = (((p.selected as i32 + delta) % n + n) % n) as usize;
            }
        }
    }

    /// The command for the currently highlighted result, if any.
    pub fn palette_selected_command(&self) -> Option<PaletteCmd> {
        let p = self.command_palette.as_ref()?;
        p.filtered().get(p.selected).map(|&i| PALETTE_COMMANDS[i].2)
    }

    /// The command for the filtered result at visible row `idx` (used by click hit-testing).
    pub fn palette_command_at(&self, idx: usize) -> Option<PaletteCmd> {
        let p = self.command_palette.as_ref()?;
        p.filtered().get(idx).map(|&i| PALETTE_COMMANDS[i].2)
    }

    /// Close just the command palette.
    pub fn close_palette(&mut self) {
        self.command_palette = None;
    }

    /// Close just the settings panel.
    pub fn close_settings(&mut self) {
        self.settings = None;
    }

    /// Apply a settings-panel `Hit` to the open panel, returning the resulting snapshot to apply
    /// and persist when a configured value actually changed. Returns `None` when the panel isn't
    /// open, the hit isn't a settings control, or only UI state (dropdown/focus) changed.
    pub fn settings_action(&mut self, hit: Hit) -> Option<SettingsState> {
        let state = self.settings.as_mut()?;
        let mut changed = false;
        match hit {
            // Clicking a dropdown field toggles its list; an in-progress size edit commits first.
            Hit::SettingsFamilyField => {
                changed = state.commit_size();
                state.dropdown = (state.dropdown != Some(SettingsDropdown::Family))
                    .then_some(SettingsDropdown::Family);
            },
            Hit::SettingsShellField => {
                changed = state.commit_size();
                state.dropdown = (state.dropdown != Some(SettingsDropdown::Shell))
                    .then_some(SettingsDropdown::Shell);
            },
            Hit::SettingsFamilyOption(i) if i < state.families.len() => {
                changed = state.family_idx != i;
                state.family_idx = i;
                state.dropdown = None;
            },
            Hit::SettingsShellOption(i) if i < state.shells.len() => {
                changed = state.shell_idx != i;
                state.shell_idx = i;
                state.dropdown = None;
            },
            Hit::SettingsSizeField => {
                state.dropdown = None;
                if !state.size_focus {
                    state.size_focus = true;
                    state.size_text = format!("{}", state.size_pt);
                }
            },
            _ => return None,
        }
        changed.then(|| state.clone())
    }

    /// Handle Escape while the settings panel is open: close an expanded dropdown, or drop focus
    /// from the size input (discarding the typed text). Returns whether the key was consumed this
    /// way; when `false` the panel itself should close.
    pub fn settings_escape(&mut self) -> bool {
        let Some(state) = self.settings.as_mut() else { return false };
        if state.dropdown.take().is_some() {
            return true;
        }
        std::mem::take(&mut state.size_focus)
    }

    /// Commit the size input on Enter, returning the snapshot to apply when the size changed.
    pub fn settings_commit_size(&mut self) -> Option<SettingsState> {
        let state = self.settings.as_mut()?;
        state.commit_size().then(|| state.clone())
    }

    /// Route typed text into the focused size input (digits and a decimal point only).
    pub fn settings_input(&mut self, text: &str) {
        if let Some(state) = self.settings.as_mut() {
            if state.size_focus {
                for ch in text.chars() {
                    if (ch.is_ascii_digit() || ch == '.') && state.size_text.len() < 6 {
                        state.size_text.push(ch);
                    }
                }
            }
        }
    }

    /// Delete the last character of the focused size input.
    pub fn settings_backspace(&mut self) {
        if let Some(state) = self.settings.as_mut() {
            if state.size_focus {
                state.size_text.pop();
            }
        }
    }

    /// Defocus the panel's controls (clicking its body or close button): collapse any dropdown and
    /// commit an in-progress size edit, returning the snapshot to apply when the size changed.
    pub fn settings_defocus(&mut self) -> Option<SettingsState> {
        let state = self.settings.as_mut()?;
        state.dropdown = None;
        state.commit_size().then(|| state.clone())
    }

    /// Whether `x` (window pixels) is within the grab zone of the sidebar's right-edge divider. The
    /// divider spans the full window height, so only the x coordinate matters.
    pub fn over_divider(&self, x: f32) -> bool {
        self.sidebar_visible && self.sidebar_w > 0. && (x - self.sidebar_w).abs() <= DIVIDER_GRAB_PX
    }

    /// Whether a divider drag is currently in progress.
    pub fn is_dragging_divider(&self) -> bool {
        self.dragging_divider
    }

    /// Begin dragging the sidebar divider.
    pub fn begin_divider_drag(&mut self) {
        self.dragging_divider = true;
    }

    /// End an in-progress divider drag, returning whether one was actually active (so the caller
    /// can decide whether to treat the mouse release as consumed).
    pub fn end_divider_drag(&mut self) -> bool {
        std::mem::take(&mut self.dragging_divider)
    }

    /// Resize the sidebar so its right edge tracks pointer `x` (window pixels), given the chrome
    /// cell width `cw`. The new width is clamped to a sensible cell range.
    pub fn drag_divider_to(&mut self, x: f32, cw: f32) {
        if cw > 0. {
            self.sidebar_cols = (x / cw).clamp(MIN_SIDEBAR_COLS, MAX_SIDEBAR_COLS);
        }
    }

    /// Whether `x` (window pixels) is within the grab zone of the project pane's left-edge
    /// divider.
    pub fn over_pane_divider(&self, x: f32) -> bool {
        self.pane_visible
            && self.pane_w > 0.
            && (x - (self.win_w - self.pane_w)).abs() <= DIVIDER_GRAB_PX
    }

    /// Whether a project pane divider drag is currently in progress.
    pub fn is_dragging_pane_divider(&self) -> bool {
        self.dragging_pane_divider
    }

    /// Begin dragging the project pane divider.
    pub fn begin_pane_divider_drag(&mut self) {
        self.dragging_pane_divider = true;
    }

    /// End an in-progress pane divider drag, returning whether one was actually active.
    pub fn end_pane_divider_drag(&mut self) -> bool {
        std::mem::take(&mut self.dragging_pane_divider)
    }

    /// Resize the project pane so its left edge tracks pointer `x` (window pixels).
    pub fn drag_pane_divider_to(&mut self, x: f32, cw: f32) {
        if cw > 0. {
            self.pane_cols = ((self.win_w - x) / cw).clamp(MIN_PANE_COLS, MAX_PANE_COLS);
        }
    }

    /// The last mouse position recorded via [`Self::set_mouse`], in window pixels.
    pub fn last_mouse(&self) -> (f32, f32) {
        self.mouse
    }

    /// Record the latest mouse position and recompute the hovered region. Returns whether the
    /// hovered region changed (i.e. a redraw is needed to update the highlight).
    pub fn set_mouse(&mut self, x: f32, y: f32) -> bool {
        self.mouse = (x, y);
        let hover = self.hit(x, y);
        let changed = hover != self.hover;
        self.hover = hover;
        changed
    }

    /// Whether `(x, y)` (window pixels) lies over any chrome surface (sidebar, project pane,
    /// tab bar or the bottom status bar).
    pub fn in_region(&self, x: f32, y: f32) -> bool {
        (self.sidebar_w > 0. && x < self.sidebar_w)
            || (self.pane_w > 0. && x >= self.win_w - self.pane_w)
            || (self.bar_h > 0. && y < self.bar_h)
            || (self.status_h > 0. && y >= self.win_h - self.status_h)
    }

    /// Hit-test the last recorded mouse position.
    pub fn hit_mouse(&self) -> Option<Hit> {
        self.hit(self.mouse.0, self.mouse.1)
    }

    fn hit(&self, x: f32, y: f32) -> Option<Hit> {
        self.hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h)
    }

    /// The tab bar is always shown for a non-empty project, so a single tab still exposes the
    /// active tab and the new-tab affordance.
    fn show_tab_bar(tab_count: usize) -> bool {
        tab_count >= 1
    }

    /// Build the chrome draw lists and refresh the hot regions. `cw`/`ch` are the chrome cell
    /// dimensions (pixels); `win_w`/`win_h` the window size (pixels). Text and rects are emitted in
    /// absolute window pixels (cells carry pixel positions, rendered with a 1×1 cell projection).
    pub fn layout(
        &mut self,
        info: &TabBarInfo,
        pane_data: Option<&ProjectPaneData>,
        cw: f32,
        ch: f32,
        win_w: f32,
        win_h: f32,
    ) -> ChromeDraw {
        let mut draw = ChromeDraw::default();
        self.hits.clear();
        self.palette_ime_rect = None;

        let pad_x = cw * PAD_X_MULT;
        let row_h = ch * ROW_H_MULT;

        let sidebar_w = if self.sidebar_visible { self.sidebar_cols * cw } else { 0. };
        let show_tabs = Self::show_tab_bar(info.titles.len());
        let bar_h = if show_tabs { ch * BAR_H_MULT } else { 0. };
        let status_h = if info.git_info.is_some() { ch * STATUS_H_MULT } else { 0. };
        let pane_w = if self.pane_visible && pane_data.is_some() { self.pane_cols * cw } else { 0. };

        self.sidebar_w = sidebar_w;
        self.pane_w = pane_w;
        self.bar_h = bar_h;
        self.status_h = status_h;
        self.win_h = win_h;
        self.win_w = win_w;
        draw.sidebar_width = sidebar_w;
        draw.bar_height = bar_h;
        draw.status_height = status_h;
        draw.pane_width = pane_w;

        if self.sidebar_visible {
            self.layout_sidebar(info, &mut draw, cw, ch, win_h, sidebar_w, pad_x, row_h);
        }
        if show_tabs {
            self.layout_tab_bar(info, &mut draw, cw, ch, win_w, sidebar_w, pad_x, bar_h);
        }
        if let Some(git_info) = info.git_info.as_deref() {
            self.layout_status_bar(&mut draw, cw, ch, win_w, win_h, sidebar_w, pad_x, status_h,
                git_info);
        }
        if pane_w > 0. {
            if let Some(data) = pane_data {
                project_pane::layout(&mut self.pane, data, self.hover, &mut self.hits, &mut draw,
                    cw, ch, win_w, win_h, bar_h, status_h, pane_w, pad_x, row_h);
            }
        }
        // The center viewer (file preview / diff) covers the terminal area between the chrome
        // surfaces. Laid out after the pane so its hit regions stack above the base chrome, but
        // before the context menu and modal overlays.
        if let Some(data) = pane_data {
            project_pane::layout_viewer(&mut self.pane, data, self.hover, &mut self.hits,
                &mut draw, cw, ch, sidebar_w, bar_h, win_w - pane_w, win_h - status_h, pad_x,
                row_h);
        }
        if self.context_menu.is_some() {
            self.layout_context_menu(&mut draw, cw, ch, win_w, win_h, pad_x, row_h);
        }
        // The settings panel and command palette are mutually exclusive overlays drawn on top.
        if let Some(state) = self.settings.clone() {
            self.layout_settings(&state, &mut draw, cw, ch, win_w, win_h, pad_x, row_h);
        } else if self.command_palette.is_some() {
            self.layout_command_palette(&mut draw, cw, ch, win_w, win_h, pad_x, row_h);
        }

        draw
    }

    #[allow(clippy::too_many_arguments)]
    fn layout_sidebar(
        &mut self,
        info: &TabBarInfo,
        draw: &mut ChromeDraw,
        cw: f32,
        ch: f32,
        win_h: f32,
        sidebar_w: f32,
        pad_x: f32,
        row_h: f32,
    ) {
        // Background strip and right border.
        draw.rects.push(rect(0., 0., sidebar_w, win_h, bar_bg()));
        draw.rects.push(rect(sidebar_w - 1., 0., 1., win_h, border()));

        // Header.
        push_text_px(&mut draw.cells, pad_x, baseline(0., row_h, ch), "项目", dim(), cw);
        draw.rects.push(rect(0., row_h, sidebar_w, 1., border()));

        // A delete affordance is shown per project once there is more than one (the window always
        // keeps at least one project). It reserves room on the right so labels don't run under it.
        let deletable = info.project_names.len() >= 2;
        let label_budget = if deletable { 3. } else { 2. };
        let max_label = ((sidebar_w - label_budget * pad_x - if deletable { cw } else { 0. }) / cw)
            .floor()
            .max(1.) as usize;

        // Project list, one row each below the header.
        let mut y = row_h;
        for (i, name) in info.project_names.iter().enumerate() {
            let label = if name.is_empty() { "~" } else { name.as_str() };
            let label = truncate(label, max_label);
            let region = PixelRect { x: 0., y, w: sidebar_w, h: row_h };

            // The row is "hovered" when the mouse is over its name or its delete button.
            let row_hovered =
                matches!(self.hover, Some(Hit::SelectProject(h) | Hit::CloseProject(h)) if h == i);

            let selected = i == info.active_project;
            if selected {
                draw.rects.push(rect(0., y, sidebar_w, row_h, accent()));
            } else if row_hovered {
                draw.rects.push(rect(0., y, sidebar_w, row_h, hover_bg()));
            }
            push_text_px(&mut draw.cells, pad_x, baseline(y, row_h, ch), &label, fg(), cw);
            self.hits.push((region, Hit::SelectProject(i)));

            // Delete button: only painted while the row is hovered, but its hit region is always
            // registered (you can only click it while hovering). Pushed after the row so it wins
            // hit-testing on the right edge.
            if deletable {
                let close_x = sidebar_w - cw - pad_x * 0.5;
                let close_region =
                    PixelRect { x: close_x - pad_x * 0.5, y, w: cw + pad_x, h: row_h };
                if row_hovered {
                    if self.hover == Some(Hit::CloseProject(i)) {
                        draw.rects.push(rect(close_region.x, y, close_region.w, row_h, accent()));
                    }
                    push_text_px(&mut draw.cells, close_x, baseline(y, row_h, ch), "×", dim(), cw);
                }
                self.hits.push((close_region, Hit::CloseProject(i)));
            }
            y += row_h;

            // Indented Claude session sub-rows under the active project only.
            if selected {
                let sub_h = row_h * 0.82;
                let sub_pad = pad_x + cw;
                let max_sub = ((sidebar_w - sub_pad - pad_x) / cw).floor().max(1.) as usize;
                for (j, session) in info.project_sessions.iter().enumerate() {
                    let label = truncate(&session.label, max_sub);
                    let region = PixelRect { x: 0., y, w: sidebar_w, h: sub_h };
                    if self.hover == Some(Hit::OpenClaudeSession(j)) {
                        draw.rects.push(rect(0., y, sidebar_w, sub_h, hover_bg()));
                    }
                    push_text_px(&mut draw.cells, sub_pad, baseline(y, sub_h, ch), &label, dim(), cw);
                    self.hits.push((region, Hit::OpenClaudeSession(j)));
                    y += sub_h;
                }
            }
        }

        // New-project affordance, separated from the list above.
        draw.rects.push(rect(0., y, sidebar_w, 1., border()));
        let region = PixelRect { x: 0., y, w: sidebar_w, h: row_h };
        if self.hover == Some(Hit::CreateProject) {
            draw.rects.push(rect(0., y, sidebar_w, row_h, hover_bg()));
        }
        push_text_px(&mut draw.cells, pad_x, baseline(y, row_h, ch), "+ 新建项目", dim(), cw);
        self.hits.push((region, Hit::CreateProject));
    }

    #[allow(clippy::too_many_arguments)]
    fn layout_tab_bar(
        &mut self,
        info: &TabBarInfo,
        draw: &mut ChromeDraw,
        cw: f32,
        ch: f32,
        win_w: f32,
        sidebar_w: f32,
        pad_x: f32,
        bar_h: f32,
    ) {
        let bar_x = sidebar_w;

        // Background strip (to the right of the sidebar) and bottom border.
        draw.rects.push(rect(bar_x, 0., win_w - bar_x, bar_h, bar_bg()));
        draw.rects.push(rect(bar_x, bar_h - 1., win_w - bar_x, 1., border()));

        let base = baseline(0., bar_h, ch);
        let tab_gap = cw * TAB_GAP_MULT;
        // Highlight blocks sit inside the bar with a vertical margin so they read as pills.
        let hl_margin = ((bar_h - ch) * 0.3).max(2.);
        let hl_h = bar_h - 2. * hl_margin;

        // Tabs auto-size to their label between MIN/MAX_TAB_COLS: the label is truncated to what
        // fits the widest allowed segment, and short labels are padded up to the narrowest one.
        let label_budget = ((MAX_TAB_COLS * cw - pad_x - cw) / cw).floor().max(1.) as usize;

        let mut x = bar_x + pad_x;
        for (i, title) in info.titles.iter().enumerate() {
            let id = info.ids[i];
            let name = if title.is_empty() { "shell" } else { title.as_str() };
            let label = truncate(&format!("{}:{}", i + 1, name), label_budget);
            let label_w = str_width(&label) as f32 * cw;
            let seg_w = (label_w + pad_x + cw).max(MIN_TAB_COLS * cw); // label + gap + close glyph

            // Stop drawing once segments overflow the window (no horizontal scroll).
            if x + seg_w + pad_x > win_w {
                break;
            }

            // A focused viewer tab steals the highlight from the active terminal tab.
            let selected = i == info.active && self.pane.viewer_focus.is_none();
            let hovered =
                matches!(self.hover, Some(Hit::SelectTab(h) | Hit::CloseTab(h)) if h == id);
            if selected {
                draw.rects.push(rect(x - pad_x * 0.5, hl_margin, seg_w + pad_x, hl_h, accent()));
            } else if hovered {
                draw.rects.push(rect(x - pad_x * 0.5, hl_margin, seg_w + pad_x, hl_h, hover_bg()));
            }

            // Label; its hit region spans the whole segment (the close button's region is pushed
            // later, so it wins hit-testing over the overlap).
            let label_region = PixelRect { x: x - pad_x * 0.5, y: 0., w: seg_w + pad_x, h: bar_h };
            push_text_px(&mut draw.cells, x, base, &label, if selected { fg() } else { dim() }, cw);
            self.hits.push((label_region, Hit::SelectTab(id)));

            // Close button, anchored to the segment's right edge.
            let close_x = x + seg_w - cw;
            let close_region =
                PixelRect { x: close_x - pad_x * 0.5, y: 0., w: cw + pad_x * 0.5, h: bar_h };
            if self.hover == Some(Hit::CloseTab(id)) {
                draw.rects.push(rect(close_x - 3., hl_margin, cw + 6., hl_h, hover_bg()));
            }
            push_text_px(&mut draw.cells, close_x, base, "×", dim(), cw);
            self.hits.push((close_region, Hit::CloseTab(id)));

            x += seg_w + tab_gap;
        }

        // The shared viewer tabs (预览 / diff), after the terminal tabs. They render like
        // terminal tabs — label plus close button — and exist only while holding content.
        let viewer_tabs: Vec<(ViewerKind, String)> = [
            (ViewerKind::Preview, "预览", self.pane.preview_tab.as_deref()),
            (ViewerKind::Diff, "diff", self.pane.diff_tab.as_deref()),
        ]
        .into_iter()
        .filter_map(|(kind, prefix, path)| {
            let name = path?.rsplit('/').next().unwrap_or_default();
            Some((kind, format!("{prefix}:{name}")))
        })
        .collect();
        for (kind, label) in viewer_tabs {
            let label = truncate(&label, label_budget);
            let label_w = str_width(&label) as f32 * cw;
            let seg_w = (label_w + pad_x + cw).max(MIN_TAB_COLS * cw);
            if x + seg_w + pad_x > win_w {
                break;
            }

            let selected = self.pane.viewer_focus == Some(kind);
            let hovered = matches!(
                self.hover,
                Some(Hit::ViewerTab(h) | Hit::ViewerTabClose(h)) if h == kind
            );
            if selected {
                draw.rects.push(rect(x - pad_x * 0.5, hl_margin, seg_w + pad_x, hl_h, accent()));
            } else if hovered {
                draw.rects.push(rect(x - pad_x * 0.5, hl_margin, seg_w + pad_x, hl_h, hover_bg()));
            }

            let label_region = PixelRect { x: x - pad_x * 0.5, y: 0., w: seg_w + pad_x, h: bar_h };
            push_text_px(&mut draw.cells, x, base, &label, if selected { fg() } else { dim() }, cw);
            self.hits.push((label_region, Hit::ViewerTab(kind)));

            let close_x = x + seg_w - cw;
            let close_region =
                PixelRect { x: close_x - pad_x * 0.5, y: 0., w: cw + pad_x * 0.5, h: bar_h };
            if self.hover == Some(Hit::ViewerTabClose(kind)) {
                draw.rects.push(rect(close_x - 3., hl_margin, cw + 6., hl_h, hover_bg()));
            }
            push_text_px(&mut draw.cells, close_x, base, "×", dim(), cw);
            self.hits.push((close_region, Hit::ViewerTabClose(kind)));

            x += seg_w + tab_gap;
        }

        // New-tab affordance.
        if x + cw + pad_x <= win_w {
            let region = PixelRect { x: x - pad_x * 0.5, y: 0., w: cw + pad_x, h: bar_h };
            if self.hover == Some(Hit::CreateTab) {
                draw.rects.push(rect(x - pad_x * 0.5, hl_margin, cw + pad_x, hl_h, hover_bg()));
            }
            push_text_px(&mut draw.cells, x, base, "+", fg(), cw);
            self.hits.push((region, Hit::CreateTab));
        }
    }

    /// The git status bar: a single dim line at the bottom of the terminal area (right of the
    /// sidebar) showing the active project's git summary.
    #[allow(clippy::too_many_arguments)]
    fn layout_status_bar(
        &mut self,
        draw: &mut ChromeDraw,
        cw: f32,
        ch: f32,
        win_w: f32,
        win_h: f32,
        sidebar_w: f32,
        pad_x: f32,
        status_h: f32,
        git_info: &str,
    ) {
        let y = win_h - status_h;
        draw.rects.push(rect(sidebar_w, y, win_w - sidebar_w, status_h, bar_bg()));
        draw.rects.push(rect(sidebar_w, y, win_w - sidebar_w, 1., border()));
        let budget = ((win_w - sidebar_w - 2. * pad_x) / cw).floor().max(1.) as usize;
        let label = truncate(git_info, budget);
        push_text_px(&mut draw.cells, sidebar_w + pad_x, baseline(y, status_h, ch), &label, dim(), cw);
    }

    #[allow(clippy::too_many_arguments)]
    fn layout_context_menu(
        &mut self,
        draw: &mut ChromeDraw,
        cw: f32,
        ch: f32,
        win_w: f32,
        win_h: f32,
        pad_x: f32,
        row_h: f32,
    ) {
        let Some((mx, my)) = self.context_menu else { return };

        const ITEMS: [(&str, Hit); 2] = [("Copy", Hit::Copy), ("Paste", Hit::Paste)];

        let w = MENU_COLS as f32 * cw;
        let h = ITEMS.len() as f32 * row_h;
        let x = mx.min((win_w - w).max(0.));
        let y = my.min((win_h - h).max(0.));

        // Popover surface with a hairline border.
        draw.rects.push(rect(x, y, w, h, menu_bg()));
        push_border(&mut draw.rects, x, y, w, h, border());

        for (k, (label, hit)) in ITEMS.iter().enumerate() {
            let item_y = y + k as f32 * row_h;
            let region = PixelRect { x, y: item_y, w, h: row_h };
            if self.hover == Some(*hit) {
                draw.rects.push(rect(x, item_y, w, row_h, hover_bg()));
            }
            push_text_px(&mut draw.cells, x + pad_x, baseline(item_y, row_h, ch), label, fg(), cw);
            self.hits.push((region, *hit));
        }
    }

    /// The `Ctrl+Shift+P` command palette: a centred list of commands, navigable by mouse or by
    /// the keyboard (arrows + Enter, handled in `Display::handle_chrome_event`).
    #[allow(clippy::too_many_arguments)]
    fn layout_command_palette(
        &mut self,
        draw: &mut ChromeDraw,
        cw: f32,
        ch: f32,
        win_w: f32,
        win_h: f32,
        pad_x: f32,
        row_h: f32,
    ) {
        let Some(state) = self.command_palette.as_ref() else { return };
        let query = state.query.clone();
        let preedit = state.preedit.clone();
        let selected = state.selected;
        let filtered = state.filtered();

        // A wide, Spotlight-style panel anchored near the top of the window.
        let w = (46. * cw).clamp(24. * cw, (win_w - 4. * cw).max(24. * cw));
        let search_h = row_h * 1.5;
        let list_rows = filtered.len().max(1) as f32;
        let pad_y = row_h * 0.3;
        let h = search_h + row_h * list_rows + pad_y;
        let x = ((win_w - w) * 0.5).max(0.);
        let y = (win_h * 0.16).clamp(0., (win_h - h).max(0.));

        // Surface, border, and a full-panel hit so body clicks are swallowed, not dismissed.
        draw.rects.push(rect(x, y, w, h, menu_bg()));
        push_border(&mut draw.rects, x, y, w, h, border());
        self.hits.push((PixelRect { x, y, w, h }, Hit::ModalBackground));

        // Search field: a raised strip with a prompt glyph, then the committed query (bright) and
        // any IME composition (dim), with a caret and the IME anchor at the end.
        let inset = pad_x * 0.5;
        draw.rects.push(rect(x + inset, y + inset, w - 2. * inset, search_h - inset, field_bg()));
        let prompt_base = baseline(y, search_h, ch);
        push_text_px(&mut draw.cells, x + pad_x, prompt_base, "›", dim(), cw);
        let text_x = x + pad_x + cw * 1.6;
        let text_end = x + w - pad_x;

        if query.is_empty() && preedit.is_empty() {
            push_text_px(&mut draw.cells, text_x, prompt_base, "搜索命令…", dim(), cw);
            self.palette_ime_rect = Some((text_x, y, cw * 2., search_h));
        } else {
            // Committed query.
            let q_budget = ((text_end - text_x) / cw).floor().max(0.) as usize;
            let q_shown = truncate(&query, q_budget);
            push_text_px(&mut draw.cells, text_x, prompt_base, &q_shown, fg(), cw);
            let mut cx = text_x + str_width(&q_shown) as f32 * cw;
            // In-flight IME composition, dimmed.
            if !preedit.is_empty() {
                let p_budget = ((text_end - cx) / cw).floor().max(0.) as usize;
                let p_shown = truncate(&preedit, p_budget);
                push_text_px(&mut draw.cells, cx, prompt_base, &p_shown, dim(), cw);
                cx += str_width(&p_shown) as f32 * cw;
            }
            draw.rects.push(rect(cx + 1., y + search_h * 0.28, 1.5, search_h * 0.44, fg()));
            self.palette_ime_rect = Some((cx, y, cw * 2., search_h));
        }
        draw.rects.push(rect(x, y + search_h, w, 1., border()));

        // Results, or an empty-state line.
        if filtered.is_empty() {
            let base = baseline(y + search_h, row_h, ch);
            push_text_px(&mut draw.cells, x + pad_x * 1.5, base, "无匹配命令", dim(), cw);
            return;
        }
        for (vis, &cmd_i) in filtered.iter().enumerate() {
            let label = PALETTE_COMMANDS[cmd_i].0;
            let item_y = y + search_h + row_h * vis as f32;
            let region = PixelRect { x, y: item_y, w, h: row_h };
            if vis == selected {
                draw.rects.push(rect(x + 3., item_y + 2., w - 6., row_h - 4., sel_bg()));
            } else if self.hover == Some(Hit::PaletteSelect(vis)) {
                draw.rects.push(rect(x + 3., item_y + 2., w - 6., row_h - 4., hover_bg()));
            }
            let fg = if vis == selected { c(0xff, 0xff, 0xff) } else { fg() };
            push_text_px(&mut draw.cells, x + pad_x * 1.5, baseline(item_y, row_h, ch), label, fg, cw);
            self.hits.push((region, Hit::PaletteSelect(vis)));
        }
    }

    /// The settings panel: font family and shell as dropdown selects, font size as an input box.
    #[allow(clippy::too_many_arguments)]
    fn layout_settings(
        &mut self,
        state: &SettingsState,
        draw: &mut ChromeDraw,
        cw: f32,
        ch: f32,
        win_w: f32,
        win_h: f32,
        pad_x: f32,
        row_h: f32,
    ) {
        let w = 34. * cw;
        let pad_v = row_h * 0.4;
        // Content rows: title, family, size, shell, note.
        let h = pad_v * 2. + row_h * 5.;
        let x = ((win_w - w) * 0.5).max(0.);
        let y = ((win_h - h) * 0.5).max(0.);
        let left = x + pad_x;
        // Fields (dropdowns / input box) occupy the right side of each row.
        let field_x = x + w * 0.3;
        let field_w = x + w - pad_x - field_x;

        draw.rects.push(rect(x, y, w, h, menu_bg()));
        push_border(&mut draw.rects, x, y, w, h, border());
        self.hits.push((PixelRect { x, y, w, h }, Hit::ModalBackground));

        // Header: title + close button, vertically centred in the whole band above the divider
        // (the top padding is part of the band, so the text doesn't sit low).
        let header_h = pad_v + row_h;
        push_text_px(&mut draw.cells, left, baseline(y, header_h, ch), "设置", fg(), cw);
        let close_x = x + w - pad_x - cw;
        let close_region = PixelRect { x: close_x - pad_x * 0.5, y, w: cw + pad_x, h: header_h };
        if self.hover == Some(Hit::SettingsClose) {
            draw.rects.push(rect(close_region.x, y, close_region.w, header_h, hover_bg()));
        }
        push_text_px(&mut draw.cells, close_x, baseline(y, header_h, ch), "×", dim(), cw);
        self.hits.push((close_region, Hit::SettingsClose));
        let mut row_y = y + header_h;
        draw.rects.push(rect(x, row_y, w, 1., border()));

        // Font family dropdown.
        push_text_px(&mut draw.cells, left, baseline(row_y, row_h, ch), "字体", dim(), cw);
        let family_row_y = row_y;
        self.dropdown_field(draw, cw, ch, field_x, row_y, field_w, row_h, pad_x, state.family(),
            state.dropdown == Some(SettingsDropdown::Family), Hit::SettingsFamilyField);
        row_y += row_h;

        // Font size input box.
        push_text_px(&mut draw.cells, left, baseline(row_y, row_h, ch), "字号", dim(), cw);
        self.size_input(draw, cw, ch, field_x, row_y, field_w, row_h, pad_x, state);
        row_y += row_h;

        // Shell dropdown.
        push_text_px(&mut draw.cells, left, baseline(row_y, row_h, ch), "Shell", dim(), cw);
        let shell_row_y = row_y;
        let shell_label = state.shell().map(|s| s.label.as_str()).unwrap_or_default();
        self.dropdown_field(draw, cw, ch, field_x, row_y, field_w, row_h, pad_x, shell_label,
            state.dropdown == Some(SettingsDropdown::Shell), Hit::SettingsShellField);
        row_y += row_h;

        // Footnote: shell changes only affect newly opened tabs.
        push_text_px(&mut draw.cells, left, baseline(row_y, row_h, ch), "Shell 仅对新标签页生效", dim(), cw);

        // The expanded dropdown's option list, drawn as an overlay popup over the panel. Laid out
        // last so its hit regions win over the rows it covers.
        match state.dropdown {
            Some(SettingsDropdown::Family) => {
                let options: Vec<&str> = state.families.iter().map(String::as_str).collect();
                self.dropdown_popup(draw, cw, ch, field_x, family_row_y + row_h, field_w, row_h,
                    pad_x, win_h, &options, state.family_idx, Hit::SettingsFamilyOption);
            },
            Some(SettingsDropdown::Shell) => {
                let options: Vec<&str> = state.shells.iter().map(|s| s.label.as_str()).collect();
                self.dropdown_popup(draw, cw, ch, field_x, shell_row_y + row_h, field_w, row_h,
                    pad_x, win_h, &options, state.shell_idx, Hit::SettingsShellOption);
            },
            None => {},
        }
    }

    /// Draw a closed dropdown select: a sunken field showing the current `value` with a ▼/▲
    /// affordance, registering `hit` over the whole field.
    #[allow(clippy::too_many_arguments)]
    fn dropdown_field(
        &mut self,
        draw: &mut ChromeDraw,
        cw: f32,
        ch: f32,
        field_x: f32,
        row_y: f32,
        field_w: f32,
        row_h: f32,
        pad_x: f32,
        value: &str,
        open: bool,
        hit: Hit,
    ) {
        let region = PixelRect { x: field_x, y: row_y, w: field_w, h: row_h };
        let bg = if open || self.hover == Some(hit) { hover_bg() } else { field_bg() };
        draw.rects.push(rect(field_x, row_y + 2., field_w, row_h - 4., bg));
        push_border(&mut draw.rects, field_x, row_y + 2., field_w, row_h - 4., border());
        // Keep the value clear of the arrow glyph at the field's right edge.
        let budget = ((field_w - 2.5 * cw - pad_x) / cw).floor().max(1.) as usize;
        let value = truncate(value, budget);
        push_text_px(&mut draw.cells, field_x + pad_x * 0.5, baseline(row_y, row_h, ch), &value, fg(), cw);
        let arrow = if open { "▲" } else { "▼" };
        let arrow_x = field_x + field_w - cw - pad_x * 0.5;
        push_text_px(&mut draw.cells, arrow_x, baseline(row_y, row_h, ch), arrow, dim(), cw);
        self.hits.push((region, hit));
    }

    /// Draw an open dropdown's option list as a popup anchored below its field (flipped above when
    /// it would run off the window), highlighting the selected and hovered rows.
    #[allow(clippy::too_many_arguments)]
    fn dropdown_popup(
        &mut self,
        draw: &mut ChromeDraw,
        cw: f32,
        ch: f32,
        x: f32,
        below_y: f32,
        w: f32,
        row_h: f32,
        pad_x: f32,
        win_h: f32,
        options: &[&str],
        selected: usize,
        hit_for: fn(usize) -> Hit,
    ) {
        let h = options.len().max(1) as f32 * row_h + 2.;
        let y = if below_y + h > win_h { (below_y - row_h - h).max(0.) } else { below_y };
        draw.overlay_rects.push(rect(x, y, w, h, menu_bg()));
        push_border(&mut draw.overlay_rects, x, y, w, h, border());
        for (i, label) in options.iter().enumerate() {
            let item_y = y + 1. + i as f32 * row_h;
            let region = PixelRect { x, y: item_y, w, h: row_h };
            let hit = hit_for(i);
            if self.hover == Some(hit) {
                draw.overlay_rects.push(rect(x + 1., item_y, w - 2., row_h, sel_bg()));
            } else if i == selected {
                draw.overlay_rects.push(rect(x + 1., item_y, w - 2., row_h, accent()));
            }
            let budget = ((w - 1.5 * pad_x) / cw).floor().max(1.) as usize;
            let label = truncate(label, budget);
            push_text_px(&mut draw.overlay_cells, x + pad_x, baseline(item_y, row_h, ch), &label, fg(), cw);
            self.hits.push((region, hit));
        }
    }

    /// Draw the font-size input box: the live edit text with a caret while focused, otherwise the
    /// current size.
    #[allow(clippy::too_many_arguments)]
    fn size_input(
        &mut self,
        draw: &mut ChromeDraw,
        cw: f32,
        ch: f32,
        field_x: f32,
        row_y: f32,
        field_w: f32,
        row_h: f32,
        pad_x: f32,
        state: &SettingsState,
    ) {
        let region = PixelRect { x: field_x, y: row_y, w: field_w, h: row_h };
        draw.rects.push(rect(field_x, row_y + 2., field_w, row_h - 4., field_bg()));
        let border_color = if state.size_focus {
            sel_bg()
        } else if self.hover == Some(Hit::SettingsSizeField) {
            dim()
        } else {
            border()
        };
        push_border(&mut draw.rects, field_x, row_y + 2., field_w, row_h - 4., border_color);
        let shown =
            if state.size_focus { state.size_text.clone() } else { format!("{}", state.size_pt) };
        let budget = ((field_w - pad_x) / cw).floor().max(1.) as usize;
        let shown = truncate(&shown, budget);
        let end = push_text_px(&mut draw.cells, field_x + pad_x * 0.5, baseline(row_y, row_h, ch),
            &shown, fg(), cw);
        if state.size_focus {
            draw.rects.push(rect(end + 1., row_y + row_h * 0.28, 1.5, row_h * 0.44, fg()));
        }
        self.hits.push((region, Hit::SettingsSizeField));
    }
}

/// Solid window-pixel rectangle.
pub(crate) fn rect(x: f32, y: f32, w: f32, h: f32, color: Rgb) -> RenderRect {
    RenderRect::new(x, y, w, h, color, 1.)
}

/// Hairline border around `(x, y, w, h)` as four 1px rects.
fn push_border(rects: &mut Vec<RenderRect>, x: f32, y: f32, w: f32, h: f32, color: Rgb) {
    rects.push(rect(x, y, w, 1., color));
    rects.push(rect(x, y + h - 1., w, 1., color));
    rects.push(rect(x, y, 1., h, color));
    rects.push(rect(x + w - 1., y, 1., h, color));
}

/// Text baseline (window pixels) that vertically centres a `ch`-tall line within a row of height
/// `row_h` starting at `row_top`. The renderer places a glyph's baseline at the bottom of its cell,
/// so this is the bottom of the centred line box.
pub(crate) fn baseline(row_top: f32, row_h: f32, ch: f32) -> f32 {
    row_top + (row_h + ch) * 0.5
}

/// Emit `text` as glyphs at absolute window pixels, starting at `(x, baseline)` and advancing by
/// `advance` pixels per cell (2× for wide glyphs). Cells carry pixel positions, so the chrome pass
/// must render them with a 1×1-pixel cell projection. Returns the x pixel after the last glyph.
pub(crate) fn push_text_px(
    cells: &mut Vec<RenderableCell>,
    x: f32,
    baseline: f32,
    text: &str,
    fg: Rgb,
    advance: f32,
) -> f32 {
    push_text_px_styled(cells, x, baseline, text, fg, advance, false, false)
}

/// Like [`push_text_px`], with bold/italic font styling (rendered via the glyph cache's font
/// variants).
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_text_px_styled(
    cells: &mut Vec<RenderableCell>,
    x: f32,
    baseline: f32,
    text: &str,
    fg: Rgb,
    advance: f32,
    bold: bool,
    italic: bool,
) -> f32 {
    // The shader puts the baseline one cell below the cell origin (cell height is 1px here).
    let row = (baseline.round() as usize).saturating_sub(1);
    let mut x = x;
    for ch in text.chars() {
        let width = ch.width().unwrap_or(0);
        if width == 0 {
            continue;
        }
        let mut flags = if width == 2 { Flags::WIDE_CHAR } else { Flags::empty() };
        if bold {
            flags.insert(Flags::BOLD);
        }
        if italic {
            flags.insert(Flags::ITALIC);
        }
        cells.push(RenderableCell {
            point: Point::new(row, Column(x.round().max(0.) as usize)),
            character: ch,
            fg,
            bg: Rgb::new(0, 0, 0),
            bg_alpha: 0.,
            underline: fg,
            flags,
            extra: None,
        });
        x += advance * width as f32;
    }
    x
}

/// Display width of `text` in cells.
pub(crate) fn str_width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Truncate `text` to at most `max` display cells, appending an ellipsis when shortened.
pub(crate) fn truncate(text: &str, max: usize) -> String {
    if str_width(text) <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut width = 0;
    let mut out = String::new();
    for ch in text.chars() {
        let cw = ch.width().unwrap_or(0);
        if width + cw > max.saturating_sub(1) {
            break;
        }
        width += cw;
        out.push(ch);
    }
    out.push('…');
    out
}
