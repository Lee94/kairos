//! The display subsystem including window management, font rasterization, and
//! GPU drawing.

use std::cmp;
use std::fmt::{self, Formatter};
use std::mem::{self, ManuallyDrop};
use std::num::NonZeroU32;
use std::ops::Deref;
use std::time::{Duration, Instant};

use glutin::config::GetGlConfig;
use glutin::context::{NotCurrentContext, PossiblyCurrentContext};
use glutin::display::GetGlDisplay;
use glutin::error::ErrorKind;
use glutin::prelude::*;
use glutin::surface::{Surface, SwapInterval, WindowSurface};

use log::{debug, info};
use parking_lot::MutexGuard;
use serde::{Deserialize, Serialize};

use winit::dpi::PhysicalSize;
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::ModifiersState;
use winit::raw_window_handle::RawWindowHandle;
use winit::window::CursorIcon;

use crossfont::{Rasterize, Rasterizer, Size as FontSize};
use unicode_width::UnicodeWidthChar;

use kairos_terminal::event::{EventListener, WindowSize};
use kairos_terminal::grid::Dimensions as TermDimensions;
use kairos_terminal::index::{Column, Direction, Line, Point};
use kairos_terminal::selection::Selection;
use kairos_terminal::term::cell::Flags;
use kairos_terminal::term::{
    self, LineDamageBounds, MIN_COLUMNS, MIN_SCREEN_LINES, Term, TermDamage, TermMode,
};
use kairos_terminal::vte::ansi::{CursorShape, NamedColor};

use crate::config::UiConfig;
use crate::config::debug::RendererPreference;
use crate::config::font::Font;
use crate::config::window::Dimensions;
#[cfg(not(windows))]
use crate::config::window::StartupMode;
use crate::display::bell::VisualBell;
use crate::display::color::{List, Rgb};
use crate::display::content::{RenderableContent, RenderableCursor};
use crate::display::cursor::IntoRects;
use crate::display::damage::{DamageTracker, damage_y_to_viewport_y};
use crate::display::hint::{HintMatch, HintState};
use crate::display::chrome::{Chrome, Hit, PaletteCmd, PixelRect, SettingsState};
use crate::display::meter::Meter;
use crate::display::project_pane::{PaneTab, ProjectPaneData};
use crate::display::window::Window;
use crate::event::{Event, EventType, Mouse, SearchState, SplitDirection};
use crate::message_bar::{MessageBuffer, MessageType};
use crate::renderer::rects::{RenderLine, RenderLines, RenderRect};
use crate::renderer::{self, GlyphCache, Renderer, platform};
use crate::scheduler::{Scheduler, TimerId, Topic};
use crate::string::{ShortenDirection, StrShortener};

pub mod color;
pub mod content;
pub mod cursor;
pub mod hint;
pub mod window;

mod bell;
pub mod chrome;
mod damage;
mod meter;
pub mod project_pane;

/// Label for the forward terminal search bar.
const FORWARD_SEARCH_LABEL: &str = "Search: ";

/// Label for the backward terminal search bar.
const BACKWARD_SEARCH_LABEL: &str = "Backward Search: ";

/// The character used to shorten the visible text like uri preview or search regex.
const SHORTENER: char = '…';

/// Color which is used to highlight damaged rects when debugging.
const DAMAGE_RECT_COLOR: Rgb = Rgb::new(255, 0, 255);

#[derive(Debug)]
pub enum Error {
    /// Error with window management.
    Window(window::Error),

    /// Error dealing with fonts.
    Font(crossfont::Error),

    /// Error in renderer.
    Render(renderer::Error),

    /// Error during context operations.
    Context(glutin::error::Error),
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Window(err) => err.source(),
            Error::Font(err) => err.source(),
            Error::Render(err) => err.source(),
            Error::Context(err) => err.source(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Error::Window(err) => err.fmt(f),
            Error::Font(err) => err.fmt(f),
            Error::Render(err) => err.fmt(f),
            Error::Context(err) => err.fmt(f),
        }
    }
}

impl From<window::Error> for Error {
    fn from(val: window::Error) -> Self {
        Error::Window(val)
    }
}

impl From<crossfont::Error> for Error {
    fn from(val: crossfont::Error) -> Self {
        Error::Font(val)
    }
}

impl From<renderer::Error> for Error {
    fn from(val: renderer::Error) -> Self {
        Error::Render(val)
    }
}

impl From<glutin::error::Error> for Error {
    fn from(val: glutin::error::Error) -> Self {
        Error::Context(val)
    }
}

/// Terminal size info.
#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub struct SizeInfo<T = f32> {
    /// Terminal window width.
    width: T,

    /// Terminal window height.
    height: T,

    /// Width of individual cell.
    cell_width: T,

    /// Height of individual cell.
    cell_height: T,

    /// Horizontal window padding.
    padding_x: T,

    /// Vertical window padding.
    padding_y: T,

    /// Extra pixels reserved at the top of the window for the egui chrome (tab bar). The grid and
    /// all overlays are shifted down by this amount.
    top_extra: T,

    /// Extra horizontal pixels reserved on the left edge for the egui project sidebar.
    left_extra: T,

    /// Extra pixels reserved at the right edge of the window beyond this grid's area: the
    /// project pane, plus — for split panes — the space taken by panes to the right.
    right_extra: T,

    /// Extra pixels reserved at the bottom edge of the window beyond this grid's area: the git
    /// status bar, plus — for split panes — the space taken by panes below.
    bottom_extra: T,

    /// Number of lines in the viewport.
    screen_lines: usize,

    /// Number of columns in the viewport.
    columns: usize,
}

impl From<SizeInfo<f32>> for SizeInfo<u32> {
    fn from(size_info: SizeInfo<f32>) -> Self {
        Self {
            width: size_info.width as u32,
            height: size_info.height as u32,
            cell_width: size_info.cell_width as u32,
            cell_height: size_info.cell_height as u32,
            padding_x: size_info.padding_x as u32,
            padding_y: size_info.padding_y as u32,
            top_extra: size_info.top_extra as u32,
            left_extra: size_info.left_extra as u32,
            right_extra: size_info.right_extra as u32,
            bottom_extra: size_info.bottom_extra as u32,
            screen_lines: size_info.screen_lines,
            // NOTE: pre-existing upstream quirk — this mirrors `screen_lines`, not `columns`.
            columns: size_info.screen_lines,
        }
    }
}

impl From<SizeInfo<f32>> for WindowSize {
    fn from(size_info: SizeInfo<f32>) -> Self {
        Self {
            num_cols: size_info.columns() as u16,
            num_lines: size_info.screen_lines() as u16,
            cell_width: size_info.cell_width() as u16,
            cell_height: size_info.cell_height() as u16,
        }
    }
}

impl<T: Clone + Copy> SizeInfo<T> {
    #[inline]
    pub fn width(&self) -> T {
        self.width
    }

    #[inline]
    pub fn height(&self) -> T {
        self.height
    }

    #[inline]
    pub fn cell_width(&self) -> T {
        self.cell_width
    }

    #[inline]
    pub fn cell_height(&self) -> T {
        self.cell_height
    }

    #[inline]
    pub fn padding_x(&self) -> T {
        self.padding_x
    }

    #[inline]
    pub fn padding_y(&self) -> T {
        self.padding_y
    }

    #[inline]
    pub fn top_extra(&self) -> T {
        self.top_extra
    }

    #[inline]
    pub fn left_extra(&self) -> T {
        self.left_extra
    }

    #[inline]
    pub fn right_extra(&self) -> T {
        self.right_extra
    }

    #[inline]
    pub fn bottom_extra(&self) -> T {
        self.bottom_extra
    }
}

impl SizeInfo<f32> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        width: f32,
        height: f32,
        cell_width: f32,
        cell_height: f32,
        mut padding_x: f32,
        mut padding_y: f32,
        dynamic_padding: bool,
    ) -> SizeInfo {
        if dynamic_padding {
            padding_x = Self::dynamic_padding(padding_x.floor(), width, cell_width);
            padding_y = Self::dynamic_padding(padding_y.floor(), height, cell_height);
        }

        let lines = (height - 2. * padding_y) / cell_height;
        let screen_lines = cmp::max(lines as usize, MIN_SCREEN_LINES);

        let columns = (width - 2. * padding_x) / cell_width;
        let columns = cmp::max(columns as usize, MIN_COLUMNS);

        SizeInfo {
            width,
            height,
            cell_width,
            cell_height,
            padding_x: padding_x.floor(),
            padding_y: padding_y.floor(),
            top_extra: 0.,
            left_extra: 0.,
            right_extra: 0.,
            bottom_extra: 0.,
            screen_lines,
            columns,
        }
    }

    /// Geometry for a pane occupying `rect` (window pixels) of a window described by `window`.
    ///
    /// The full window dimensions are kept (the renderer's NDC math depends on them); the pane
    /// rect is expressed through the four `*_extra` reservations, so `grid_left`/`grid_top`
    /// point at the pane's padded origin and all grid-origin-relative consumers (mouse mapping,
    /// rect drawing, IME anchoring) work per-pane unchanged.
    pub fn for_pane(window: &SizeInfo, rect: PixelRect, padding_x: f32, padding_y: f32) -> SizeInfo {
        let padding_x = padding_x.floor();
        let padding_y = padding_y.floor();

        let columns = ((rect.w - 2. * padding_x) / window.cell_width) as usize;
        let screen_lines = ((rect.h - 2. * padding_y) / window.cell_height) as usize;

        SizeInfo {
            width: window.width,
            height: window.height,
            cell_width: window.cell_width,
            cell_height: window.cell_height,
            padding_x,
            padding_y,
            left_extra: rect.x.floor(),
            top_extra: rect.y.floor(),
            right_extra: (window.width - rect.x - rect.w).floor().max(0.),
            bottom_extra: (window.height - rect.y - rect.h).floor().max(0.),
            screen_lines: cmp::max(screen_lines, MIN_SCREEN_LINES),
            columns: cmp::max(columns, MIN_COLUMNS),
        }
    }

    #[inline]
    pub fn reserve_lines(&mut self, count: usize) {
        self.screen_lines = cmp::max(self.screen_lines.saturating_sub(count), MIN_SCREEN_LINES);
    }

    /// Reserve `px` pixels at the top of the window (for the chrome tab bar), removing the
    /// equivalent whole rows from the grid and shifting it down.
    #[inline]
    pub fn reserve_top_px(&mut self, px: f32) {
        self.top_extra = px;
        if self.cell_height > 0. {
            self.reserve_lines((px / self.cell_height).ceil() as usize);
        }
    }

    /// Reserve `px` pixels at the bottom of the window (for the chrome status bar), removing the
    /// equivalent whole rows from the bottom of the grid.
    #[inline]
    pub fn reserve_bottom_px(&mut self, px: f32) {
        if self.cell_height > 0. && px > 0. {
            self.reserve_lines((px / self.cell_height).ceil() as usize);
        }
    }

    /// Reserve `width` pixels at the left of the window (for the project sidebar), removing the
    /// covered columns from the grid and shifting it right.
    #[inline]
    pub fn reserve_left_cols(&mut self, width: f32) {
        self.left_extra = width;
        self.recompute_columns();
    }

    /// Reserve `width` pixels at the right of the window (for the project pane), removing the
    /// covered columns from the grid. The grid stays left-anchored.
    #[inline]
    pub fn reserve_right_cols(&mut self, width: f32) {
        self.right_extra = width;
        self.recompute_columns();
    }

    /// Recompute the column count from the width left over after padding and both horizontal
    /// chrome reservations, so the left/right reserve calls are order-independent.
    #[inline]
    fn recompute_columns(&mut self) {
        let columns =
            (self.width - 2. * self.padding_x - self.left_extra - self.right_extra)
                / self.cell_width;
        self.columns = cmp::max(columns as usize, MIN_COLUMNS);
    }

    /// The vertical pixel offset at which the grid (and overlays) begin.
    #[inline]
    pub fn grid_top(&self) -> f32 {
        self.padding_y + self.top_extra
    }

    /// The horizontal pixel offset at which the grid (and overlays) begin.
    #[inline]
    pub fn grid_left(&self) -> f32 {
        self.padding_x + self.left_extra
    }

    /// Check if coordinates are inside the terminal grid.
    ///
    /// The padding, top chrome, message bar or search are not counted as part of the grid.
    #[inline]
    pub fn contains_point(&self, x: usize, y: usize) -> bool {
        let grid_top = self.grid_top();
        let grid_left = self.grid_left();
        x <= (grid_left + self.columns as f32 * self.cell_width) as usize
            && x > grid_left as usize
            && y <= (grid_top + self.screen_lines as f32 * self.cell_height) as usize
            && y > grid_top as usize
    }

    /// Calculate padding to spread it evenly around the terminal content.
    #[inline]
    fn dynamic_padding(padding: f32, dimension: f32, cell_dimension: f32) -> f32 {
        padding + ((dimension - 2. * padding) % cell_dimension) / 2.
    }
}

impl TermDimensions for SizeInfo {
    #[inline]
    fn columns(&self) -> usize {
        self.columns
    }

    #[inline]
    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    #[inline]
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }
}

#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct DisplayUpdate {
    pub dirty: bool,

    dimensions: Option<PhysicalSize<u32>>,
    cursor_dirty: bool,
    font: Option<Font>,
}

impl DisplayUpdate {
    pub fn dimensions(&self) -> Option<PhysicalSize<u32>> {
        self.dimensions
    }

    pub fn font(&self) -> Option<&Font> {
        self.font.as_ref()
    }

    pub fn cursor_dirty(&self) -> bool {
        self.cursor_dirty
    }

    pub fn set_dimensions(&mut self, dimensions: PhysicalSize<u32>) {
        self.dimensions = Some(dimensions);
        self.dirty = true;
    }

    pub fn set_font(&mut self, font: Font) {
        self.font = Some(font);
        self.dirty = true;
    }

    pub fn set_cursor_dirty(&mut self) {
        self.cursor_dirty = true;
        self.dirty = true;
    }
}

/// A Claude Code session shown as a sub-row under the active project in the sidebar.
///
/// Only the label is needed for rendering; the click is resolved back to a session by its row
/// index against the window's own session cache.
pub struct ClaudeSessionRow {
    /// Display label (first user prompt, or `(no prompt)`).
    pub label: String,
}

/// Content needed to render the in-window tab bar.
///
/// The tab bar is only shown when a window hosts more than one tab.
pub struct TabBarInfo {
    /// Title of each tab in the active project, in order.
    pub titles: Vec<String>,
    /// Stable terminal id of each tab, in order (parallel to `titles`).
    pub ids: Vec<u64>,
    /// Index of the currently active tab within the active project.
    pub active: usize,
    /// Display name of each project, in order (shown in the left sidebar).
    pub project_names: Vec<String>,
    /// Index of the currently active project.
    pub active_project: usize,
    /// Claude Code sessions for the active project (newest first), shown as indented sub-rows.
    pub project_sessions: Vec<ClaudeSessionRow>,
    /// One-line git summary (current branch) for the active project, shown in a status bar at the
    /// bottom of the terminal area. `None` (no bar) when the project isn't inside a git repo.
    pub git_info: Option<String>,
}

/// Actions produced by the egui chrome during a frame, drained and dispatched by the window.
#[derive(Default)]
pub struct ChromeActions {
    /// Activate the tab with this tab id.
    pub select: Option<u64>,
    /// Close the tab with this tab id.
    pub close: Option<u64>,
    /// Create a new tab.
    pub create: bool,
    /// Split the focused pane in this direction (from the command palette).
    pub split_pane: Option<SplitDirection>,
    /// Close the focused pane (from the command palette).
    pub close_pane: bool,
    /// Maximize or restore the focused pane (from the command palette).
    pub toggle_pane_zoom: bool,
    /// Move pane focus to the next pane (from the command palette).
    pub focus_next_pane: bool,
    /// Copy the active terminal's selection.
    pub copy: bool,
    /// Paste the clipboard into the active terminal.
    pub paste: bool,
    /// Activate the project at this index.
    pub select_project: Option<usize>,
    /// Delete the project at this index (and all its tabs).
    pub close_project: Option<usize>,
    /// Open the folder picker to create a new project.
    pub create_project: bool,
    /// Open (resume) the active project's Claude session at this index into `project_sessions`.
    pub open_claude_session: Option<usize>,
    /// Open the settings panel (the window seeds its state from the live config).
    pub open_settings: bool,
    /// Toggle the project sidebar (from the command palette).
    pub toggle_sidebar: bool,
    /// Toggle the right-side project pane (from the command palette).
    pub toggle_pane: bool,
    /// Expand/collapse the pane's file-tree directory with this root-relative path.
    pub pane_toggle_dir: Option<String>,
    /// Toggle the center viewer's diff for the changed file with this root-relative path.
    pub pane_toggle_diff: Option<String>,
    /// Toggle the center viewer's file preview for this root-relative path.
    pub pane_preview: Option<String>,
    /// Switch the viewer's diff layout (`false` = unified 行间, `true` = side-by-side 分栏).
    pub viewer_layout: Option<bool>,
    /// Toggle difftastic rendering for the viewer's diff.
    pub viewer_toggle_difft: bool,
    /// Toggle the markdown preview between rendered output and source text.
    pub viewer_toggle_md_source: bool,
    /// Refresh the project pane's directory tree and git status.
    pub pane_refresh: bool,
    /// A settings-panel change to apply live and persist to disk.
    pub settings: Option<SettingsState>,
    /// The chrome's reserved size (top bar height or sidebar width) changed; the terminal layout
    /// must be recomputed.
    pub layout_changed: bool,
}

/// The display wraps a window, font rasterizer, and GPU renderer.
pub struct Display {
    pub window: Window,

    pub size_info: SizeInfo,

    /// Hint highlighted by the mouse.
    pub highlighted_hint: Option<HintMatch>,
    /// Frames since hint highlight was created.
    highlighted_hint_age: usize,

    /// Hint highlighted by the vi mode cursor.
    pub vi_highlighted_hint: Option<HintMatch>,
    /// Frames since hint highlight was created.
    vi_highlighted_hint_age: usize,

    pub raw_window_handle: RawWindowHandle,

    /// UI cursor visibility for blinking.
    pub cursor_hidden: bool,

    pub visual_bell: VisualBell,

    /// Mapped RGB values for each terminal color.
    pub colors: List,

    /// State of the keyboard hints.
    pub hint_state: HintState,

    /// Unprocessed display updates.
    pub pending_update: DisplayUpdate,

    /// The renderer update that takes place only once before the actual rendering.
    pub pending_renderer_update: Option<RendererUpdate>,

    /// The ime on the given display.
    pub ime: Ime,

    /// The state of the timer for frame scheduling.
    pub frame_timer: FrameTimer,

    /// Damage tracker for the given display.
    pub damage_tracker: DamageTracker,

    /// Font size used by the window.
    pub font_size: FontSize,

    // Mouse point position when highlighting hints.
    hint_mouse_point: Option<Point>,

    renderer: ManuallyDrop<Renderer>,
    renderer_preference: Option<RendererPreference>,

    surface: ManuallyDrop<Surface<WindowSurface>>,

    context: ManuallyDrop<PossiblyCurrentContext>,

    glyph_cache: GlyphCache,
    meter: Meter,

    /// Native window chrome state: tab bar, project sidebar and context menu.
    chrome: Chrome,
    /// Base font for the chrome, captured from the config at startup; only its family is used —
    /// the size is always the fixed [`chrome::CHROME_FONT_PT`] × window scale factor, so the
    /// chrome deliberately does *not* follow runtime terminal font changes.
    chrome_base: Font,
    /// Glyph cache for the chrome, at a fixed UI size independent of the terminal font.
    chrome_glyph_cache: GlyphCache,
    /// Cell `(width, height)` in pixels of the chrome font, used to lay out and render the chrome.
    chrome_cell_size: (f32, f32),
    /// Chrome actions collected during the last frame, drained by the window.
    chrome_actions: ChromeActions,
    /// Physical-pixel height reserved at the top for the tab bar (0 when hidden).
    chrome_bar_height: f32,
    /// Physical-pixel width reserved on the left for the project sidebar (0 when hidden).
    chrome_sidebar_width: f32,
    /// Physical-pixel height reserved at the bottom for the git status bar (0 when hidden).
    chrome_status_height: f32,
    /// Physical-pixel width reserved on the right for the project pane (0 when hidden).
    chrome_pane_width: f32,
}

impl Display {
    pub fn new(
        window: Window,
        gl_context: NotCurrentContext,
        config: &UiConfig,
        _tabbed: bool,
        _event_loop: &ActiveEventLoop,
    ) -> Result<Display, Error> {
        let raw_window_handle = window.raw_window_handle();

        let scale_factor = window.scale_factor as f32;
        let rasterizer = Rasterizer::new()?;

        let font_size = config.font.size().scale(scale_factor);
        debug!("Loading \"{}\" font", &config.font.normal().family);
        let font = config.font.clone().with_size(font_size);
        let mut glyph_cache = GlyphCache::new(rasterizer, &font)?;

        let metrics = glyph_cache.font_metrics();
        let (cell_width, cell_height) = compute_cell_size(config, &metrics);

        // A second glyph cache for the window chrome, at a fixed UI size (Zed-style) that is
        // independent of the terminal font and tracks only the window scale factor.
        let chrome_rasterizer = Rasterizer::new()?;
        let chrome_size = FontSize::new(chrome::CHROME_FONT_PT).scale(scale_factor);
        let chrome_font = config.font.clone().with_size(chrome_size);
        let mut chrome_glyph_cache = GlyphCache::new(chrome_rasterizer, &chrome_font)?;
        let chrome_cell_size = chrome_cell_size(&chrome_glyph_cache.font_metrics());

        // Resize the window to account for the user configured size.
        if let Some(dimensions) = config.window.dimensions() {
            let size = window_size(config, dimensions, cell_width, cell_height, scale_factor);
            window.request_inner_size(size);
        }

        // Create the GL surface to draw into.
        let surface = platform::create_gl_surface(
            &gl_context,
            window.inner_size(),
            window.raw_window_handle(),
        )?;

        // Make the context current.
        let context = gl_context.make_current(&surface)?;

        // Create renderer.
        let mut renderer = Renderer::new(&context, config.debug.renderer)?;

        // Load font common glyphs to accelerate rendering.
        debug!("Filling glyph cache with common glyphs");
        renderer.with_loader(|mut api| {
            glyph_cache.reset_glyph_cache(&mut api);
        });
        // Add the chrome font's common glyphs into the shared atlas (without clearing it).
        renderer.with_loader(|mut api| {
            chrome_glyph_cache.load_common_glyphs(&mut api);
        });

        let padding = config.window.padding(window.scale_factor as f32);
        let viewport_size = window.inner_size();

        // Create new size with at least one column and row.
        let size_info = SizeInfo::new(
            viewport_size.width as f32,
            viewport_size.height as f32,
            cell_width,
            cell_height,
            padding.0,
            padding.1,
            config.window.dynamic_padding && config.window.dimensions().is_none(),
        );

        info!("Cell size: {cell_width} x {cell_height}");
        info!("Padding: {} x {}", size_info.padding_x(), size_info.padding_y());
        info!("Width: {}, Height: {}", size_info.width(), size_info.height());

        // Update OpenGL projection.
        renderer.resize(&size_info);

        // Clear screen.
        let background_color = config.colors.primary.background;
        renderer.clear(background_color, config.window_opacity());

        // Disable shadows for transparent windows on macOS.
        #[cfg(target_os = "macos")]
        window.set_has_shadow(config.window_opacity() >= 1.0);

        let is_wayland = matches!(raw_window_handle, RawWindowHandle::Wayland(_));

        // On Wayland we can safely ignore this call, since the window isn't visible until you
        // actually draw something into it and commit those changes.
        if !is_wayland {
            surface.swap_buffers(&context).expect("failed to swap buffers.");
            renderer.finish();
        }

        // Set resize increments for the newly created window.
        if config.window.resize_increments {
            window.set_resize_increments(PhysicalSize::new(cell_width, cell_height));
        }

        window.set_visible(true);

        // Always focus new windows, even if no Kairos window is currently focused.
        #[cfg(target_os = "macos")]
        window.focus_window();

        #[allow(clippy::single_match)]
        #[cfg(not(windows))]
        if !_tabbed {
            match config.window.startup_mode {
                #[cfg(target_os = "macos")]
                StartupMode::SimpleFullscreen => window.set_simple_fullscreen(true),
                StartupMode::Maximized if !is_wayland => window.set_maximized(true),
                _ => (),
            }
        }

        let hint_state = HintState::new(config.hints.alphabet());

        let mut damage_tracker = DamageTracker::new(size_info.screen_lines(), size_info.columns());
        damage_tracker.debug = config.debug.highlight_damage;

        // Disable vsync.
        if let Err(err) = surface.set_swap_interval(&context, SwapInterval::DontWait) {
            info!("Failed to disable vsync: {err}");
        }

        Ok(Self {
            chrome: Chrome::new(),
            chrome_base: config.font.clone(),
            chrome_glyph_cache,
            chrome_cell_size,
            chrome_actions: ChromeActions::default(),
            chrome_bar_height: 0.,
            chrome_sidebar_width: 0.,
            chrome_status_height: 0.,
            chrome_pane_width: 0.,
            context: ManuallyDrop::new(context),
            visual_bell: VisualBell::from(&config.bell),
            renderer: ManuallyDrop::new(renderer),
            renderer_preference: config.debug.renderer,
            surface: ManuallyDrop::new(surface),
            colors: List::from(&config.colors),
            frame_timer: FrameTimer::new(),
            raw_window_handle,
            damage_tracker,
            glyph_cache,
            hint_state,
            size_info,
            font_size,
            window,
            pending_renderer_update: Default::default(),
            vi_highlighted_hint_age: Default::default(),
            highlighted_hint_age: Default::default(),
            vi_highlighted_hint: Default::default(),
            highlighted_hint: Default::default(),
            hint_mouse_point: Default::default(),
            pending_update: Default::default(),
            cursor_hidden: Default::default(),
            meter: Default::default(),
            ime: Default::default(),
        })
    }

    /// Open the right-click context menu anchored at the given window pixel position.
    pub fn open_context_menu(&mut self, x: usize, y: usize) {
        self.chrome.context_menu = Some((x as f32, y as f32));
    }

    #[inline]
    pub fn gl_context(&self) -> &PossiblyCurrentContext {
        &self.context
    }

    pub fn make_not_current(&mut self) {
        if self.context.is_current() {
            self.context.make_not_current_in_place().expect("failed to disable context");
        }
    }

    pub fn make_current(&mut self) {
        let is_current = self.context.is_current();

        // Attempt to make the context current if it's not.
        let context_loss = if is_current {
            self.renderer.was_context_reset()
        } else {
            match self.context.make_current(&self.surface) {
                Err(err) if err.error_kind() == ErrorKind::ContextLost => {
                    info!("Context lost for window {:?}", self.window.id());
                    true
                },
                _ => false,
            }
        };

        if !context_loss {
            return;
        }

        let gl_display = self.context.display();
        let gl_config = self.context.config();
        let raw_window_handle = Some(self.window.raw_window_handle());
        let context = platform::create_gl_context(&gl_display, &gl_config, raw_window_handle)
            .expect("failed to recreate context.");

        // Drop the old context and renderer.
        unsafe {
            ManuallyDrop::drop(&mut self.renderer);
            ManuallyDrop::drop(&mut self.context);
        }

        // Activate new context.
        let context = context.treat_as_possibly_current();
        self.context = ManuallyDrop::new(context);
        self.context.make_current(&self.surface).expect("failed to reativate context after reset.");

        // Recreate renderer.
        let renderer = Renderer::new(&self.context, self.renderer_preference)
            .expect("failed to recreate renderer after reset");
        self.renderer = ManuallyDrop::new(renderer);

        // Resize the renderer.
        self.renderer.resize(&self.size_info);

        self.reset_glyph_cache();
        self.damage_tracker.frame().mark_fully_damaged();

        debug!("Recovered window {:?} from gpu reset", self.window.id());
    }

    fn swap_buffers(&self) {
        #[allow(clippy::single_match)]
        let res = match (self.surface.deref(), &self.context.deref()) {
            #[cfg(not(any(target_os = "macos", windows)))]
            (Surface::Egl(surface), PossiblyCurrentContext::Egl(context))
                if matches!(self.raw_window_handle, RawWindowHandle::Wayland(_))
                    && !self.damage_tracker.debug =>
            {
                let damage = self.damage_tracker.shape_frame_damage(self.size_info.into());
                surface.swap_buffers_with_damage(context, &damage)
            },
            (surface, context) => surface.swap_buffers(context),
        };
        if let Err(err) = res {
            debug!("error calling swap_buffers: {err}");
        }
    }

    /// Update font size and cell dimensions.
    ///
    /// This will return a tuple of the cell width and height.
    fn update_font_size(
        glyph_cache: &mut GlyphCache,
        config: &UiConfig,
        font: &Font,
    ) -> (f32, f32) {
        let _ = glyph_cache.update_font_size(font);

        // Compute new cell sizes.
        compute_cell_size(config, &glyph_cache.font_metrics())
    }

    /// Reset glyph cache.
    fn reset_glyph_cache(&mut self) {
        // Clears the shared atlas and reloads the terminal font's common glyphs.
        let cache = &mut self.glyph_cache;
        self.renderer.with_loader(|mut api| {
            cache.reset_glyph_cache(&mut api);
        });
        // Re-add the chrome font's glyphs into the now-cleared atlas (without clearing it again).
        let chrome_cache = &mut self.chrome_glyph_cache;
        self.renderer.with_loader(|mut api| {
            chrome_cache.reload(&mut api);
        });
    }

    // XXX: this function must not call to any `OpenGL` related tasks. Renderer updates are
    // performed in [`Self::process_renderer_update`] right before drawing.
    //
    /// Process update events, recomputing the window-level [`SizeInfo`].
    ///
    /// Terminal and PTY resizing is the caller's responsibility: every pane derives its own
    /// geometry from [`Self::content_rect`] and is resized by the window's relayout pass.
    pub fn handle_update(&mut self, message_buffer: &MessageBuffer, config: &UiConfig) {
        let pending_update = mem::take(&mut self.pending_update);

        let (mut cell_width, mut cell_height) =
            (self.size_info.cell_width(), self.size_info.cell_height());

        if pending_update.font().is_some() || pending_update.cursor_dirty() {
            let renderer_update = self.pending_renderer_update.get_or_insert(Default::default());
            renderer_update.clear_font_cache = true
        }

        // Update font size and cell dimensions.
        if let Some(font) = pending_update.font() {
            let cell_dimensions = Self::update_font_size(&mut self.glyph_cache, config, font);
            cell_width = cell_dimensions.0;
            cell_height = cell_dimensions.1;

            // Keep the chrome font at its fixed UI size, tracking only the window scale factor
            // so it stays crisp across DPI changes — it must not follow terminal font changes.
            let scale = self.window.scale_factor as f32;
            let chrome_size = FontSize::new(chrome::CHROME_FONT_PT).scale(scale);
            let chrome_font = self.chrome_base.clone().with_size(chrome_size);
            let _ = self.chrome_glyph_cache.update_font_size(&chrome_font);
            self.chrome_cell_size = chrome_cell_size(&self.chrome_glyph_cache.font_metrics());

            info!("Cell size: {cell_width} x {cell_height}");

            // Mark entire terminal as damaged since glyph size could change without cell size
            // changes.
            self.damage_tracker.frame().mark_fully_damaged();
        }

        let (mut width, mut height) = (self.size_info.width(), self.size_info.height());
        if let Some(dimensions) = pending_update.dimensions() {
            width = dimensions.width as f32;
            height = dimensions.height as f32;
        }

        let padding = config.window.padding(self.window.scale_factor as f32);

        let mut new_size = SizeInfo::new(
            width,
            height,
            cell_width,
            cell_height,
            padding.0,
            padding.1,
            config.window.dynamic_padding,
        );

        // Reserve rows at the bottom for the message bar. The search bar is a per-pane concern,
        // reserved inside the searching pane by the relayout pass.
        let message_bar_lines = message_buffer.message().map_or(0, |m| m.text(&new_size).len());
        new_size.reserve_lines(message_bar_lines);

        // Reserve space at the top for the tab bar.
        new_size.reserve_top_px(self.chrome_bar_height);

        // Reserve space at the left for the project sidebar.
        new_size.reserve_left_cols(self.chrome_sidebar_width);

        // Reserve space at the right for the project pane.
        new_size.reserve_right_cols(self.chrome_pane_width);

        // Reserve space at the bottom for the git status bar.
        new_size.reserve_bottom_px(self.chrome_status_height);

        // Update resize increments.
        if config.window.resize_increments {
            self.window.set_resize_increments(PhysicalSize::new(cell_width, cell_height));
        }

        // Check if dimensions have changed.
        if new_size != self.size_info {
            // Queue renderer update.
            let renderer_update = self.pending_renderer_update.get_or_insert(Default::default());
            renderer_update.resize = true;

            // The context menu's anchor is tied to the old layout; close it on resize.
            self.chrome.context_menu = None;
        }
        self.size_info = new_size;
    }

    /// The window-pixel rectangle shared by the active tab's panes: the window minus the chrome
    /// reservations (tab bar, sidebar, status bar) and the message-bar rows.
    ///
    /// The height is expressed as whole grid rows plus padding so a single full-content pane
    /// reproduces exactly the window-level grid, and the message bar band (drawn in window-level
    /// grid rows right below [`Self::size_info`]'s `screen_lines`) never underlaps a pane.
    pub fn content_rect(&self) -> PixelRect {
        let size_info = &self.size_info;
        PixelRect {
            x: size_info.left_extra(),
            y: size_info.top_extra(),
            w: size_info.width() - size_info.left_extra(),
            h: 2. * size_info.padding_y()
                + size_info.screen_lines() as f32 * size_info.cell_height(),
        }
    }

    // NOTE: Renderer updates are split off, since platforms like Wayland require resize and other
    // OpenGL operations to be performed right before rendering. Otherwise they could lock the
    // back buffer and render with the previous state. This also solves flickering during resizes.
    //
    /// Update the state of the renderer.
    pub fn process_renderer_update(&mut self) {
        let renderer_update = match self.pending_renderer_update.take() {
            Some(renderer_update) => renderer_update,
            _ => return,
        };

        // Resize renderer.
        if renderer_update.resize {
            let width = NonZeroU32::new(self.size_info.width() as u32).unwrap();
            let height = NonZeroU32::new(self.size_info.height() as u32).unwrap();
            self.surface.resize(&self.context, width, height);
        }

        // Ensure we're modifying the correct OpenGL context.
        self.make_current();

        if renderer_update.clear_font_cache {
            self.reset_glyph_cache();
        }

        self.renderer.resize(&self.size_info);

        info!("Padding: {} x {}", self.size_info.padding_x(), self.size_info.padding_y());
        info!("Width: {}, Height: {}", self.size_info.width(), self.size_info.height());
    }

    /// Start drawing a frame: activate the GL context and clear the window.
    ///
    /// With more than one visible pane the whole frame is damaged — pane rects move around on
    /// split/close/drag, making line-based damage from previous frames meaningless.
    pub fn begin_frame(&mut self, background_color: Rgb, config: &UiConfig, multi_pane: bool) {
        // Make sure this window's OpenGL context is active.
        self.make_current();

        if multi_pane {
            self.damage_tracker.frame().mark_fully_damaged();
            self.damage_tracker.next_frame().mark_fully_damaged();
        }

        self.renderer.clear(background_color, config.window_opacity());
    }

    /// Draw one pane's terminal grid and overlays into its sub-rectangle of the window.
    ///
    /// Must be called between [`Self::begin_frame`] and [`Self::end_frame`], once per visible
    /// pane, with the focused pane last. `single_pane` selects the partial-damage path that is
    /// only sound when this pane is the frame's sole content.
    pub fn draw_pane<T: EventListener>(
        &mut self,
        mut terminal: MutexGuard<'_, Term<T>>,
        size_info: SizeInfo,
        search_state: &mut SearchState,
        focused: bool,
        single_pane: bool,
        config: &UiConfig,
    ) {
        // Collect renderable content before the terminal is dropped.
        let mut content =
            RenderableContent::new(config, self, &terminal, search_state, &size_info, focused);
        let mut grid_cells = Vec::new();
        for cell in &mut content {
            grid_cells.push(cell);
        }
        let selection_range = content.selection_range();
        let foreground_color = content.color(NamedColor::Foreground as usize);
        let background_color = content.color(NamedColor::Background as usize);
        let display_offset = content.display_offset();
        let cursor = content.cursor();

        let cursor_point = terminal.grid().cursor.point;
        let total_lines = terminal.grid().total_lines();
        let metrics = self.glyph_cache.font_metrics();

        let vi_mode = terminal.mode().contains(TermMode::VI);
        let vi_cursor_point = if vi_mode { Some(terminal.vi_mode_cursor.point) } else { None };

        // Add damage from the terminal. In multi-pane frames everything is already fully
        // damaged; the terminal's damage is merely consumed.
        if single_pane {
            match terminal.damage() {
                TermDamage::Full => self.damage_tracker.frame().mark_fully_damaged(),
                TermDamage::Partial(damaged_lines) => {
                    for damage in damaged_lines {
                        self.damage_tracker.frame().damage_line(damage);
                    }
                },
            }
        }
        terminal.reset_damage();

        // Drop terminal as early as possible to free lock.
        drop(terminal);

        let vi_cursor_viewport_point =
            vi_cursor_point.and_then(|cursor| term::point_to_viewport(display_offset, cursor));

        if focused {
            // Invalidate highlighted hints if grid has changed.
            self.validate_hint_highlights(display_offset);
        }

        if single_pane {
            // Add damage from kairos's UI elements overlapping terminal.
            let requires_full_damage = self.visual_bell.intensity() != 0.
                || self.hint_state.active()
                || search_state.regex().is_some();
            if requires_full_damage {
                self.damage_tracker.frame().mark_fully_damaged();
                self.damage_tracker.next_frame().mark_fully_damaged();
            }

            self.damage_tracker.damage_vi_cursor(vi_cursor_viewport_point);
            self.damage_tracker.damage_selection(selection_range, display_offset);
        }

        // Point the GL viewport and text projection at this pane's rectangle.
        self.renderer.resize(&size_info);

        let mut lines = RenderLines::new();

        // Optimize loop hint comparator. Hint underlines follow the focused pane (mouse-hover
        // hints across panes are resolved before the highlight is set).
        let has_highlighted_hint = focused
            && (self.highlighted_hint.is_some() || self.vi_highlighted_hint.is_some());

        // Draw grid.
        {
            let _sampler = self.meter.sampler();

            let glyph_cache = &mut self.glyph_cache;
            let highlighted_hint = &self.highlighted_hint;
            let vi_highlighted_hint = &self.vi_highlighted_hint;
            let damage_tracker = &mut self.damage_tracker;

            let cells = grid_cells.into_iter().map(|mut cell| {
                // Underline hints hovered by mouse or vi mode cursor.
                if has_highlighted_hint {
                    let point = term::viewport_to_point(display_offset, cell.point);
                    let hyperlink = cell.extra.as_ref().and_then(|extra| extra.hyperlink.as_ref());

                    let should_highlight = |hint: &Option<HintMatch>| {
                        hint.as_ref().is_some_and(|hint| hint.should_highlight(point, hyperlink))
                    };
                    if should_highlight(highlighted_hint) || should_highlight(vi_highlighted_hint) {
                        damage_tracker.frame().damage_point(cell.point);
                        cell.flags.insert(Flags::UNDERLINE);
                    }
                }

                // Update underline/strikeout.
                lines.update(&cell);

                cell
            });
            self.renderer.draw_cells(&size_info, glyph_cache, cells);
        }

        let mut rects = lines.rects(&metrics, &size_info);

        if focused {
            if let Some(vi_cursor_point) = vi_cursor_point {
                // Indicate vi mode by showing the cursor's position in the top right corner.
                let line = (-vi_cursor_point.line.0 + size_info.bottommost_line().0) as usize;
                let obstructed_column = Some(vi_cursor_point)
                    .filter(|point| point.line == -(display_offset as i32))
                    .map(|point| point.column);
                self.draw_line_indicator(config, &size_info, total_lines, obstructed_column, line);
            } else if search_state.regex().is_some() {
                // Show current display offset in vi-less search to indicate match position.
                self.draw_line_indicator(config, &size_info, total_lines, None, display_offset);
            }
        }

        // Draw cursor.
        rects.extend(cursor.rects(&size_info, config.cursor.thickness()));

        // Search bar and IME positioning. The bar renders in any searching pane (its bottom row
        // was reserved by the relayout pass); the input caret and IME belong to the focused pane.
        let mut ime_position = None;
        if let Some(regex) = search_state.regex() {
            let search_label = match search_state.direction() {
                Direction::Right => FORWARD_SEARCH_LABEL,
                Direction::Left => BACKWARD_SEARCH_LABEL,
            };

            let search_text = Self::format_search(regex, search_label, size_info.columns());

            // Render the search bar.
            self.draw_search(config, &size_info, &search_text);

            if focused {
                let line = size_info.screen_lines();
                let column = Column(search_text.chars().count() - 1);

                // Add cursor to search bar if IME is not active.
                if self.ime.preedit().is_none() {
                    let fg = config.colors.footer_bar_foreground();
                    let shape = CursorShape::Underline;
                    let cursor_width = NonZeroU32::new(1).unwrap();
                    let cursor =
                        RenderableCursor::new(Point::new(line, column), shape, fg, cursor_width);
                    rects.extend(cursor.rects(&size_info, config.cursor.thickness()));
                }

                ime_position = Some(Point::new(line, column));
            }
        } else if focused {
            let num_lines = size_info.screen_lines();
            ime_position = match vi_cursor_viewport_point {
                None => term::point_to_viewport(display_offset, cursor_point)
                    .filter(|point| point.line < num_lines),
                point => point,
            };
        }

        // Handle IME.
        if focused && self.ime.is_enabled() {
            if let Some(point) = ime_position {
                let (fg, bg) = if search_state.regex().is_some() {
                    (config.colors.footer_bar_foreground(), config.colors.footer_bar_background())
                } else {
                    (foreground_color, background_color)
                };

                self.draw_ime_preview(point, fg, bg, &mut rects, config, &size_info);
            }
        }

        // Draw rectangles.
        self.renderer.draw_rects(&size_info, &metrics, rects);

        // Draw hyperlink uri preview.
        if has_highlighted_hint {
            let cursor_point = vi_cursor_point.or(Some(cursor_point));
            self.draw_hyperlink_preview(config, &size_info, cursor_point, display_offset);
        }
    }

    /// Finish a frame: window-level overlays (pane dividers, visual bell, message bar, chrome)
    /// and buffer swap.
    #[allow(clippy::too_many_arguments)]
    pub fn end_frame(
        &mut self,
        scheduler: &mut Scheduler,
        message_buffer: &MessageBuffer,
        config: &UiConfig,
        tab_bar: &TabBarInfo,
        pane_data: Option<&ProjectPaneData>,
        dividers: &[PixelRect],
    ) {
        let metrics = self.glyph_cache.font_metrics();
        let size_info = self.size_info;

        // Restore the window-level viewport and projection after the per-pane passes.
        self.renderer.resize(&size_info);

        // Pane dividers and the visual bell live in absolute window pixels.
        let mut window_rects = Vec::new();
        let divider_color = self.chrome.divider_color();
        for divider in dividers {
            window_rects.push(RenderRect::new(
                divider.x,
                divider.y,
                divider.w,
                divider.h,
                divider_color,
                1.,
            ));
        }

        let visual_bell_intensity = self.visual_bell.intensity();
        if visual_bell_intensity != 0. {
            window_rects.push(RenderRect::new(
                0.,
                0.,
                size_info.width(),
                size_info.height(),
                config.bell.color,
                visual_bell_intensity as f32,
            ));
        }

        if !window_rects.is_empty() {
            self.renderer.draw_chrome_rects(&size_info, &metrics, window_rects);
        }

        // The message bar spans the window-level grid rows right below `screen_lines`.
        if let Some(message) = message_buffer.message() {
            let text = message.text(&size_info);

            // Create a new rectangle for the background.
            let start_line = size_info.screen_lines();
            let y = size_info.cell_height().mul_add(start_line as f32, size_info.padding_y());

            let bg = match message.ty() {
                MessageType::Error => config.colors.normal.red,
                MessageType::Warning => config.colors.normal.yellow,
            };

            let x = 0;
            let width = size_info.width() as i32;
            let height = (size_info.height() - y) as i32;
            let message_bar_rect =
                RenderRect::new(x as f32, y, width as f32, height as f32, bg, 1.);

            // Always damage message bar, since it could have messages of the same size in it.
            self.damage_tracker.frame().add_viewport_rect(&size_info, x, y as i32, width, height);

            // Draw rectangles.
            self.renderer.draw_rects(&size_info, &metrics, vec![message_bar_rect]);

            // Relay messages to the user.
            let glyph_cache = &mut self.glyph_cache;
            let fg = config.colors.primary.background;
            for (i, message_text) in text.iter().enumerate() {
                let point = Point::new(start_line + i, Column(0));
                self.renderer.draw_string(
                    point,
                    fg,
                    bg,
                    message_text.chars(),
                    &size_info,
                    glyph_cache,
                );
            }
        }

        self.draw_render_timer(config);

        // Notify winit that we're about to present.
        self.window.pre_present_notify();

        // Highlight damage for debugging.
        if self.damage_tracker.debug {
            let damage = self.damage_tracker.shape_frame_damage(self.size_info.into());
            let mut rects = Vec::with_capacity(damage.len());
            self.highlight_damage(&mut rects);
            self.renderer.draw_rects(&self.size_info, &metrics, rects);
        }

        // Render the native chrome (tab bar, sidebar, project pane, context menu) over the
        // terminal.
        self.draw_chrome(tab_bar, pane_data);

        // Clearing debug highlights from the previous frame requires full redraw.
        self.swap_buffers();

        if matches!(self.raw_window_handle, RawWindowHandle::Xcb(_) | RawWindowHandle::Xlib(_)) {
            // On X11 `swap_buffers` does not block for vsync. However the next OpenGl command
            // will block to synchronize (this is `glClear` in Kairos), which causes a
            // permanent one frame delay.
            self.renderer.finish();
        }

        // XXX: Request the new frame after swapping buffers, so the
        // time to finish OpenGL operations is accounted for in the timeout.
        if !matches!(self.raw_window_handle, RawWindowHandle::Wayland(_)) {
            self.request_frame(scheduler);
        }

        self.damage_tracker.swap_damage();
    }

    /// Update to a new configuration.
    pub fn update_config(&mut self, config: &UiConfig) {
        self.damage_tracker.debug = config.debug.highlight_damage;
        self.visual_bell.update_config(&config.bell);
        self.colors = List::from(&config.colors);
    }

    /// Update the mouse/vi mode cursor hint highlighting.
    ///
    /// `size_info` must describe the geometry of `term`'s pane — the mouse position is mapped
    /// through it into that grid.
    ///
    /// This will return whether the highlighted hints changed.
    pub fn update_highlighted_hints<T>(
        &mut self,
        term: &Term<T>,
        config: &UiConfig,
        mouse: &Mouse,
        modifiers: ModifiersState,
        size_info: &SizeInfo,
    ) -> bool {
        // Update vi mode cursor hint.
        let vi_highlighted_hint = if term.mode().contains(TermMode::VI) {
            let mods = ModifiersState::all();
            let point = term.vi_mode_cursor.point;
            hint::highlighted_at(term, config, point, mods)
        } else {
            None
        };
        let mut dirty = vi_highlighted_hint != self.vi_highlighted_hint;
        self.vi_highlighted_hint = vi_highlighted_hint;
        self.vi_highlighted_hint_age = 0;

        // Force full redraw if the vi mode highlight was cleared.
        if dirty {
            self.damage_tracker.frame().mark_fully_damaged();
        }

        // Abort if mouse highlighting conditions are not met.
        if !self.window.mouse_visible()
            || !mouse.inside_text_area
            || !term.selection.as_ref().is_none_or(Selection::is_empty)
        {
            if self.highlighted_hint.take().is_some() {
                self.damage_tracker.frame().mark_fully_damaged();
                dirty = true;
            }
            return dirty;
        }

        // Find highlighted hint at mouse position.
        let point = mouse.point(size_info, term.grid().display_offset());
        let highlighted_hint = hint::highlighted_at(term, config, point, modifiers);

        // Update cursor shape.
        if highlighted_hint.is_some() {
            // If mouse changed the line, we should update the hyperlink preview, since the
            // highlighted hint could be disrupted by the old preview.
            dirty = self.hint_mouse_point.is_some_and(|p| p.line != point.line);
            self.hint_mouse_point = Some(point);
            self.window.set_mouse_cursor(CursorIcon::Pointer);
        } else if self.highlighted_hint.is_some() {
            self.hint_mouse_point = None;
            if term.mode().intersects(TermMode::MOUSE_MODE) && !term.mode().contains(TermMode::VI) {
                self.window.set_mouse_cursor(CursorIcon::Default);
            } else {
                self.window.set_mouse_cursor(CursorIcon::Text);
            }
        }

        let mouse_highlight_dirty = self.highlighted_hint != highlighted_hint;
        dirty |= mouse_highlight_dirty;
        self.highlighted_hint = highlighted_hint;
        self.highlighted_hint_age = 0;

        // Force full redraw if the mouse cursor highlight was changed.
        if mouse_highlight_dirty {
            self.damage_tracker.frame().mark_fully_damaged();
        }

        dirty
    }

    #[inline(never)]
    fn draw_ime_preview(
        &mut self,
        point: Point<usize>,
        fg: Rgb,
        bg: Rgb,
        rects: &mut Vec<RenderRect>,
        config: &UiConfig,
        size_info: &SizeInfo,
    ) {
        let preedit = match self.ime.preedit() {
            Some(preedit) => preedit,
            None => {
                // In case we don't have preedit, just set the popup point.
                self.window.update_ime_position(point, size_info);
                return;
            },
        };

        let num_cols = size_info.columns();

        // Get the visible preedit.
        let visible_text: String = match (preedit.cursor_byte_offset, preedit.cursor_end_offset) {
            (Some(byte_offset), Some(end_offset)) if end_offset.0 > num_cols => StrShortener::new(
                &preedit.text[byte_offset.0..],
                num_cols,
                ShortenDirection::Right,
                Some(SHORTENER),
            ),
            _ => {
                StrShortener::new(&preedit.text, num_cols, ShortenDirection::Left, Some(SHORTENER))
            },
        }
        .collect();

        let visible_len = visible_text.chars().count();

        let end = cmp::min(point.column.0 + visible_len, num_cols);
        let start = end.saturating_sub(visible_len);

        let start = Point::new(point.line, Column(start));
        let end = Point::new(point.line, Column(end - 1));

        let glyph_cache = &mut self.glyph_cache;
        let metrics = glyph_cache.font_metrics();

        self.renderer.draw_string(start, fg, bg, visible_text.chars(), size_info, glyph_cache);

        // Damage preedit inside the terminal viewport.
        if point.line < size_info.screen_lines() {
            let damage = LineDamageBounds::new(start.line, 0, num_cols);
            self.damage_tracker.frame().damage_line(damage);
            self.damage_tracker.next_frame().damage_line(damage);
        }

        // Add underline for preedit text.
        let underline = RenderLine { start, end, color: fg };
        rects.extend(underline.rects(Flags::UNDERLINE, &metrics, size_info));

        let ime_popup_point = match preedit.cursor_end_offset {
            Some(cursor_end_offset) => {
                // Use hollow block when multiple characters are changed at once.
                let (shape, width) = if let Some(width) =
                    NonZeroU32::new((cursor_end_offset.0 - cursor_end_offset.1) as u32)
                {
                    (CursorShape::HollowBlock, width)
                } else {
                    (CursorShape::Beam, NonZeroU32::new(1).unwrap())
                };

                let cursor_column = Column(
                    (end.column.0 as isize - cursor_end_offset.0 as isize + 1).max(0) as usize,
                );
                let cursor_point = Point::new(point.line, cursor_column);
                let cursor = RenderableCursor::new(cursor_point, shape, fg, width);
                rects.extend(cursor.rects(size_info, config.cursor.thickness()));
                cursor_point
            },
            _ => end,
        };

        self.window.update_ime_position(ime_popup_point, size_info);
    }

    /// Format search regex to account for the cursor and fullwidth characters.
    fn format_search(search_regex: &str, search_label: &str, max_width: usize) -> String {
        let label_len = search_label.len();

        // Skip `search_regex` formatting if only label is visible.
        if label_len > max_width {
            return search_label[..max_width].to_owned();
        }

        // The search string consists of `search_label` + `search_regex` + `cursor`.
        let mut bar_text = String::from(search_label);
        bar_text.extend(StrShortener::new(
            search_regex,
            max_width.wrapping_sub(label_len + 1),
            ShortenDirection::Left,
            Some(SHORTENER),
        ));

        // Add place for cursor.
        bar_text.push(' ');

        bar_text
    }

    /// Draw preview for the currently highlighted `Hyperlink`.
    #[inline(never)]
    fn draw_hyperlink_preview(
        &mut self,
        config: &UiConfig,
        size_info: &SizeInfo,
        cursor_point: Option<Point>,
        display_offset: usize,
    ) {
        let num_cols = size_info.columns();
        let uris: Vec<_> = self
            .highlighted_hint
            .iter()
            .chain(&self.vi_highlighted_hint)
            .filter_map(|hint| hint.hyperlink().map(|hyperlink| hyperlink.uri()))
            .map(|uri| StrShortener::new(uri, num_cols, ShortenDirection::Right, Some(SHORTENER)))
            .collect();

        if uris.is_empty() {
            return;
        }

        // The maximum amount of protected lines including the ones we'll show preview on.
        let max_protected_lines = uris.len() * 2;

        // Lines we shouldn't show preview on, because it'll obscure the highlighted hint.
        let mut protected_lines = Vec::with_capacity(max_protected_lines);
        if size_info.screen_lines() > max_protected_lines {
            // Prefer to show preview even when it'll likely obscure the highlighted hint, when
            // there's no place left for it.
            protected_lines.push(self.hint_mouse_point.map(|point| point.line));
            protected_lines.push(cursor_point.map(|point| point.line));
        }

        // Find the line in viewport we can draw preview on without obscuring protected lines.
        let viewport_bottom = size_info.bottommost_line() - Line(display_offset as i32);
        let viewport_top = viewport_bottom - (size_info.screen_lines() - 1);
        let uri_lines = (viewport_top.0..=viewport_bottom.0)
            .rev()
            .map(|line| Some(Line(line)))
            .filter_map(|line| {
                if protected_lines.contains(&line) {
                    None
                } else {
                    protected_lines.push(line);
                    line
                }
            })
            .take(uris.len())
            .flat_map(|line| term::point_to_viewport(display_offset, Point::new(line, Column(0))));

        let fg = config.colors.footer_bar_foreground();
        let bg = config.colors.footer_bar_background();
        for (uri, point) in uris.into_iter().zip(uri_lines) {
            // Damage the uri preview.
            let damage = LineDamageBounds::new(point.line, point.column.0, num_cols);
            self.damage_tracker.frame().damage_line(damage);

            // Damage the uri preview for the next frame as well.
            self.damage_tracker.next_frame().damage_line(damage);

            self.renderer.draw_string(point, fg, bg, uri, size_info, &mut self.glyph_cache);
        }
    }

    /// Draw current search regex.
    #[inline(never)]
    fn draw_search(&mut self, config: &UiConfig, size_info: &SizeInfo, text: &str) {
        // Assure text length is at least num_cols.
        let num_cols = size_info.columns();
        let text = format!("{text:<num_cols$}");

        let point = Point::new(size_info.screen_lines(), Column(0));

        let fg = config.colors.footer_bar_foreground();
        let bg = config.colors.footer_bar_background();

        self.renderer.draw_string(point, fg, bg, text.chars(), size_info, &mut self.glyph_cache);
    }

    /// Offer a winit window event to the native chrome. Returns whether the chrome consumed it
    /// (in which case it must not also reach the terminal).
    pub fn handle_chrome_event(&mut self, event: &winit::event::WindowEvent) -> bool {
        use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
        use winit::keyboard::{Key, NamedKey};

        match event {
            WindowEvent::CursorMoved { position, .. } => {
                let (x, y) = (position.x as f32, position.y as f32);
                if self.chrome.set_mouse(x, y) {
                    self.pending_update.dirty = true;
                }
                let cw = self.chrome_cell_size.0;
                // While dragging, the sidebar's right edge follows the pointer; keep the resize
                // cursor and consume the move so the terminal doesn't also act on it.
                if self.chrome.is_dragging_divider() {
                    self.chrome.drag_divider_to(x, cw);
                    self.window.set_mouse_cursor(CursorIcon::ColResize);
                    self.pending_update.dirty = true;
                    return true;
                }
                // Same for the project pane's left edge.
                if self.chrome.is_dragging_pane_divider() {
                    self.chrome.drag_pane_divider_to(x, cw);
                    self.window.set_mouse_cursor(CursorIcon::ColResize);
                    self.pending_update.dirty = true;
                    return true;
                }
                // While selecting in the center viewer, the focus follows the pointer.
                if self.chrome.is_dragging_viewer_selection() {
                    self.chrome.update_viewer_selection(x, y);
                    self.window.set_mouse_cursor(CursorIcon::Text);
                    self.pending_update.dirty = true;
                    return true;
                }
                // Hovering a divider shows the resize cursor; consume so the terminal doesn't
                // immediately overwrite it with the text cursor.
                if self.chrome.over_divider(x) || self.chrome.over_pane_divider(x) {
                    self.window.set_mouse_cursor(CursorIcon::ColResize);
                    return true;
                }
                // Over a chrome control (a row, button, tab…): show its cursor — a pointer for
                // clickable controls, the I-beam for the viewer body — and consume so the terminal
                // doesn't immediately reset it to the text cursor.
                if let Some(hit) = self.chrome.hit_mouse() {
                    self.window.set_mouse_cursor(hit.cursor_icon());
                    return true;
                }
                // Never consume other movement; the terminal still tracks the cursor.
                false
            },
            // Scroll the project pane or the center viewer when the wheel turns over them.
            WindowEvent::MouseWheel { delta, .. } => {
                let (x, y) = self.chrome.last_mouse();
                let over_pane = self.chrome.pane_contains(x, y);
                let over_viewer = self.chrome.viewer_contains(x, y);
                if !over_pane && !over_viewer {
                    return false;
                }
                let rows = match delta {
                    MouseScrollDelta::LineDelta(_, lines) => -lines * 3.,
                    MouseScrollDelta::PixelDelta(pos) => {
                        -(pos.y as f32) / self.chrome_cell_size.1.max(1.)
                    },
                };
                if over_pane {
                    self.chrome.scroll_pane(rows);
                } else {
                    self.chrome.scroll_viewer(rows);
                }
                self.pending_update.dirty = true;
                true
            },
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                // Grab a divider before any control hit-test: their zones sit at the panel edges,
                // clear of the rows' buttons.
                let (x, _) = self.chrome.last_mouse();
                if self.chrome.over_divider(x) {
                    self.chrome.begin_divider_drag();
                    self.window.set_mouse_cursor(CursorIcon::ColResize);
                    return true;
                }
                if self.chrome.over_pane_divider(x) {
                    self.chrome.begin_pane_divider_drag();
                    self.window.set_mouse_cursor(CursorIcon::ColResize);
                    return true;
                }
                if let Some(hit) = self.chrome.hit_mouse() {
                    // A press on the viewer body starts a text selection rather than an action;
                    // controls in the viewer header keep their own hits and fall through below.
                    if hit == Hit::ViewerBackground {
                        let (x, y) = self.chrome.last_mouse();
                        self.chrome.begin_viewer_selection(x, y);
                        self.chrome.context_menu = None;
                        self.window.set_mouse_cursor(CursorIcon::Text);
                        self.pending_update.dirty = true;
                        return true;
                    }
                    self.apply_chrome_hit(hit);
                    self.chrome.context_menu = None;
                    self.pending_update.dirty = true;
                    return true;
                }
                // Dismiss any open overlay (context menu, command palette, settings) when clicking
                // away, consuming the click so it doesn't also act on the terminal.
                if self.chrome.close_modals() {
                    self.pending_update.dirty = true;
                    return true;
                }
                // Swallow clicks that land on a chrome surface but not a specific control.
                let (x, y) = self.chrome.last_mouse();
                self.chrome.in_region(x, y)
            },
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                // Only consume the release that ends a divider drag or a viewer selection; let the
                // terminal see every other release (e.g. to finish its own selection).
                let sidebar_drag = self.chrome.end_divider_drag();
                let pane_drag = self.chrome.end_pane_divider_drag();
                let viewer_drag = self.chrome.end_viewer_selection_drag();
                sidebar_drag || pane_drag || viewer_drag
            },
            WindowEvent::KeyboardInput { event: key_event, .. } => {
                let pressed = key_event.state == ElementState::Pressed;
                let key = &key_event.logical_key;
                // While the command palette is open it captures every key: navigate / activate /
                // dismiss / edit the search query, and swallows the rest so nothing leaks into the
                // terminal.
                if self.chrome.palette_open() {
                    if pressed {
                        if *key == Key::Named(NamedKey::Escape) {
                            self.chrome.close_modals();
                        } else if *key == Key::Named(NamedKey::ArrowUp) {
                            self.chrome.palette_move(-1);
                        } else if *key == Key::Named(NamedKey::ArrowDown) {
                            self.chrome.palette_move(1);
                        } else if *key == Key::Named(NamedKey::Enter) {
                            if let Some(cmd) = self.chrome.palette_selected_command() {
                                self.queue_palette_cmd(cmd);
                            }
                            self.chrome.close_palette();
                        } else if *key == Key::Named(NamedKey::Backspace) {
                            self.chrome.palette_backspace();
                        } else if let Some(text) = key_event.text.as_deref() {
                            self.chrome.palette_input(text);
                        }
                        self.pending_update.dirty = true;
                    }
                    return true;
                }
                // While the settings panel is open it captures every key: Escape collapses an open
                // dropdown / unfocuses the size input before closing the panel, Enter commits the
                // size input, and printable text edits it. Everything else is swallowed.
                if self.chrome.settings_open() {
                    if pressed {
                        if *key == Key::Named(NamedKey::Escape) {
                            if !self.chrome.settings_escape() {
                                self.chrome.close_modals();
                            }
                        } else if *key == Key::Named(NamedKey::Enter) {
                            if let Some(state) = self.chrome.settings_commit_size() {
                                self.chrome_actions.settings = Some(state);
                            }
                        } else if *key == Key::Named(NamedKey::Backspace) {
                            self.chrome.settings_backspace();
                        } else if let Some(text) = key_event.text.as_deref() {
                            self.chrome.settings_input(text);
                        }
                        self.pending_update.dirty = true;
                    }
                    return true;
                }
                // Escape closes an open context menu, or hands focus from a viewer tab back to
                // the terminal; only consume the key when it did one of those.
                if pressed
                    && *key == Key::Named(NamedKey::Escape)
                    && (self.chrome.context_menu.take().is_some() || self.chrome.unfocus_viewer())
                {
                    self.pending_update.dirty = true;
                    return true;
                }
                false
            },
            // Route IME composition/commits to the command palette's search box; otherwise (or when
            // the settings panel is open) swallow them so CJK text never leaks into the terminal.
            WindowEvent::Ime(ime) => {
                if self.chrome.palette_open() {
                    match ime {
                        Ime::Commit(text) => self.chrome.palette_input(text),
                        Ime::Preedit(text, _) => self.chrome.palette_preedit(text),
                        _ => {},
                    }
                    self.pending_update.dirty = true;
                    return true;
                }
                // Route IME commits into the size input (digits only survive the filter); swallow
                // everything else so CJK text never leaks into the terminal.
                if self.chrome.settings_open() {
                    if let Ime::Commit(text) = ime {
                        self.chrome.settings_input(text);
                        self.pending_update.dirty = true;
                    }
                    return true;
                }
                false
            },
            _ => false,
        }
    }

    /// Translate a chrome hit into a queued [`ChromeActions`] entry.
    fn apply_chrome_hit(&mut self, hit: Hit) {
        match hit {
            // Selecting or creating a terminal tab takes focus back from the viewer tabs.
            Hit::SelectTab(id) => {
                self.chrome.unfocus_viewer();
                self.chrome_actions.select = Some(id);
            },
            Hit::CloseTab(id) => self.chrome_actions.close = Some(id),
            Hit::CreateTab => {
                self.chrome.unfocus_viewer();
                self.chrome_actions.create = true;
            },
            Hit::SelectProject(i) => self.chrome_actions.select_project = Some(i),
            Hit::CloseProject(i) => self.chrome_actions.close_project = Some(i),
            Hit::CreateProject => self.chrome_actions.create_project = true,
            Hit::OpenClaudeSession(i) => self.chrome_actions.open_claude_session = Some(i),
            // Pane tab switches are pure chrome state; row clicks need project data, so they go
            // through the action queue like every other window-level operation. Row indices are
            // resolved to paths against the same layout pass that registered the hit regions, so
            // a git refresh landing in between can't redirect the click to another entry.
            Hit::PaneShowFiles => self.chrome.pane_set_tab(PaneTab::Files),
            Hit::PaneShowChanges => self.chrome.pane_set_tab(PaneTab::Changes),
            // A tree row is either a directory (expand/collapse) or a file (preview in the
            // center viewer); the snapshots resolve which one was registered at this index.
            Hit::PaneFileRow(i) => {
                if let Some(dir) = self.chrome.pane_dir_path(i) {
                    self.chrome_actions.pane_toggle_dir = Some(dir);
                } else if let Some(file) = self.chrome.pane_file_path(i) {
                    self.chrome_actions.pane_preview = Some(file);
                }
            },
            Hit::PaneChangeRow(i) => {
                self.chrome_actions.pane_toggle_diff = self.chrome.pane_change_path(i);
            },
            Hit::PaneRefresh => self.chrome_actions.pane_refresh = true,
            Hit::ViewerTab(kind) => self.chrome.focus_viewer(kind),
            Hit::ViewerTabClose(kind) => self.chrome.close_viewer_tab(kind),
            Hit::ViewerClose => {
                self.chrome.close_focused_viewer();
            },
            // Layout/engine switches go through the action queue: difftastic renderings are
            // produced per (file, layout) by the git worker, so the window may need to queue a
            // request when the variant isn't cached yet.
            Hit::ViewerUnified => self.chrome_actions.viewer_layout = Some(false),
            Hit::ViewerSplit => self.chrome_actions.viewer_layout = Some(true),
            Hit::ViewerDifft => self.chrome_actions.viewer_toggle_difft = true,
            Hit::ViewerMdSource => self.chrome_actions.viewer_toggle_md_source = true,
            // Clicks on the viewer's body are consumed so they never reach the terminal.
            Hit::ViewerBackground => {},
            Hit::Copy => self.chrome_actions.copy = true,
            Hit::Paste => self.chrome_actions.paste = true,
            Hit::PaletteSelect(i) => {
                if let Some(cmd) = self.chrome.palette_command_at(i) {
                    self.queue_palette_cmd(cmd);
                }
                self.chrome.close_palette();
            },
            // A click on a settings control: mutate the panel state and queue the new snapshot
            // when a configured value changed (dropdown toggles and focus changes just redraw).
            Hit::SettingsFamilyField
            | Hit::SettingsFamilyOption(_)
            | Hit::SettingsSizeField
            | Hit::SettingsShellField
            | Hit::SettingsShellOption(_) => {
                if let Some(state) = self.chrome.settings_action(hit) {
                    self.chrome_actions.settings = Some(state);
                }
            },
            Hit::SettingsClose => {
                // Commit a pending size edit so typed text isn't lost on close.
                if let Some(state) = self.chrome.settings_defocus() {
                    self.chrome_actions.settings = Some(state);
                }
                self.chrome.close_settings();
            },
            // The modal body absorbs clicks so they don't dismiss it; it also defocuses the
            // settings controls (collapsing dropdowns, committing the size input).
            Hit::ModalBackground => {
                if let Some(state) = self.chrome.settings_defocus() {
                    self.chrome_actions.settings = Some(state);
                }
            },
        }
    }

    /// Queue the chrome action for a command-palette command.
    fn queue_palette_cmd(&mut self, cmd: PaletteCmd) {
        match cmd {
            PaletteCmd::Settings => self.chrome_actions.open_settings = true,
            PaletteCmd::NewTab => self.chrome_actions.create = true,
            PaletteCmd::NewProject => self.chrome_actions.create_project = true,
            PaletteCmd::ToggleSidebar => self.chrome_actions.toggle_sidebar = true,
            PaletteCmd::ToggleProjectPane => self.chrome_actions.toggle_pane = true,
            PaletteCmd::SplitRight => {
                self.chrome_actions.split_pane = Some(SplitDirection::Right)
            },
            PaletteCmd::SplitDown => self.chrome_actions.split_pane = Some(SplitDirection::Down),
            PaletteCmd::ClosePane => self.chrome_actions.close_pane = true,
            PaletteCmd::TogglePaneZoom => self.chrome_actions.toggle_pane_zoom = true,
            PaletteCmd::FocusNextPane => self.chrome_actions.focus_next_pane = true,
        }
    }

    /// Drain the chrome actions collected during the last frame.
    pub fn take_chrome_actions(&mut self) -> ChromeActions {
        std::mem::take(&mut self.chrome_actions)
    }

    /// Toggle the project sidebar's visibility and trigger a relayout.
    pub fn toggle_sidebar(&mut self) {
        self.chrome.toggle_sidebar();
        self.pending_update.dirty = true;
    }

    /// Toggle the right-side project pane's visibility and trigger a relayout. Returns whether
    /// the pane is now visible (so the window can kick off a git refresh).
    pub fn toggle_project_pane(&mut self) -> bool {
        self.chrome.toggle_pane();
        self.pending_update.dirty = true;
        self.chrome.pane_visible
    }

    /// Open (or retarget) the shared diff tab to the changed file `path` and focus it.
    pub fn open_diff_viewer(&mut self, path: &str) {
        self.pending_update.dirty = true;
        self.chrome.open_diff_viewer(path);
    }

    /// Open (or retarget) the shared 预览 tab to `path` and focus it.
    pub fn open_file_preview(&mut self, path: &str) {
        self.pending_update.dirty = true;
        self.chrome.open_file_preview(path);
    }

    /// Hand focus from a viewer tab back to the terminal (e.g. on terminal tab switches).
    pub fn unfocus_viewer(&mut self) {
        if self.chrome.unfocus_viewer() {
            self.pending_update.dirty = true;
        }
    }

    /// Switch the viewer's diff layout between unified and side-by-side.
    pub fn set_viewer_layout(&mut self, split: bool) {
        self.pending_update.dirty = true;
        self.chrome.set_viewer_split(split);
    }

    /// Whether the viewer's diff uses the side-by-side layout.
    pub fn viewer_split(&self) -> bool {
        self.chrome.viewer_split()
    }

    /// Toggle difftastic rendering for the viewer's diff, returning the new state.
    pub fn toggle_viewer_difft(&mut self) -> bool {
        self.pending_update.dirty = true;
        self.chrome.toggle_viewer_difft()
    }

    /// Whether the viewer renders diffs with difftastic.
    pub fn viewer_difft(&self) -> bool {
        self.chrome.viewer_difft()
    }

    /// Toggle the markdown preview between rendered output and source, returning whether the
    /// source view is now active.
    pub fn toggle_md_source(&mut self) -> bool {
        self.pending_update.dirty = true;
        self.chrome.toggle_md_source()
    }

    /// Whether the markdown preview shows source text instead of rendered output.
    pub fn md_source(&self) -> bool {
        self.chrome.md_source()
    }

    /// Path of the file shown in the center viewer's preview, if any.
    pub fn previewed_file(&self) -> Option<String> {
        self.chrome.previewed_file().map(str::to_owned)
    }

    /// The selected text in the focused viewer tab (file preview / diff), if any. Preferred over
    /// the terminal selection by the copy paths while a viewer is focused.
    pub fn viewer_selection_text(&self) -> Option<String> {
        self.chrome.viewer_selection_text()
    }

    /// Width of the viewer's text area in chrome cells (terminal columns), for sizing
    /// difftastic's output. Derived from the last frame's reserved chrome sizes.
    pub fn viewer_text_cols(&self) -> usize {
        let w = self.size_info.width() - self.chrome_sidebar_width - self.chrome_pane_width;
        let cols = (w / self.chrome_cell_size.0.max(1.)).floor() as usize;
        // Horizontal padding on both sides plus the scrollbar gutter.
        cols.saturating_sub(3).max(40)
    }

    /// Relative path of the changed file whose diff the center viewer shows, if any.
    pub fn pane_selected(&self) -> Option<String> {
        self.chrome.pane_selected().map(str::to_owned)
    }

    /// Whether the project pane is currently visible.
    pub fn pane_visible(&self) -> bool {
        self.chrome.pane_visible
    }

    /// The project pane's width in chrome cells, for session persistence.
    pub fn pane_cols(&self) -> f32 {
        self.chrome.pane_cols()
    }

    /// Restore the pane's visibility and width from a saved session.
    pub fn set_pane_state(&mut self, visible: bool, cols: f32) {
        self.chrome.set_pane_state(visible, cols);
        self.pending_update.dirty = true;
    }

    /// Reset the pane's view state (scrolls, expanded diff, folds) when the shown project changes.
    pub fn pane_reset_view(&mut self) {
        self.chrome.pane_reset_view();
        self.pending_update.dirty = true;
    }

    /// Toggle the command palette overlay.
    pub fn toggle_command_palette(&mut self) {
        self.chrome.toggle_command_palette();
        self.pending_update.dirty = true;
    }

    /// Open the settings panel seeded with the given state.
    pub fn open_settings_panel(&mut self, state: SettingsState) {
        self.chrome.open_settings(state);
        self.pending_update.dirty = true;
    }

    /// Render the native chrome (tab bar, sidebar, project pane, context menu) over the terminal
    /// grid.
    fn draw_chrome(&mut self, tab_bar: &TabBarInfo, pane_data: Option<&ProjectPaneData>) {
        let (chrome_cw, chrome_ch) = self.chrome_cell_size;
        let win_w = self.size_info.width();
        let win_h = self.size_info.height();

        let draw = self.chrome.layout(tab_bar, pane_data, chrome_cw, chrome_ch, win_w, win_h);

        // Anchor the IME candidate window to the palette's search caret while it's open, so CJK
        // composition appears under the search box rather than at the terminal cursor.
        if let Some((ix, iy, iw, ih)) = self.chrome.palette_ime_rect() {
            self.window.set_ime_cursor_px(ix, iy, iw, ih);
        }

        let metrics = self.glyph_cache.font_metrics();
        // Chrome cells carry absolute pixel positions, so render them with a 1×1-pixel cell
        // grid; glyphs are still rasterized from the larger chrome glyph cache.
        let chrome_size = SizeInfo::new(win_w, win_h, 1., 1., 0., 0., false);

        if !draw.rects.is_empty() {
            self.renderer.draw_chrome_rects(&self.size_info, &metrics, draw.rects);
        }
        if !draw.cells.is_empty() {
            self.renderer.draw_chrome_cells(
                &self.size_info,
                &chrome_size,
                &mut self.chrome_glyph_cache,
                draw.cells.into_iter(),
            );
        }
        // Popups (open dropdown lists) get their own rect+text pass after the base cells, so they
        // paint over text already emitted for the surface beneath them.
        if !draw.overlay_rects.is_empty() {
            self.renderer.draw_chrome_rects(&self.size_info, &metrics, draw.overlay_rects);
        }
        if !draw.overlay_cells.is_empty() {
            self.renderer.draw_chrome_cells(
                &self.size_info,
                &chrome_size,
                &mut self.chrome_glyph_cache,
                draw.overlay_cells.into_iter(),
            );
        }

        // Feed the reserved tab-bar height back into the layout so the terminal sits below the bar.
        if (draw.bar_height - self.chrome_bar_height).abs() > f32::EPSILON {
            self.chrome_bar_height = draw.bar_height;
            self.chrome_actions.layout_changed = true;
            self.pending_update.dirty = true;
        }

        // Feed the reserved sidebar width back into the layout so the terminal sits to its right.
        if (draw.sidebar_width - self.chrome_sidebar_width).abs() > f32::EPSILON {
            self.chrome_sidebar_width = draw.sidebar_width;
            self.chrome_actions.layout_changed = true;
            self.pending_update.dirty = true;
        }

        // Feed the reserved status-bar height back into the layout so the terminal sits above it.
        if (draw.status_height - self.chrome_status_height).abs() > f32::EPSILON {
            self.chrome_status_height = draw.status_height;
            self.chrome_actions.layout_changed = true;
            self.pending_update.dirty = true;
        }

        // Feed the reserved pane width back into the layout so the terminal sits to its left.
        if (draw.pane_width - self.chrome_pane_width).abs() > f32::EPSILON {
            self.chrome_pane_width = draw.pane_width;
            self.chrome_actions.layout_changed = true;
            self.pending_update.dirty = true;
        }
    }

    /// Draw render timer.
    #[inline(never)]
    fn draw_render_timer(&mut self, config: &UiConfig) {
        if !config.debug.render_timer {
            return;
        }

        let timing = format!("{:.3} usec", self.meter.average());
        let point = Point::new(self.size_info.screen_lines().saturating_sub(2), Column(0));
        let fg = config.colors.primary.background;
        let bg = config.colors.normal.red;

        // Damage render timer for current and next frame.
        let damage = LineDamageBounds::new(point.line, point.column.0, timing.len());
        self.damage_tracker.frame().damage_line(damage);
        self.damage_tracker.next_frame().damage_line(damage);

        let glyph_cache = &mut self.glyph_cache;
        self.renderer.draw_string(point, fg, bg, timing.chars(), &self.size_info, glyph_cache);
    }

    /// Draw an indicator for the position of a line in history.
    #[inline(never)]
    fn draw_line_indicator(
        &mut self,
        config: &UiConfig,
        size_info: &SizeInfo,
        total_lines: usize,
        obstructed_column: Option<Column>,
        line: usize,
    ) {
        let columns = size_info.columns();
        let text = format!("[{}/{}]", line, total_lines - 1);
        let column = Column(size_info.columns().saturating_sub(text.len()));
        let point = Point::new(0, column);

        // Damage the line indicator for current and next frame.
        let damage = LineDamageBounds::new(point.line, point.column.0, columns - 1);
        self.damage_tracker.frame().damage_line(damage);
        self.damage_tracker.next_frame().damage_line(damage);

        let colors = &config.colors;
        let fg = colors.line_indicator.foreground.unwrap_or(colors.primary.background);
        let bg = colors.line_indicator.background.unwrap_or(colors.primary.foreground);

        // Do not render anything if it would obscure the vi mode cursor.
        if obstructed_column.is_none_or(|obstructed_column| obstructed_column < column) {
            let glyph_cache = &mut self.glyph_cache;
            self.renderer.draw_string(point, fg, bg, text.chars(), size_info, glyph_cache);
        }
    }

    /// Highlight damaged rects.
    ///
    /// This function is for debug purposes only.
    fn highlight_damage(&self, render_rects: &mut Vec<RenderRect>) {
        for damage_rect in &self.damage_tracker.shape_frame_damage(self.size_info.into()) {
            let x = damage_rect.x as f32;
            let height = damage_rect.height as f32;
            let width = damage_rect.width as f32;
            let y = damage_y_to_viewport_y(&self.size_info, damage_rect) as f32;
            let render_rect = RenderRect::new(x, y, width, height, DAMAGE_RECT_COLOR, 0.5);

            render_rects.push(render_rect);
        }
    }

    /// Check whether a hint highlight needs to be cleared.
    fn validate_hint_highlights(&mut self, display_offset: usize) {
        let frame = self.damage_tracker.frame();
        let hints = [
            (&mut self.highlighted_hint, &mut self.highlighted_hint_age, true),
            (&mut self.vi_highlighted_hint, &mut self.vi_highlighted_hint_age, false),
        ];

        let num_lines = self.size_info.screen_lines();
        for (hint, hint_age, reset_mouse) in hints {
            let (start, end) = match hint {
                Some(hint) => (*hint.bounds().start(), *hint.bounds().end()),
                None => continue,
            };

            // Ignore hints that were created this frame.
            *hint_age += 1;
            if *hint_age == 1 {
                continue;
            }

            // Convert hint bounds to viewport coordinates.
            let start = term::point_to_viewport(display_offset, start)
                .filter(|point| point.line < num_lines)
                .unwrap_or_default();
            let end = term::point_to_viewport(display_offset, end)
                .filter(|point| point.line < num_lines)
                .unwrap_or_else(|| Point::new(num_lines - 1, self.size_info.last_column()));

            // Clear invalidated hints.
            if frame.intersects(start, end) {
                if reset_mouse {
                    self.window.set_mouse_cursor(CursorIcon::Default);
                }
                frame.mark_fully_damaged();
                *hint = None;
            }
        }
    }

    /// Request a new frame for a window on Wayland.
    fn request_frame(&mut self, scheduler: &mut Scheduler) {
        // Mark that we've used a frame.
        self.window.has_frame = false;

        // Get the display vblank interval.
        let monitor_vblank_interval = 1_000_000.
            / self
                .window
                .current_monitor()
                .and_then(|monitor| monitor.refresh_rate_millihertz())
                .unwrap_or(60_000) as f64;

        // Now convert it to micro seconds.
        let monitor_vblank_interval =
            Duration::from_micros((1000. * monitor_vblank_interval) as u64);

        let swap_timeout = self.frame_timer.compute_timeout(monitor_vblank_interval);

        let window_id = self.window.id();
        let timer_id = TimerId::new(Topic::Frame, window_id);
        let event = Event::new(EventType::Frame, window_id);

        scheduler.schedule(event, swap_timeout, false, timer_id);
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        // Switch OpenGL context before dropping, otherwise objects (like programs) from other
        // contexts might be deleted when dropping renderer.
        self.make_current();
        unsafe {
            ManuallyDrop::drop(&mut self.renderer);
            ManuallyDrop::drop(&mut self.context);
            ManuallyDrop::drop(&mut self.surface);
        }
    }
}

/// Input method state.
#[derive(Debug, Default)]
pub struct Ime {
    /// Whether the IME is enabled.
    enabled: bool,

    /// Current IME preedit.
    preedit: Option<Preedit>,
}

impl Ime {
    #[inline]
    pub fn set_enabled(&mut self, is_enabled: bool) {
        if is_enabled {
            self.enabled = is_enabled
        } else {
            // Clear state when disabling IME.
            *self = Default::default();
        }
    }

    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    #[inline]
    pub fn set_preedit(&mut self, preedit: Option<Preedit>) {
        self.preedit = preedit;
    }

    #[inline]
    pub fn preedit(&self) -> Option<&Preedit> {
        self.preedit.as_ref()
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Preedit {
    /// The preedit text.
    text: String,

    /// Byte offset for cursor start into the preedit text.
    ///
    /// `None` means that the cursor is invisible.
    cursor_byte_offset: Option<(usize, usize)>,

    /// The cursor offset from the end of the start of the preedit in char width.
    cursor_end_offset: Option<(usize, usize)>,
}

impl Preedit {
    pub fn new(text: String, cursor_byte_offset: Option<(usize, usize)>) -> Self {
        let cursor_end_offset = if let Some(byte_offset) = cursor_byte_offset {
            // Convert byte offset into char offset.
            let start_to_end_offset =
                text[byte_offset.0..].chars().fold(0, |acc, ch| acc + ch.width().unwrap_or(1));
            let end_to_end_offset =
                text[byte_offset.1..].chars().fold(0, |acc, ch| acc + ch.width().unwrap_or(1));

            Some((start_to_end_offset, end_to_end_offset))
        } else {
            None
        };

        Self { text, cursor_byte_offset, cursor_end_offset }
    }
}

/// Pending renderer updates.
///
/// All renderer updates are cached to be applied just before rendering, to avoid platform-specific
/// rendering issues.
#[derive(Debug, Default, Copy, Clone)]
pub struct RendererUpdate {
    /// Should resize the window.
    resize: bool,

    /// Clear font caches.
    clear_font_cache: bool,
}

/// The frame timer state.
pub struct FrameTimer {
    /// Base timestamp used to compute sync points.
    base: Instant,

    /// The last timestamp we synced to.
    last_synced_timestamp: Instant,

    /// The refresh rate we've used to compute sync timestamps.
    refresh_interval: Duration,
}

impl FrameTimer {
    pub fn new() -> Self {
        let now = Instant::now();
        Self { base: now, last_synced_timestamp: now, refresh_interval: Duration::ZERO }
    }

    /// Compute the delay that we should use to achieve the target frame
    /// rate.
    pub fn compute_timeout(&mut self, refresh_interval: Duration) -> Duration {
        let now = Instant::now();

        // Handle refresh rate change.
        if self.refresh_interval != refresh_interval {
            self.base = now;
            self.last_synced_timestamp = now;
            self.refresh_interval = refresh_interval;
            return refresh_interval;
        }

        let next_frame = self.last_synced_timestamp + self.refresh_interval;

        if next_frame < now {
            // Redraw immediately if we haven't drawn in over `refresh_interval` microseconds.
            let elapsed_micros = (now - self.base).as_micros() as u64;
            let refresh_micros = self.refresh_interval.as_micros() as u64;
            self.last_synced_timestamp =
                now - Duration::from_micros(elapsed_micros % refresh_micros);
            Duration::ZERO
        } else {
            // Redraw on the next `refresh_interval` clock tick.
            self.last_synced_timestamp = next_frame;
            next_frame - now
        }
    }
}

/// Cell `(width, height)` in pixels for the chrome font, derived directly from its metrics (the
/// terminal's per-cell offsets don't apply to the chrome).
fn chrome_cell_size(metrics: &crossfont::Metrics) -> (f32, f32) {
    (metrics.average_advance.floor().max(1.) as f32, metrics.line_height.floor().max(1.) as f32)
}

fn compute_cell_size(config: &UiConfig, metrics: &crossfont::Metrics) -> (f32, f32) {
    let offset_x = f64::from(config.font.offset.x);
    let offset_y = f64::from(config.font.offset.y);
    (
        (metrics.average_advance + offset_x).floor().max(1.) as f32,
        (metrics.line_height + offset_y).floor().max(1.) as f32,
    )
}

/// Calculate the size of the window given padding, terminal dimensions and cell size.
fn window_size(
    config: &UiConfig,
    dimensions: Dimensions,
    cell_width: f32,
    cell_height: f32,
    scale_factor: f32,
) -> PhysicalSize<u32> {
    let padding = config.window.padding(scale_factor);

    let grid_width = cell_width * dimensions.columns.max(MIN_COLUMNS) as f32;
    let grid_height = cell_height * dimensions.lines.max(MIN_SCREEN_LINES) as f32;

    let width = (padding.0).mul_add(2., grid_width).floor();
    let height = (padding.1).mul_add(2., grid_height).floor();

    PhysicalSize::new(width as u32, height as u32)
}
