//! Terminal window context.

use std::error::Error;
use std::fs::{self, File};
use std::io::Write;
use std::mem;
#[cfg(not(windows))]
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossfont::Size as FontSize;
use glutin::config::Config as GlutinConfig;
use glutin::display::GetGlDisplay;
#[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
use glutin::platform::x11::X11GlConfigExt;
use log::{debug, error, info};
use serde_json as json;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, Event as WinitEvent, Modifiers, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::raw_window_handle::HasDisplayHandle;
use winit::window::{CursorIcon, WindowId};

use kairos_terminal::event::{Event as TerminalEvent, Notify, OnResize};
use kairos_terminal::event_loop::{EventLoop as PtyEventLoop, Msg, Notifier};
use kairos_terminal::grid::{Dimensions, Scroll};
use kairos_terminal::index::Direction;
use kairos_terminal::sync::FairMutex;
use kairos_terminal::term::test::TermSize;
use kairos_terminal::term::{ClipboardType, Term, TermMode};
use kairos_terminal::tty;
use kairos_terminal::vte::ansi::NamedColor;

use crate::cli::{ParsedOptions, WindowOptions};
use crate::clipboard::Clipboard;
use crate::config::UiConfig;
use crate::config::persist::{self, ShellChoice};
use crate::config::ui_config::Program;
use crate::claude_sessions::{self, ClaudeSession};
use crate::display::chrome::{PixelRect, SettingsState, ShellPreset};
use crate::display::color::Rgb;
use crate::display::project_pane::ProjectPaneData;
use crate::display::window::Window;
use crate::display::{ClaudeSessionRow, Display, SizeInfo, TabBarInfo};
use crate::event::{
    ActionContext, Event, EventProxy, EventType, InlineSearchState, Mouse, PaneDirection,
    SearchState, SplitDirection, TabSelection, TouchPurpose,
};
use crate::git_worker::{GitData, GitWorker};
#[cfg(unix)]
use crate::logging::LOG_TARGET_IPC_CONFIG;
use crate::message_bar::MessageBuffer;
use crate::scheduler::{Scheduler, TimerId, Topic};
use crate::session::{SavedPaneLayout, SavedProject, SavedSplitDirection, SavedTab, SavedWindow};
use crate::{input, renderer};

/// Monotonic counter used to assign every terminal (tab) a process-wide unique id.
static NEXT_TERMINAL_ID: AtomicU64 = AtomicU64::new(0);

/// Convert a settings-panel shell choice into a config [`Program`].
fn program_from_preset(shell: &ShellPreset) -> Program {
    if shell.args.is_empty() {
        Program::Just(shell.program.clone())
    } else {
        Program::WithArgs { program: shell.program.clone(), args: shell.args.clone() }
    }
}

/// One-line git summary for `dir` (current branch, or the detached commit), or `None` when `dir`
/// isn't inside a git repository. Resolved by walking up to the containing repo and reading
/// `.git/HEAD` directly — no subprocess, cheap enough to call per frame.
fn git_branch_line(dir: &std::path::Path) -> Option<String> {
    let mut current = Some(dir);
    let git = loop {
        let dir = current?;
        let git = dir.join(".git");
        if git.exists() {
            break git;
        }
        current = dir.parent();
    };

    // `.git` may be a worktree/submodule pointer file containing "gitdir: <path>".
    let git_dir = if git.is_file() {
        let pointer = fs::read_to_string(&git).ok()?;
        let target = std::path::Path::new(pointer.strip_prefix("gitdir:")?.trim());
        if target.is_absolute() { target.to_owned() } else { git.parent()?.join(target) }
    } else {
        git
    };

    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    Some(match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => format!("git: {branch}"),
        // Detached HEAD: show the abbreviated commit (hex, so byte slicing is safe).
        None => format!("git: {} (detached)", &head[..head.len().min(8)]),
    })
}

/// State of a single terminal pane: a leaf of a tab's split tree.
///
/// Everything that belongs to one running shell — the terminal grid, the PTY notifier and the
/// per-terminal UI state (search, title) — lives here. The window-level rendering resources are
/// shared across all tabs and stay in [`WindowContext`].
pub struct Pane {
    /// Process-wide unique id, used to route PTY events to this pane.
    terminal_id: u64,
    terminal: Arc<FairMutex<Term<EventProxy>>>,
    notifier: Notifier,
    inline_search_state: InlineSearchState,
    search_state: SearchState,
    /// Latest title reported by this terminal, shown on the tab bar while this pane is focused.
    title: String,
    preserve_title: bool,
    /// Claude Code session this pane was opened to resume, if any. Used to focus an existing tab
    /// instead of opening a duplicate when its sidebar entry is clicked again.
    claude_session: Option<String>,
    /// Authoritative geometry of this pane's grid, refreshed by the window's relayout pass.
    size_info: SizeInfo,
    #[cfg(not(windows))]
    master_fd: RawFd,
    #[cfg(not(windows))]
    shell_pid: u32,
}

impl Pane {
    /// Spawn a new terminal with its own PTY and I/O thread.
    fn new(
        config: &UiConfig,
        options: &WindowOptions,
        size_info: &SizeInfo,
        window_id: WindowId,
        proxy: EventLoopProxy<Event>,
    ) -> Result<Self, Box<dyn Error>> {
        let mut pty_config = config.pty_config();
        options.terminal_options.override_pty_config(&mut pty_config);

        let preserve_title = options.window_identity.title.is_some();

        let terminal_id = NEXT_TERMINAL_ID.fetch_add(1, Ordering::Relaxed);
        let event_proxy = EventProxy::new(proxy, window_id, terminal_id);

        // Create the terminal.
        //
        // This object contains all of the state about what's being displayed. It's
        // wrapped in a clonable mutex since both the I/O loop and display need to
        // access it.
        let terminal = Term::new(config.term_options(), size_info, event_proxy.clone());
        let terminal = Arc::new(FairMutex::new(terminal));

        // Create the PTY.
        //
        // The PTY forks a process to run the shell on the slave side of the
        // pseudoterminal. A file descriptor for the master side is retained for
        // reading/writing to the shell.
        let pty = tty::new(&pty_config, (*size_info).into(), window_id.into())?;

        #[cfg(not(windows))]
        let master_fd = pty.file().as_raw_fd();
        #[cfg(not(windows))]
        let shell_pid = pty.child().id();

        // Create the pseudoterminal I/O loop.
        //
        // PTY I/O is ran on another thread as to not occupy cycles used by the
        // renderer and input processing. Note that access to the terminal state is
        // synchronized since the I/O loop updates the state, and the display
        // consumes it periodically.
        let event_loop = PtyEventLoop::new(
            Arc::clone(&terminal),
            event_proxy.clone(),
            pty,
            pty_config.drain_on_exit,
            config.debug.ref_test,
        )?;

        // The event loop channel allows write requests from the event processor
        // to be sent to the pty loop and ultimately written to the pty.
        let loop_tx = event_loop.channel();

        // Kick off the I/O thread.
        let _io_thread = event_loop.spawn();

        // Start cursor blinking, in case `Focused` isn't sent on startup.
        if config.cursor.style().blinking {
            event_proxy.send_event(TerminalEvent::CursorBlinkingChange.into());
        }

        Ok(Pane {
            terminal_id,
            terminal,
            notifier: Notifier(loop_tx),
            inline_search_state: Default::default(),
            search_state: Default::default(),
            title: String::new(),
            preserve_title,
            claude_session: None,
            size_info: *size_info,
            #[cfg(not(windows))]
            master_fd,
            #[cfg(not(windows))]
            shell_pid,
        })
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        // Shutdown the terminal's PTY.
        let _ = self.notifier.0.send(Msg::Shutdown);
    }
}

/// Pixel thickness of the divider drawn between split panes.
pub const DIVIDER_PX: f32 = 3.;

/// Extra pixels on each side of a divider that still grab it for dragging.
pub const DIVIDER_GRAB_PX: f32 = 4.;

/// Minimum number of columns a pane may be reduced to (by splitting or divider resizing).
pub const MIN_PANE_COLUMNS: usize = 20;

/// Minimum number of grid lines a pane may be reduced to (by splitting or divider resizing).
pub const MIN_PANE_LINES: usize = 4;

/// Clamp a split ratio so both children keep at least `min_px` of `extent` (the split's pixel
/// extent along its axis, divider included).
fn clamp_ratio(ratio: f32, extent: f32, min_px: f32) -> f32 {
    let usable = (extent - DIVIDER_PX).max(1.);
    let min = (min_px / usable).min(0.5);
    ratio.clamp(min, 1. - min)
}

/// Which child of a [`Split`] a path step descends into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildSlot {
    First,
    Second,
}

/// An in-progress divider drag: which split is being resized and its pixel span.
struct PaneDrag {
    /// Path from the tree root to the dragged [`Split`].
    path: Vec<ChildSlot>,
    /// Axis of the dragged split.
    direction: SplitDirection,
    /// `(start, extent)` of the split's rect along its axis.
    span: (f32, f32),
}

/// A divider between two pane subtrees, produced by the layout pass.
pub struct PaneDivider {
    /// Path from the tree root to the [`Split`] that owns this divider. Stable until the next
    /// structural change; recomputed on every relayout.
    pub path: Vec<ChildSlot>,
    /// Axis of the owning split.
    pub direction: SplitDirection,
    /// The divider's window-pixel rectangle (drawing and hit-testing).
    pub rect: PixelRect,
    /// `(start, extent)` of the owning split's rect along the split axis, for ratio math.
    pub span: (f32, f32),
}

/// Divide `rect` along `direction` into `(first, divider, second)`, snapped to whole pixels.
fn split_rect(
    rect: PixelRect,
    direction: SplitDirection,
    ratio: f32,
) -> (PixelRect, PixelRect, PixelRect) {
    match direction {
        SplitDirection::Right => {
            let first_w = (ratio * (rect.w - DIVIDER_PX)).round().clamp(0., rect.w - DIVIDER_PX);
            (
                PixelRect { x: rect.x, y: rect.y, w: first_w, h: rect.h },
                PixelRect { x: rect.x + first_w, y: rect.y, w: DIVIDER_PX, h: rect.h },
                PixelRect {
                    x: rect.x + first_w + DIVIDER_PX,
                    y: rect.y,
                    w: rect.w - first_w - DIVIDER_PX,
                    h: rect.h,
                },
            )
        },
        SplitDirection::Down => {
            let first_h = (ratio * (rect.h - DIVIDER_PX)).round().clamp(0., rect.h - DIVIDER_PX);
            (
                PixelRect { x: rect.x, y: rect.y, w: rect.w, h: first_h },
                PixelRect { x: rect.x, y: rect.y + first_h, w: rect.w, h: DIVIDER_PX },
                PixelRect {
                    x: rect.x,
                    y: rect.y + first_h + DIVIDER_PX,
                    w: rect.w,
                    h: rect.h - first_h - DIVIDER_PX,
                },
            )
        },
    }
}

/// An inner node of a tab's split tree: two children sharing the parent's area.
pub struct Split {
    /// Axis the parent area is divided along.
    pub direction: SplitDirection,
    /// `first`'s share of the parent area, in `0.0..1.0`.
    pub ratio: f32,
    /// Left (`Right` split) or top (`Down` split) child.
    pub first: Box<PaneNode>,
    /// Right or bottom child.
    pub second: Box<PaneNode>,
}

/// A tab's split tree: terminals at the leaves, splits at the inner nodes.
// Leaves dwarf the split nodes, but the tree's nodes already live behind `Box`es (the root and
// every `Split` child), so boxing `Pane` again would only add a pointer hop.
#[allow(clippy::large_enum_variant)]
pub enum PaneNode {
    Leaf(Pane),
    Split(Split),
}

impl PaneNode {
    /// The pane with the given terminal id, if it lives in this subtree.
    fn pane(&self, id: u64) -> Option<&Pane> {
        match self {
            PaneNode::Leaf(pane) => (pane.terminal_id == id).then_some(pane),
            PaneNode::Split(split) => split.first.pane(id).or_else(|| split.second.pane(id)),
        }
    }

    /// Mutable access to the pane with the given terminal id.
    fn pane_mut(&mut self, id: u64) -> Option<&mut Pane> {
        match self {
            PaneNode::Leaf(pane) => (pane.terminal_id == id).then_some(pane),
            PaneNode::Split(split) => {
                split.first.pane_mut(id).or_else(|| split.second.pane_mut(id))
            },
        }
    }

    /// The first (top-left-most) pane of this subtree.
    fn first_pane(&self) -> &Pane {
        match self {
            PaneNode::Leaf(pane) => pane,
            PaneNode::Split(split) => split.first.first_pane(),
        }
    }

    /// Number of panes in this subtree.
    fn pane_count(&self) -> usize {
        match self {
            PaneNode::Leaf(_) => 1,
            PaneNode::Split(split) => split.first.pane_count() + split.second.pane_count(),
        }
    }

    /// Visit every pane of this subtree, in-order.
    fn for_each_pane<'a>(&'a self, f: &mut impl FnMut(&'a Pane)) {
        match self {
            PaneNode::Leaf(pane) => f(pane),
            PaneNode::Split(split) => {
                split.first.for_each_pane(f);
                split.second.for_each_pane(f);
            },
        }
    }

    /// Terminal ids of every pane in this subtree, in-order.
    fn pane_ids(&self) -> Vec<u64> {
        let mut ids = Vec::with_capacity(self.pane_count());
        self.for_each_pane(&mut |pane| ids.push(pane.terminal_id));
        ids
    }

    /// Replace leaf `target` with a split of `{ old leaf, new pane }` along `direction`.
    ///
    /// Structural edits are by-value (consume and rebuild the spine) to stay clear of
    /// borrow-checker contortions; `new_pane` is threaded as an `Option` so its ownership
    /// survives the no-match branches and is consumed exactly once at the matching leaf.
    fn split_at(self, target: u64, direction: SplitDirection, new_pane: &mut Option<Pane>) -> Self {
        match self {
            PaneNode::Leaf(pane) if pane.terminal_id == target => {
                let new = new_pane.take().expect("split pane consumed once");
                PaneNode::Split(Split {
                    direction,
                    ratio: 0.5,
                    first: Box::new(PaneNode::Leaf(pane)),
                    second: Box::new(PaneNode::Leaf(new)),
                })
            },
            leaf @ PaneNode::Leaf(_) => leaf,
            PaneNode::Split(split) => {
                let Split { direction: dir, ratio, first, second } = split;
                let first = Box::new(first.split_at(target, direction, new_pane));
                let second = Box::new(second.split_at(target, direction, new_pane));
                PaneNode::Split(Split { direction: dir, ratio, first, second })
            },
        }
    }

    /// Remove leaf `target` from this subtree; the sibling subtree replaces the parent split.
    ///
    /// Returns what replaces this subtree (`None` when the subtree *was* the target) plus the
    /// removed pane, if it was found here.
    fn remove(self, target: u64) -> (Option<PaneNode>, Option<Pane>) {
        match self {
            PaneNode::Leaf(pane) if pane.terminal_id == target => (None, Some(pane)),
            leaf @ PaneNode::Leaf(_) => (Some(leaf), None),
            PaneNode::Split(split) => {
                let Split { direction, ratio, first, second } = split;
                let (first, removed) = (*first).remove(target);
                if removed.is_some() {
                    return match first {
                        Some(first) => (
                            Some(PaneNode::Split(Split {
                                direction,
                                ratio,
                                first: Box::new(first),
                                second,
                            })),
                            removed,
                        ),
                        // Collapse: the sibling subtree absorbs the parent's area.
                        None => (Some(*second), removed),
                    };
                }
                let first = first.expect("non-target subtree intact");
                let (second, removed) = (*second).remove(target);
                match second {
                    Some(second) => (
                        Some(PaneNode::Split(Split {
                            direction,
                            ratio,
                            first: Box::new(first),
                            second: Box::new(second),
                        })),
                        removed,
                    ),
                    // Collapse: the sibling subtree absorbs the parent's area.
                    None => (Some(first), removed),
                }
            },
        }
    }

    /// The first leaf of the subtree that would absorb `target`'s area if it were removed.
    fn sibling_first_pane_id(&self, target: u64) -> Option<u64> {
        match self {
            PaneNode::Leaf(_) => None,
            PaneNode::Split(split) => {
                if matches!(&*split.first, PaneNode::Leaf(pane) if pane.terminal_id == target) {
                    return Some(split.second.first_pane().terminal_id);
                }
                if matches!(&*split.second, PaneNode::Leaf(pane) if pane.terminal_id == target) {
                    return Some(split.first.first_pane().terminal_id);
                }
                split
                    .first
                    .sibling_first_pane_id(target)
                    .or_else(|| split.second.sibling_first_pane_id(target))
            },
        }
    }

    /// Path of child slots from this node down to the leaf `id`, or `None` if it isn't here.
    fn path_to(&self, id: u64, path: &mut Vec<ChildSlot>) -> bool {
        match self {
            PaneNode::Leaf(pane) => pane.terminal_id == id,
            PaneNode::Split(split) => {
                path.push(ChildSlot::First);
                if split.first.path_to(id, path) {
                    return true;
                }
                path.pop();
                path.push(ChildSlot::Second);
                if split.second.path_to(id, path) {
                    return true;
                }
                path.pop();
                false
            },
        }
    }

    /// The split reached by descending `path` from this node.
    fn split_at_path_mut(&mut self, path: &[ChildSlot]) -> Option<&mut Split> {
        match self {
            PaneNode::Leaf(_) => None,
            PaneNode::Split(split) => match path.split_first() {
                None => Some(split),
                Some((ChildSlot::First, rest)) => split.first.split_at_path_mut(rest),
                Some((ChildSlot::Second, rest)) => split.second.split_at_path_mut(rest),
            },
        }
    }

    /// Assign window-pixel rects: leaves get their share of `rect`, splits divide it by ratio
    /// along their axis and record the divider between the halves.
    fn layout(
        &self,
        rect: PixelRect,
        path: &mut Vec<ChildSlot>,
        panes: &mut Vec<(u64, PixelRect)>,
        dividers: &mut Vec<PaneDivider>,
    ) {
        match self {
            PaneNode::Leaf(pane) => panes.push((pane.terminal_id, rect)),
            PaneNode::Split(split) => {
                let (first, divider, second) = split_rect(rect, split.direction, split.ratio);
                let span = match split.direction {
                    SplitDirection::Right => (rect.x, rect.w),
                    SplitDirection::Down => (rect.y, rect.h),
                };
                dividers.push(PaneDivider {
                    path: path.clone(),
                    direction: split.direction,
                    rect: divider,
                    span,
                });
                path.push(ChildSlot::First);
                split.first.layout(first, path, panes, dividers);
                path.pop();
                path.push(ChildSlot::Second);
                split.second.layout(second, path, panes, dividers);
                path.pop();
            },
        }
    }
}

/// Monotonic counter assigning every tab a process-wide unique id, used by the chrome's tab bar.
/// Distinct from terminal (pane) ids: a tab keeps its id while panes come and go.
static NEXT_TAB_ID: AtomicU64 = AtomicU64::new(0);

/// One tab of a window: a tree of terminal panes sharing the tab's content area.
pub struct Tab {
    /// Stable tab-bar identity (select/close from the chrome). Not a terminal id.
    tab_id: u64,
    /// Split tree; `Some` outside the few by-value structural edits; always holds >= 1 pane.
    root: Option<PaneNode>,
    /// Terminal id of the focused leaf.
    focused_pane: u64,
    /// Focused pane temporarily maximized over the whole tab content area.
    zoomed: bool,
    /// Dividers between panes, cached from the last relayout (drawing and hit-testing).
    dividers: Vec<PaneDivider>,
}

impl Tab {
    /// Spawn a tab holding a single fresh terminal pane.
    fn new_single(
        config: &UiConfig,
        options: &WindowOptions,
        size_info: &SizeInfo,
        window_id: WindowId,
        proxy: EventLoopProxy<Event>,
    ) -> Result<Self, Box<dyn Error>> {
        Ok(Self::from_pane(Pane::new(config, options, size_info, window_id, proxy)?))
    }

    /// Wrap an existing pane into a single-pane tab.
    fn from_pane(pane: Pane) -> Self {
        Tab {
            tab_id: NEXT_TAB_ID.fetch_add(1, Ordering::Relaxed),
            focused_pane: pane.terminal_id,
            root: Some(PaneNode::Leaf(pane)),
            zoomed: false,
            dividers: Vec::new(),
        }
    }

    fn root(&self) -> &PaneNode {
        self.root.as_ref().expect("tab pane tree")
    }

    fn root_mut(&mut self) -> &mut PaneNode {
        self.root.as_mut().expect("tab pane tree")
    }

    /// The pane with the given terminal id, if it lives in this tab.
    fn pane(&self, id: u64) -> Option<&Pane> {
        self.root().pane(id)
    }

    /// Mutable access to the pane with the given terminal id.
    fn pane_mut(&mut self, id: u64) -> Option<&mut Pane> {
        self.root_mut().pane_mut(id)
    }

    /// Whether the pane with the given terminal id lives in this tab.
    fn contains(&self, id: u64) -> bool {
        self.pane(id).is_some()
    }

    /// The focused pane (falling back to the first pane if the focus id went stale).
    fn focused(&self) -> &Pane {
        self.pane(self.focused_pane).unwrap_or_else(|| self.root().first_pane())
    }

    /// Mutable access to the focused pane.
    fn focused_mut(&mut self) -> &mut Pane {
        let id =
            if self.contains(self.focused_pane) { self.focused_pane } else { self.first_pane_id() };
        self.pane_mut(id).expect("tab focused pane")
    }

    /// Terminal id of the first (top-left-most) pane.
    fn first_pane_id(&self) -> u64 {
        self.root().first_pane().terminal_id
    }

    /// Number of panes in this tab.
    fn pane_count(&self) -> usize {
        self.root().pane_count()
    }

    /// Tab-bar label: the focused pane's latest title.
    fn title(&self) -> &str {
        &self.focused().title
    }

    /// Visit every pane of this tab, in-order.
    fn for_each_pane<'a>(&'a self, f: &mut impl FnMut(&'a Pane)) {
        self.root().for_each_pane(f);
    }

    /// Terminal ids of this tab's panes, in-order.
    fn pane_ids(&self) -> Vec<u64> {
        self.root().pane_ids()
    }

    /// Terminal ids of the panes visible on screen, ordered for drawing (focused pane last).
    /// While zoomed only the focused pane is visible.
    fn visible_pane_ids(&self) -> Vec<u64> {
        let focused = self.focused().terminal_id;
        if self.zoomed {
            return vec![focused];
        }
        let mut ids = self.pane_ids();
        ids.retain(|id| *id != focused);
        ids.push(focused);
        ids
    }

    /// Window-pixel rects for all visible panes within `content`, plus the dividers between
    /// them. While zoomed the focused pane covers the whole content area with no dividers.
    fn compute_layout(&self, content: PixelRect) -> (Vec<(u64, PixelRect)>, Vec<PaneDivider>) {
        let mut panes = Vec::with_capacity(self.pane_count());
        let mut dividers = Vec::new();
        if self.zoomed {
            panes.push((self.focused().terminal_id, content));
        } else {
            self.root().layout(content, &mut Vec::new(), &mut panes, &mut dividers);
        }
        (panes, dividers)
    }

    /// Re-establish the `Term::is_focused` invariant for this tab: clear the flag on every pane,
    /// then set it on the focused pane when the window itself is focused.
    fn apply_focus_flag(&self, window_focused: bool) {
        let focused_pane = self.focused().terminal_id;
        self.for_each_pane(&mut |pane| {
            pane.terminal.lock().is_focused = window_focused && pane.terminal_id == focused_pane;
        });
    }

    /// Split the focused leaf along `direction`; the new pane becomes focused. Clears zoom.
    fn split(&mut self, direction: SplitDirection, new_pane: Pane) {
        self.zoomed = false;
        let new_id = new_pane.terminal_id;
        let target = self.focused().terminal_id;
        let mut new_pane = Some(new_pane);
        let root = self.root.take().expect("tab pane tree");
        self.root = Some(root.split_at(target, direction, &mut new_pane));
        debug_assert!(new_pane.is_none(), "split target leaf not found");
        self.focused_pane = new_id;
    }

    /// Rebuild this single-pane tab's tree to mirror a saved `layout`.
    ///
    /// The existing pane stands in for the layout's focused leaf — it was already spawned in
    /// that leaf's directory by the session bootstrap. The remaining leaves spawn via `spawn`;
    /// a leaf that fails to spawn collapses into its sibling (the close-pane transformation).
    fn apply_saved_layout(
        &mut self,
        layout: &SavedPaneLayout,
        spawn: &mut dyn FnMut(Option<&PathBuf>) -> Option<Pane>,
    ) {
        fn build(
            layout: &SavedPaneLayout,
            target: usize,
            next_leaf: &mut usize,
            reuse: &mut Option<Pane>,
            spawn: &mut dyn FnMut(Option<&PathBuf>) -> Option<Pane>,
        ) -> Option<PaneNode> {
            match layout {
                SavedPaneLayout::Leaf { cwd, .. } => {
                    let index = *next_leaf;
                    *next_leaf += 1;
                    let pane =
                        if index == target { reuse.take() } else { None }
                            .or_else(|| spawn(cwd.as_ref()))?;
                    Some(PaneNode::Leaf(pane))
                },
                SavedPaneLayout::Split { direction, ratio, first, second } => {
                    let first = build(first, target, next_leaf, reuse, spawn);
                    let second = build(second, target, next_leaf, reuse, spawn);
                    match (first, second) {
                        (Some(first), Some(second)) => Some(PaneNode::Split(Split {
                            direction: match direction {
                                SavedSplitDirection::Right => SplitDirection::Right,
                                SavedSplitDirection::Down => SplitDirection::Down,
                            },
                            ratio: *ratio,
                            first: Box::new(first),
                            second: Box::new(second),
                        })),
                        (Some(node), None) | (None, Some(node)) => Some(node),
                        (None, None) => None,
                    }
                },
            }
        }

        if self.pane_count() > 1 {
            return;
        }

        let existing = match self.root.take().expect("tab pane tree") {
            PaneNode::Leaf(pane) => pane,
            node => {
                self.root = Some(node);
                return;
            },
        };
        let focused_id = existing.terminal_id;
        let mut reuse = Some(existing);

        let target = layout.focused_leaf_index().unwrap_or(0);
        let mut next_leaf = 0;
        let root = build(layout, target, &mut next_leaf, &mut reuse, spawn);
        self.root = Some(match root {
            Some(node) => node,
            // Every other leaf failed to spawn; keep the bootstrap pane as a single-pane tab.
            None => PaneNode::Leaf(reuse.take().expect("bootstrap pane survives failed restore")),
        });
        self.focused_pane = focused_id;
        self.zoomed = false;
    }

    /// Remove pane `id`; its sibling subtree absorbs the space. Returns the removed pane, or
    /// `None` when it is the tab's last pane (the caller closes the whole tab instead) or the
    /// id is unknown. Refocuses onto the absorbing subtree and clears zoom.
    fn close_pane(&mut self, id: u64) -> Option<Pane> {
        if self.pane_count() <= 1 || !self.contains(id) {
            return None;
        }
        self.zoomed = false;

        let next_focus = self.root().sibling_first_pane_id(id);
        let root = self.root.take().expect("tab pane tree");
        let (root, removed) = root.remove(id);
        self.root = Some(root.expect("tab keeps at least one pane"));

        if self.focused_pane == id {
            self.focused_pane = next_focus.unwrap_or_else(|| self.first_pane_id());
        }
        removed
    }
}

/// A project groups a set of tabs bound to a local folder.
///
/// Tabs spawned inside a project default their working directory to [`Self::root`]. A window owns
/// one or more projects and shows the active project's tabs in the tab bar.
struct Project {
    /// Display name shown in the project sidebar.
    name: String,
    /// Folder new tabs in this project are rooted at. `None` for the default (home) project.
    root: Option<PathBuf>,
    /// Terminals belonging to this project. Always contains at least one tab.
    tabs: Vec<Tab>,
    /// Index of the focused tab within [`Self::tabs`].
    active_tab: usize,
    /// Cached Claude Code sessions for this project's `root`, shown in the sidebar. Refreshed when
    /// the project is selected (never per frame); empty for the root-less home project.
    sessions: Vec<ClaudeSession>,
    /// Cached data for the right-side project pane (file tree, git changes, diffs), fed by the
    /// background git worker and passed to the renderer by reference each frame.
    pane: ProjectPaneData,
}

impl Project {
    /// Derive a sidebar label from a project root (folder basename, or `~` for the default).
    fn name_for(root: &Option<PathBuf>) -> String {
        match root {
            Some(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned()),
            None => "~".to_owned(),
        }
    }
}

/// Event context for one individual Kairos window.
///
/// A window owns one or more [`Tab`]s (terminals) and the shared rendering resources used to draw
/// the currently active tab.
pub struct WindowContext {
    pub message_buffer: MessageBuffer,
    pub display: Display,
    pub dirty: bool,
    event_queue: Vec<WinitEvent<Event>>,
    /// Projects hosted in this window. Always contains at least one project.
    projects: Vec<Project>,
    /// Index of the currently active project within [`Self::projects`].
    active_project: usize,
    /// Stable key used to persist this window's layout across restarts.
    pub session_key: i64,
    /// Whether this window participates in session persistence.
    ///
    /// One-off windows launched with an explicit command (`kairos -e ...`) are ephemeral and
    /// must never be written to or restored from the session store.
    pub persistent: bool,
    cursor_blink_timed_out: bool,
    prev_bell_cmd: Option<Instant>,
    modifiers: Modifiers,
    mouse: Mouse,
    touch: TouchPurpose,
    occluded: bool,
    window_config: ParsedOptions,
    config: Rc<UiConfig>,
    /// Background thread answering git queries for the project pane.
    git_worker: GitWorker,
    /// Pane that captured the mouse on button press; mouse events stick to it until release so
    /// selection drags never migrate across pane boundaries.
    mouse_grab_pane: Option<u64>,
    /// Pane currently under the pointer, refreshed on cursor movement.
    hovered_pane: Option<u64>,
    /// In-progress pane divider drag, if any.
    pane_drag: Option<PaneDrag>,
}

impl WindowContext {
    /// Create initial window context that does bootstrapping the graphics API we're going to use.
    pub fn initial(
        event_loop: &ActiveEventLoop,
        proxy: EventLoopProxy<Event>,
        config: Rc<UiConfig>,
        mut options: WindowOptions,
    ) -> Result<Self, Box<dyn Error>> {
        let raw_display_handle = event_loop.display_handle().unwrap().as_raw();

        let mut identity = config.window.identity.clone();
        options.window_identity.override_identity_config(&mut identity);

        // Windows has different order of GL platform initialization compared to any other platform;
        // it requires the window first.
        #[cfg(windows)]
        let window = Window::new(event_loop, &config, &identity, &mut options)?;
        #[cfg(windows)]
        let raw_window_handle = Some(window.raw_window_handle());

        #[cfg(not(windows))]
        let raw_window_handle = None;

        let gl_display = renderer::platform::create_gl_display(
            raw_display_handle,
            raw_window_handle,
            config.debug.prefer_egl,
        )?;
        let gl_config = renderer::platform::pick_gl_config(&gl_display, raw_window_handle)?;

        #[cfg(not(windows))]
        let window = Window::new(
            event_loop,
            &config,
            &identity,
            &mut options,
            #[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
            gl_config.x11_visual(),
        )?;

        // Create context.
        let gl_context =
            renderer::platform::create_gl_context(&gl_display, &gl_config, raw_window_handle)?;

        let display = Display::new(window, gl_context, &config, false, event_loop)?;

        Self::new(display, config, options, proxy)
    }

    /// Create additional context with the graphics platform other windows are using.
    pub fn additional(
        gl_config: &GlutinConfig,
        event_loop: &ActiveEventLoop,
        proxy: EventLoopProxy<Event>,
        config: Rc<UiConfig>,
        mut options: WindowOptions,
        config_overrides: ParsedOptions,
    ) -> Result<Self, Box<dyn Error>> {
        let gl_display = gl_config.display();

        let mut identity = config.window.identity.clone();
        options.window_identity.override_identity_config(&mut identity);

        // Check if new window will be opened as a tab.
        // This must be done before `Window::new()`, which unsets `window_tabbing_id`.
        #[cfg(target_os = "macos")]
        let tabbed = options.window_tabbing_id.is_some();
        #[cfg(not(target_os = "macos"))]
        let tabbed = false;

        let window = Window::new(
            event_loop,
            &config,
            &identity,
            &mut options,
            #[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
            gl_config.x11_visual(),
        )?;

        // Create context.
        let raw_window_handle = window.raw_window_handle();
        let gl_context =
            renderer::platform::create_gl_context(&gl_display, gl_config, Some(raw_window_handle))?;

        let display = Display::new(window, gl_context, &config, tabbed, event_loop)?;

        let mut window_context = Self::new(display, config, options, proxy)?;

        // Set the config overrides at startup.
        //
        // These are already applied to `config`, so no update is necessary.
        window_context.window_config = config_overrides;

        Ok(window_context)
    }

    /// Create a new terminal window context.
    fn new(
        display: Display,
        config: Rc<UiConfig>,
        options: WindowOptions,
        proxy: EventLoopProxy<Event>,
    ) -> Result<Self, Box<dyn Error>> {
        info!(
            "PTY dimensions: {:?} x {:?}",
            display.size_info.screen_lines(),
            display.size_info.columns()
        );

        // Windows launched to run a specific command are one-offs and never persisted.
        let persistent = options.terminal_options.command().is_none();

        // The git worker outlives every project; it answers for whichever root is queried.
        let git_worker = GitWorker::spawn(display.window.id(), proxy.clone());

        // Create the initial tab (terminal) for this window.
        let tab = Tab::new_single(&config, &options, &display.size_info, display.window.id(), proxy)?;

        // The initial window is itself a project, rooted at its launch directory (if any).
        let root = options.terminal_options.working_directory.clone();
        let project = Project {
            name: Project::name_for(&root),
            pane: ProjectPaneData::new(root.clone()),
            root,
            tabs: vec![tab],
            active_tab: 0,
            sessions: Vec::new(),
        };

        // Create context for the Kairos window.
        let mut window_context = WindowContext {
            display,
            config,
            projects: vec![project],
            active_project: 0,
            session_key: 0,
            persistent,
            git_worker,
            cursor_blink_timed_out: Default::default(),
            prev_bell_cmd: Default::default(),
            message_buffer: Default::default(),
            window_config: Default::default(),
            event_queue: Default::default(),
            modifiers: Default::default(),
            occluded: Default::default(),
            mouse: Default::default(),
            touch: Default::default(),
            dirty: Default::default(),
            mouse_grab_pane: None,
            hovered_pane: None,
            pane_drag: None,
        };

        // Populate the active project's Claude session list for the sidebar and kick off the
        // first git status query for the project pane.
        window_context.refresh_project_sessions(0);
        window_context.refresh_pane_git();

        Ok(window_context)
    }

    /// Reference to the currently active project.
    #[inline]
    fn active_project(&self) -> &Project {
        &self.projects[self.active_project]
    }

    /// Reference to the currently active tab (active tab of the active project).
    #[inline]
    fn active_tab(&self) -> &Tab {
        let project = self.active_project();
        &project.tabs[project.active_tab]
    }

    /// Working directory of the focused pane's foreground process, if it can be resolved.
    #[cfg(not(windows))]
    pub fn active_working_directory(&self) -> Option<PathBuf> {
        self.pane_working_directory(self.active_tab().focused())
    }

    /// Working directory of a specific pane's foreground process, if it can be resolved.
    #[cfg(not(windows))]
    fn pane_working_directory(&self, pane: &Pane) -> Option<PathBuf> {
        crate::daemon::foreground_process_path(pane.master_fd, pane.shell_pid).ok()
    }

    #[cfg(windows)]
    fn pane_working_directory(&self, _pane: &Pane) -> Option<PathBuf> {
        None
    }

    /// Persisted layout of `tab`'s panes; `None` for single-pane tabs.
    fn pane_layout_snapshot(&self, tab: &Tab) -> Option<SavedPaneLayout> {
        fn node_layout(
            this: &WindowContext,
            node: &PaneNode,
            focused_id: u64,
        ) -> SavedPaneLayout {
            match node {
                PaneNode::Leaf(pane) => SavedPaneLayout::Leaf {
                    cwd: this.pane_working_directory(pane),
                    focused: pane.terminal_id == focused_id,
                },
                PaneNode::Split(split) => SavedPaneLayout::Split {
                    direction: match split.direction {
                        SplitDirection::Right => SavedSplitDirection::Right,
                        SplitDirection::Down => SavedSplitDirection::Down,
                    },
                    ratio: split.ratio,
                    first: Box::new(node_layout(this, &split.first, focused_id)),
                    second: Box::new(node_layout(this, &split.second, focused_id)),
                },
            }
        }

        if tab.pane_count() <= 1 {
            return None;
        }
        Some(node_layout(self, tab.root(), tab.focused().terminal_id))
    }

    /// Capture this window's layout (projects, tabs, geometry) for session persistence.
    pub fn session_snapshot(&self) -> SavedWindow {
        let projects = self
            .projects
            .iter()
            .map(|project| {
                let tabs = project
                    .tabs
                    .iter()
                    .map(|tab| SavedTab {
                        working_directory: self.pane_working_directory(tab.focused()),
                        title: tab.title().to_owned(),
                        layout: self.pane_layout_snapshot(tab),
                    })
                    .collect();
                SavedProject {
                    name: project.name.clone(),
                    root: project.root.clone(),
                    active_tab: project.active_tab,
                    tabs,
                }
            })
            .collect();

        let size = self.display.window.winit_window().inner_size();

        SavedWindow {
            key: self.session_key,
            width: size.width,
            height: size.height,
            active_project: self.active_project,
            pane_visible: self.display.pane_visible(),
            pane_cols: self.display.pane_cols(),
            projects,
        }
    }

    /// Recreate the remaining tabs of a restored window.
    ///
    /// The window is created with its first tab already spawned (in `tabs[0]`'s directory), so
    /// this adds `tabs[1..]`, restores the saved titles and focuses the previously active tab.
    /// Recreate a restored window's projects and tabs.
    ///
    /// The window is created with one bootstrap project holding one tab (spawned in the first
    /// saved tab's directory). This configures that project, spawns the remaining tabs, then
    /// rebuilds the other projects with their tabs and finally focuses the saved active project.
    pub fn restore_projects(
        &mut self,
        proxy: EventLoopProxy<Event>,
        saved: &[SavedProject],
        active_project: usize,
    ) {
        if saved.is_empty() {
            return;
        }

        // Configure the bootstrap project (index 0, already holding tab 0) from `saved[0]`.
        self.projects[0].name = saved[0].name.clone();
        self.projects[0].root = saved[0].root.clone();
        self.projects[0].pane = ProjectPaneData::new(saved[0].root.clone());

        // Add placeholder projects for the rest; tabs are spawned below.
        for sp in saved.iter().skip(1) {
            self.projects.push(Project {
                name: sp.name.clone(),
                pane: ProjectPaneData::new(sp.root.clone()),
                root: sp.root.clone(),
                tabs: Vec::new(),
                active_tab: 0,
                sessions: Vec::new(),
            });
        }

        // Spawn each project's tabs. Project 0 already has its first tab, so skip it there.
        let window_id = self.display.window.id();
        for (pi, sp) in saved.iter().enumerate() {
            let start = usize::from(pi == 0);
            for tab in sp.tabs.iter().skip(start) {
                let mut options = WindowOptions::default();
                options.terminal_options.working_directory =
                    tab.working_directory.clone().or_else(|| sp.root.clone());
                match Tab::new_single(
                    &self.config,
                    &options,
                    &self.display.size_info,
                    window_id,
                    proxy.clone(),
                ) {
                    Ok(tab) => self.projects[pi].tabs.push(tab),
                    Err(err) => error!("Unable to restore tab: {err}"),
                }
            }
        }

        // Rebuild each tab's pane split layout, spawning the additional panes.
        let config = self.config.clone();
        let size_info = self.display.size_info;
        for (pi, sp) in saved.iter().enumerate() {
            let project_root = sp.root.clone();
            let project = &mut self.projects[pi];
            for (tab, st) in project.tabs.iter_mut().zip(&sp.tabs) {
                let Some(layout) = &st.layout else { continue };
                let mut spawn = |cwd: Option<&PathBuf>| {
                    let mut options = WindowOptions::default();
                    options.terminal_options.working_directory =
                        cwd.cloned().or_else(|| project_root.clone());
                    match Pane::new(&config, &options, &size_info, window_id, proxy.clone()) {
                        Ok(pane) => Some(pane),
                        Err(err) => {
                            error!("Unable to restore pane: {err}");
                            None
                        },
                    }
                };
                tab.apply_saved_layout(layout, &mut spawn);
            }
        }

        // Seed saved titles and clamp each project's active tab.
        for (pi, sp) in saved.iter().enumerate() {
            let project = &mut self.projects[pi];
            for (tab, st) in project.tabs.iter_mut().zip(&sp.tabs) {
                if !st.title.is_empty() {
                    tab.focused_mut().title = st.title.clone();
                }
            }
            project.active_tab = sp.active_tab.min(project.tabs.len().saturating_sub(1));
        }

        // Drop any project whose tabs all failed to spawn, then focus the saved active project.
        self.projects.retain(|p| !p.tabs.is_empty());
        self.active_project = active_project.min(self.projects.len().saturating_sub(1));
        self.refresh_project_sessions(self.active_project);
        self.refresh_pane_git();
        self.sync_tabs();
    }

    /// Resize the window to the given physical pixel dimensions (used when restoring).
    pub fn set_pixel_size(&self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        let _ =
            self.display.window.winit_window().request_inner_size(PhysicalSize::new(width, height));
    }

    /// Spawn a new tab in the active project and focus it. Returns whether the tab was created.
    ///
    /// When the active project has a root folder it takes precedence, so tabs opened inside a
    /// project always start in the project directory.
    pub fn add_tab(
        &mut self,
        proxy: EventLoopProxy<Event>,
        working_directory: Option<PathBuf>,
    ) -> bool {
        self.spawn_tab_in(self.active_project, proxy, working_directory)
    }

    /// Spawn a fresh shell tab in the active project, then type `command` into it (submitted with
    /// the `\r` the shell's line discipline treats as Enter — a `\n` would only insert the text).
    /// Seeds the tab `title` when non-empty and records `claude_session` for resume deduplication.
    /// Does nothing when the tab fails to spawn, so the command never leaks into a pre-existing pane.
    fn spawn_command_tab(
        &mut self,
        proxy: EventLoopProxy<Event>,
        command: Vec<u8>,
        title: String,
        claude_session: Option<String>,
    ) {
        if !self.add_tab(proxy, None) {
            return;
        }
        if let Some(tab) = self.projects[self.active_project].tabs.last_mut() {
            let pane = tab.focused_mut();
            pane.claude_session = claude_session;
            pane.notifier.notify(command);
            if !title.is_empty() {
                pane.title = title;
            }
        }
    }

    /// Spawn a fresh shell tab in `project_index`, rooted at the project folder (falling back to
    /// `working_directory`), make it that project's active tab, and mark the layout dirty. Returns
    /// whether the tab was created.
    fn spawn_tab_in(
        &mut self,
        project_index: usize,
        proxy: EventLoopProxy<Event>,
        working_directory: Option<PathBuf>,
    ) -> bool {
        let working_directory = self.projects[project_index].root.clone().or(working_directory);

        let mut options = WindowOptions::default();
        options.terminal_options.working_directory = working_directory;

        let window_id = self.display.window.id();
        match Tab::new_single(&self.config, &options, &self.display.size_info, window_id, proxy) {
            Ok(tab) => {
                let project = &mut self.projects[project_index];
                project.tabs.push(tab);
                project.active_tab = project.tabs.len() - 1;
                // The new terminal takes focus, including from a focused viewer tab.
                self.display.unfocus_viewer();
                // The tab bar may have just appeared; recompute layout and redraw.
                self.display.pending_update.dirty = true;
                self.dirty = true;
                true
            },
            Err(err) => {
                error!("Unable to create new tab: {err}");
                false
            },
        }
    }

    /// Open a new tab in `project_index` that resumes the Claude Code session `session_id`.
    ///
    /// Spawns a normal shell rooted at the project folder, then types `claude --resume <id>` into
    /// it so a usable prompt remains after the session ends. `session_id` is validated to a UUID
    /// charset where it's discovered, so writing it into the shell carries no injection risk.
    pub fn open_claude_session(
        &mut self,
        proxy: EventLoopProxy<Event>,
        project_index: usize,
        session_id: String,
    ) {
        if self.projects.get(project_index).map(|p| p.root.is_none()).unwrap_or(true) {
            return;
        }

        // New tabs are added to the active project, so focus the target project first.
        self.select_project(project_index);

        // If this session is already open in a tab, just focus that tab instead of duplicating it.
        if let Some(tab_index) = self.projects[project_index]
            .tabs
            .iter()
            .position(|t| t.focused().claude_session.as_deref() == Some(session_id.as_str()))
        {
            self.set_active_tab(tab_index);
            return;
        }

        // Label the new tab with the session's prompt, if cached.
        let label = self.projects[project_index]
            .sessions
            .iter()
            .find(|s| s.id == session_id)
            .map(|s| s.label.clone());

        let command = format!("claude --resume {session_id}\r").into_bytes();
        self.spawn_command_tab(proxy, command, label.unwrap_or_default(), Some(session_id));
    }

    /// Open a new tab that starts a fresh Claude Code session.
    ///
    /// Like [`Self::open_claude_session`] but runs `claude` without `--resume`, so a new
    /// conversation begins. Spawns a normal shell rooted at the active project folder (falling back
    /// to the default working directory when no project is open), then types `claude` into it so a
    /// usable prompt remains after the session ends.
    pub fn new_claude_tab(&mut self, proxy: EventLoopProxy<Event>) {
        // Seed a title so the tab reads "Claude" until the CLI emits its own OSC title.
        self.spawn_command_tab(proxy, b"claude\r".to_vec(), "Claude".to_owned(), None);
    }

    /// Create a new project rooted at `root` (with one tab) and switch to it.
    pub fn add_project(&mut self, proxy: EventLoopProxy<Event>, root: PathBuf) {
        let mut options = WindowOptions::default();
        options.terminal_options.working_directory = Some(root.clone());

        let window_id = self.display.window.id();
        match Tab::new_single(&self.config, &options, &self.display.size_info, window_id, proxy) {
            Ok(tab) => {
                let name = Project::name_for(&Some(root.clone()));
                self.projects.push(Project {
                    name,
                    pane: ProjectPaneData::new(Some(root.clone())),
                    root: Some(root),
                    tabs: vec![tab],
                    active_tab: 0,
                    sessions: Vec::new(),
                });
                self.select_project(self.projects.len() - 1);
                self.display.pending_update.dirty = true;
                self.dirty = true;
            },
            Err(err) => error!("Unable to create new project: {err}"),
        }
    }

    /// Reload the cached Claude Code sessions for the project at `index` (no-op for the root-less
    /// home project). Called on project selection, never per frame.
    fn refresh_project_sessions(&mut self, index: usize) {
        let Some(project) = self.projects.get_mut(index) else { return };
        project.sessions = match &project.root {
            Some(root) => claude_sessions::sessions_for(root),
            None => Vec::new(),
        };
    }

    /// Queue a git status refresh for the active project's pane (no-op while the pane is hidden
    /// or for the root-less home project).
    pub fn refresh_pane_git(&mut self) {
        if !self.display.pane_visible() {
            return;
        }
        let Some(root) = self.active_project().root.clone() else { return };
        self.git_worker.request_refresh(root);
    }

    /// Apply a background git result to every project with the root it was computed for —
    /// duplicate-root projects are legal and must all stay current. Results for a root no longer
    /// hosted in this window (e.g. its project was closed) are dropped.
    pub fn apply_pane_git_data(&mut self, root: PathBuf, data: GitData) {
        let matches: Vec<usize> = self
            .projects
            .iter()
            .enumerate()
            .filter(|(_, p)| p.root.as_deref() == Some(root.as_path()))
            .map(|(i, _)| i)
            .collect();
        if matches.is_empty() {
            return;
        }

        let selected = self.display.pane_selected();
        let is_status = matches!(data, GitData::Status { .. });
        let difft_failed = matches!(data, GitData::Difft { lines: None, .. });
        for &index in &matches {
            match data.clone() {
                GitData::Status { changed, ignored, is_repo } => {
                    self.projects[index].pane.apply_status(changed, ignored, is_repo,
                        selected.as_deref());
                },
                GitData::Diff { file, lines } => self.projects[index].pane.apply_diff(file, lines),
                GitData::Difft { file, split, lines } => {
                    self.projects[index].pane.apply_difft(file, split, lines);
                },
            }
        }

        // Redraw only when the result affects the project currently on screen.
        if matches.contains(&self.active_project) {
            // A status refresh invalidates the open file's retained diff data; fetch a fresh
            // result to replace it in place.
            if is_status && selected.is_some() {
                self.ensure_diff_data(true);
            }

            // difft turned out to be unavailable: fall back to the builtin diff.
            if difft_failed {
                self.ensure_diff_data(false);
            }

            self.display.pending_update.dirty = true;
            self.dirty = true;
            if self.display.window.has_frame {
                self.display.window.request_redraw();
            }
        }
    }

    /// Point the shared diff tab at the changed file `path`, requesting any missing diff data.
    fn toggle_pane_change(&mut self, path: &str) {
        if self.projects[self.active_project].pane.changed_by_path(path).is_none() {
            return;
        }
        self.display.open_diff_viewer(path);
        self.ensure_diff_data(false);
        self.dirty = true;
    }

    /// Queue whatever data the open diff viewer is missing: the difftastic rendering of the
    /// current `(file, layout)` when difft mode is active, the builtin parsed diff otherwise.
    /// `force` re-requests even when a (possibly stale) result is already cached, so the
    /// periodic git refresh replaces the entry in place.
    fn ensure_diff_data(&mut self, force: bool) {
        let project = &self.projects[self.active_project];
        let Some(root) = project.root.clone() else { return };
        let Some(path) = self.display.pane_selected() else { return };
        let Some(file) = project.pane.changed_by_path(&path).cloned() else { return };

        if self.display.viewer_difft() && !project.pane.difft_missing() {
            let split = self.display.viewer_split();
            if force || !project.pane.has_difft(&path, split) {
                let width = self.display.viewer_text_cols();
                self.git_worker.request_difft(root, path, split, width);
            }
        } else if force || !project.pane.has_diff(&path) {
            self.git_worker.request_diff(root, file);
        }
    }

    /// Point the shared 预览 tab at the project file `path`, (re)loading its content from disk.
    fn toggle_pane_preview(&mut self, path: &str) {
        self.display.open_file_preview(path);
        self.reload_preview(path);
        self.dirty = true;
    }

    /// (Re)load the previewed file's content, honoring the markdown view mode and the viewer's
    /// current width (used to wrap rendered markdown).
    fn reload_preview(&mut self, path: &str) {
        let width = self.display.viewer_text_cols();
        let md_source = self.display.md_source();
        self.projects[self.active_project].pane.load_preview(path, width, md_source);
    }

    /// Refresh the project pane's directory to the latest on-disk / git state: re-list the file
    /// tree (picking up added/removed files), re-run git status (updating the Changes list, which
    /// also refreshes an open diff), and reload an open file preview from disk.
    fn refresh_pane_dir(&mut self) {
        self.projects[self.active_project].pane.refresh_tree();
        self.refresh_pane_git();
        if let Some(path) = self.display.previewed_file() {
            self.reload_preview(&path);
        }
        self.display.pending_update.dirty = true;
        self.dirty = true;
    }

    /// Switch the active project, carrying focus and updating the window title.
    pub fn select_project(&mut self, index: usize) {
        if index >= self.projects.len() {
            return;
        }

        // Refresh the target project's session list (also lets re-clicking a project refresh it).
        self.refresh_project_sessions(index);

        if index == self.active_project {
            // Already active; the refreshed session list may need a redraw, and re-clicking also
            // refreshes the pane's git data.
            self.refresh_pane_git();
            self.display.pending_update.dirty = true;
            self.dirty = true;
            return;
        }

        // A divider drag or mouse grab belongs to the layout being left behind.
        self.pane_drag = None;
        self.mouse_grab_pane = None;

        // Carry the focus state from the old active tab to the new project's active tab.
        let old_project = self.active_project;
        let old_tab = self.projects[old_project].active_tab;
        let focused = self.projects[old_project].tabs[old_tab].focused().terminal.lock().is_focused;

        self.active_project = index;

        let project = &self.projects[index];
        let new_tab = &project.tabs[project.active_tab];
        new_tab.apply_focus_flag(focused);

        // Reflect the newly-focused tab's title in the window title.
        let title = new_tab.title().to_owned();
        let preserve_title = new_tab.focused().preserve_title;
        if !preserve_title && self.config.window.dynamic_title && !title.is_empty() {
            self.display.window.set_title(title);
        }

        // The pane now shows the new project: drop the old project's view state (scrolls,
        // expanded diff, folds) and bring the new one's git data up to date.
        self.display.pane_reset_view();
        self.refresh_pane_git();

        self.display.pending_update.dirty = true;
        self.dirty = true;
    }

    /// Close the pane with the given terminal id, wherever it lives.
    ///
    /// The exiting pane may belong to a background project, so all projects are searched. When a
    /// tab's last pane closes, the tab itself is removed; when a project's last tab closes the
    /// project respawns a fresh shell (or is removed if that fails). Returns `true` only when the
    /// window's very last tab (of its last project) is gone and the window should close.
    pub fn close_pane(&mut self, proxy: EventLoopProxy<Event>, terminal_id: u64) -> bool {
        // Locate the containing tab across all projects.
        let located = self.projects.iter().enumerate().find_map(|(pi, project)| {
            project.tabs.iter().position(|t| t.contains(terminal_id)).map(|ti| (pi, ti))
        });
        let (project_index, tab_index) = match located {
            Some(loc) => loc,
            None => return self.projects.is_empty(),
        };

        // With sibling panes remaining, only the exited pane is removed; the tab survives.
        if self.projects[project_index].tabs[tab_index].pane_count() > 1 {
            let tab = &mut self.projects[project_index].tabs[tab_index];
            let window_focused = tab.focused().terminal.lock().is_focused;
            // Dropping the removed pane shuts down its PTY.
            drop(tab.close_pane(terminal_id));
            tab.apply_focus_flag(window_focused);
            self.relayout_tab(project_index, tab_index);
            self.sync_window_title();
            self.dirty = true;
            return false;
        }

        // Dropping the tab shuts down its PTY.
        self.projects[project_index].tabs.remove(tab_index);

        if self.projects[project_index].tabs.is_empty() {
            // A project always keeps at least one terminal: closing its last tab respawns a fresh
            // shell in place rather than deleting the project. Projects are removed only via the
            // sidebar's explicit delete button (`close_project`).
            self.spawn_tab_in(project_index, proxy, None);

            // Guard the invariant: if the shell failed to spawn, don't leave a tab-less project
            // (which would panic on access) — fall back to removing it as before.
            if self.projects[project_index].tabs.is_empty() {
                self.projects.remove(project_index);
                if self.projects.is_empty() {
                    return true;
                }
                // Keep `active_project` pointing at a valid project.
                if project_index < self.active_project {
                    self.active_project -= 1;
                } else if self.active_project >= self.projects.len() {
                    self.active_project = self.projects.len() - 1;
                }
            }
        } else {
            // Keep the project's `active_tab` pointing at a valid tab.
            let project = &mut self.projects[project_index];
            if tab_index < project.active_tab {
                project.active_tab -= 1;
            } else if project.active_tab >= project.tabs.len() {
                project.active_tab = project.tabs.len() - 1;
            }
        }

        self.display.pending_update.dirty = true;
        self.dirty = true;
        false
    }

    /// Split the active tab's focused pane, spawning a fresh shell in the new pane.
    ///
    /// The new pane prefers the focused pane's live working directory (`working_directory`,
    /// resolvable on unix only), falling back to the project root — the opposite precedence
    /// from new tabs, matching the tmux intuition that a split continues where you are.
    /// Returns whether a pane was spawned.
    pub fn split_active_pane(
        &mut self,
        proxy: EventLoopProxy<Event>,
        direction: SplitDirection,
        working_directory: Option<PathBuf>,
    ) -> bool {
        let project_index = self.active_project;
        let tab_index = self.projects[project_index].active_tab;

        // Reject the split when the focused pane can't host two minimum-size children plus the
        // divider; ring the visual bell as feedback.
        {
            let size_info = &self.projects[project_index].tabs[tab_index].focused().size_info;
            let fits = match direction {
                SplitDirection::Right => {
                    let pane_width =
                        size_info.width() - size_info.left_extra() - size_info.right_extra();
                    let needed = 2.
                        * (MIN_PANE_COLUMNS as f32 * size_info.cell_width()
                            + 2. * size_info.padding_x())
                        + DIVIDER_PX;
                    pane_width >= needed
                },
                SplitDirection::Down => {
                    let pane_height =
                        size_info.height() - size_info.top_extra() - size_info.bottom_extra();
                    let needed = 2.
                        * (MIN_PANE_LINES as f32 * size_info.cell_height()
                            + 2. * size_info.padding_y())
                        + DIVIDER_PX;
                    pane_height >= needed
                },
            };
            if !fits {
                self.display.visual_bell.ring();
                self.dirty = true;
                return false;
            }
        }

        let working_directory =
            working_directory.or_else(|| self.projects[project_index].root.clone());
        let mut options = WindowOptions::default();
        options.terminal_options.working_directory = working_directory;

        let window_id = self.display.window.id();
        let pane =
            match Pane::new(&self.config, &options, &self.display.size_info, window_id, proxy) {
                Ok(pane) => pane,
                Err(err) => {
                    error!("Unable to split pane: {err}");
                    return false;
                },
            };

        let tab = &mut self.projects[project_index].tabs[tab_index];
        let window_focused = tab.focused().terminal.lock().is_focused;
        tab.split(direction, pane);
        tab.apply_focus_flag(window_focused);

        self.dirty = true;
        true
    }

    /// Exit the focused pane's terminal; the resulting `Exit` event removes the pane (the tab
    /// when it is the last pane).
    pub fn request_close_focused_pane(&mut self) {
        self.active_tab().focused().terminal.lock().exit();
    }

    /// Move focus to the geometrically nearest pane in `direction` within the active tab.
    ///
    /// Navigating while zoomed restores the split layout first. At the layout's edge this is a
    /// no-op (no wrapping).
    pub fn focus_pane_direction(&mut self, direction: PaneDirection) {
        let project_index = self.active_project;
        let tab_index = self.projects[project_index].active_tab;

        if self.projects[project_index].tabs[tab_index].zoomed {
            self.projects[project_index].tabs[tab_index].zoomed = false;
            self.relayout_tab(project_index, tab_index);
        }

        let content = self.display.content_rect();
        let tab = &self.projects[project_index].tabs[tab_index];
        if tab.pane_count() <= 1 {
            return;
        }
        let (rects, _) = tab.compute_layout(content);
        let focused_id = tab.focused().terminal_id;
        let Some(&(_, focused_rect)) = rects.iter().find(|(id, _)| *id == focused_id) else {
            return;
        };

        // Candidates lie strictly beyond the focused pane's edge (a 1px epsilon absorbs the
        // divider gap) and overlap its span on the perpendicular axis; the winner is the
        // closest along the axis, breaking ties by largest perpendicular overlap.
        let mut best: Option<(u64, f32, f32)> = None;
        for &(id, rect) in &rects {
            if id == focused_id {
                continue;
            }
            let (beyond, distance) = match direction {
                PaneDirection::Left => (
                    rect.x + rect.w <= focused_rect.x + 1.,
                    focused_rect.x - (rect.x + rect.w),
                ),
                PaneDirection::Right => (
                    rect.x >= focused_rect.x + focused_rect.w - 1.,
                    rect.x - (focused_rect.x + focused_rect.w),
                ),
                PaneDirection::Up => (
                    rect.y + rect.h <= focused_rect.y + 1.,
                    focused_rect.y - (rect.y + rect.h),
                ),
                PaneDirection::Down => (
                    rect.y >= focused_rect.y + focused_rect.h - 1.,
                    rect.y - (focused_rect.y + focused_rect.h),
                ),
            };
            if !beyond {
                continue;
            }
            let overlap = match direction {
                PaneDirection::Left | PaneDirection::Right => {
                    (rect.y + rect.h).min(focused_rect.y + focused_rect.h)
                        - rect.y.max(focused_rect.y)
                },
                PaneDirection::Up | PaneDirection::Down => {
                    (rect.x + rect.w).min(focused_rect.x + focused_rect.w)
                        - rect.x.max(focused_rect.x)
                },
            };
            if overlap <= 0. {
                continue;
            }
            let better = match best {
                None => true,
                Some((_, best_distance, best_overlap)) => {
                    distance < best_distance - 0.5
                        || (distance <= best_distance + 0.5 && overlap > best_overlap)
                },
            };
            if better {
                best = Some((id, distance, overlap));
            }
        }

        if let Some((id, ..)) = best {
            self.focus_pane_by_id(id);
        }
    }

    /// Focus the next pane of the active tab in layout order, wrapping around.
    pub fn focus_next_pane(&mut self) {
        let project_index = self.active_project;
        let tab_index = self.projects[project_index].active_tab;

        if self.projects[project_index].tabs[tab_index].zoomed {
            self.projects[project_index].tabs[tab_index].zoomed = false;
            self.relayout_tab(project_index, tab_index);
        }

        let tab = &self.projects[project_index].tabs[tab_index];
        let ids = tab.pane_ids();
        if ids.len() <= 1 {
            return;
        }
        let focused_id = tab.focused().terminal_id;
        let position = ids.iter().position(|id| *id == focused_id).unwrap_or(0);
        let next = ids[(position + 1) % ids.len()];
        self.focus_pane_by_id(next);
    }

    /// Focus pane `id` of the active tab, carrying the window focus flag and the title.
    fn focus_pane_by_id(&mut self, id: u64) {
        let project_index = self.active_project;
        let tab_index = self.projects[project_index].active_tab;
        let tab = &mut self.projects[project_index].tabs[tab_index];
        if !tab.contains(id) || tab.focused().terminal_id == id {
            return;
        }
        let window_focused = tab.focused().terminal.lock().is_focused;
        tab.focused_pane = id;
        tab.apply_focus_flag(window_focused);
        self.sync_window_title();
        self.dirty = true;
    }

    /// Move the divider of the deepest same-axis split containing the focused pane by one cell
    /// in `direction`. No-op while zoomed or without a matching split.
    pub fn resize_focused_pane(&mut self, direction: PaneDirection) {
        let project_index = self.active_project;
        let tab_index = self.projects[project_index].active_tab;
        let tab = &self.projects[project_index].tabs[tab_index];
        if tab.zoomed || tab.pane_count() <= 1 {
            return;
        }

        let axis = match direction {
            PaneDirection::Left | PaneDirection::Right => SplitDirection::Right,
            PaneDirection::Up | PaneDirection::Down => SplitDirection::Down,
        };

        // Walk the path from the root to the focused leaf, remembering the deepest split along
        // the matching axis (tmux behavior: the innermost divider follows the keys).
        let focused_id = tab.focused().terminal_id;
        let mut path = Vec::new();
        if !tab.root().path_to(focused_id, &mut path) {
            return;
        }
        let mut node = tab.root();
        let mut deepest: Option<usize> = None;
        for (depth, slot) in path.iter().enumerate() {
            let PaneNode::Split(split) = node else { break };
            if split.direction == axis {
                deepest = Some(depth);
            }
            node = match slot {
                ChildSlot::First => &split.first,
                ChildSlot::Second => &split.second,
            };
        }
        let Some(depth) = deepest else { return };

        // One cell per keypress, converted into a ratio delta over the split's extent; the
        // shared edge moves in the arrow direction regardless of which side has focus.
        let size_info = self.display.size_info;
        let (cell, min_px) = match axis {
            SplitDirection::Right => (
                size_info.cell_width(),
                MIN_PANE_COLUMNS as f32 * size_info.cell_width() + 2. * size_info.padding_x(),
            ),
            SplitDirection::Down => (
                size_info.cell_height(),
                MIN_PANE_LINES as f32 * size_info.cell_height() + 2. * size_info.padding_y(),
            ),
        };
        let delta_px = match direction {
            PaneDirection::Left | PaneDirection::Up => -cell,
            PaneDirection::Right | PaneDirection::Down => cell,
        };

        let Some(extent) = tab
            .dividers
            .iter()
            .find(|divider| divider.path.len() == depth && divider.path[..] == path[..depth])
            .map(|divider| divider.span.1)
        else {
            return;
        };

        let tab = &mut self.projects[project_index].tabs[tab_index];
        let Some(split) = tab.root_mut().split_at_path_mut(&path[..depth]) else { return };
        let ratio = split.ratio + delta_px / (extent - DIVIDER_PX).max(1.);
        split.ratio = clamp_ratio(ratio, extent, min_px);

        self.relayout_tab(project_index, tab_index);
    }

    /// Maximize the focused pane over the tab's whole content area, or restore the splits.
    pub fn toggle_pane_zoom(&mut self) {
        let project_index = self.active_project;
        let tab_index = self.projects[project_index].active_tab;
        let tab = &mut self.projects[project_index].tabs[tab_index];
        if tab.pane_count() <= 1 {
            return;
        }
        tab.zoomed = !tab.zoomed;
        // The zoomed layout has no dividers; drop any in-progress drag.
        self.pane_drag = None;
        self.relayout_tab(project_index, tab_index);
        self.dirty = true;
    }

    /// The visible pane of the active tab under the window-pixel position, if any.
    fn pane_at_point(&self, x: f32, y: f32) -> Option<u64> {
        let tab = self.active_tab();
        if tab.zoomed {
            let size_info = &tab.focused().size_info;
            let inside = x >= size_info.left_extra()
                && x < size_info.width() - size_info.right_extra()
                && y >= size_info.top_extra()
                && y < size_info.height() - size_info.bottom_extra();
            return inside.then(|| tab.focused().terminal_id);
        }

        let mut hit = None;
        tab.for_each_pane(&mut |pane| {
            let size_info = &pane.size_info;
            if x >= size_info.left_extra()
                && x < size_info.width() - size_info.right_extra()
                && y >= size_info.top_extra()
                && y < size_info.height() - size_info.bottom_extra()
            {
                hit = Some(pane.terminal_id);
            }
        });
        hit
    }

    /// The active tab's divider whose grab zone contains the window-pixel position, if any.
    fn divider_at_point(&self, x: f32, y: f32) -> Option<&PaneDivider> {
        let tab = self.active_tab();
        if tab.zoomed {
            return None;
        }
        tab.dividers.iter().find(|divider| {
            let rect = divider.rect;
            x >= rect.x - DIVIDER_GRAB_PX
                && x < rect.x + rect.w + DIVIDER_GRAB_PX
                && y >= rect.y - DIVIDER_GRAB_PX
                && y < rect.y + rect.h + DIVIDER_GRAB_PX
        })
    }

    /// Handle the pane layer of a window event: divider hover/drag, click-to-focus and the
    /// mouse grab. Returns whether the event was consumed (and must not reach the terminal).
    ///
    /// Runs after the chrome (which keeps priority for its own divider and overlays) and before
    /// terminal input routing.
    fn handle_pane_mouse_event(&mut self, event: &WindowEvent) -> bool {
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                let (x, y) = (position.x as f32, position.y as f32);
                self.hovered_pane = self.pane_at_point(x, y);

                // Keep the window-level mouse position current even when this event is consumed
                // below — the divider press looks it up.
                self.mouse.x = position.x as usize;
                self.mouse.y = position.y as usize;

                // An active drag follows the pointer, relaying out live.
                if let Some(drag) = &self.pane_drag {
                    let position = match drag.direction {
                        SplitDirection::Right => x,
                        SplitDirection::Down => y,
                    };
                    let (start, extent) = drag.span;
                    let size_info = self.display.size_info;
                    let min_px = match drag.direction {
                        SplitDirection::Right => {
                            MIN_PANE_COLUMNS as f32 * size_info.cell_width()
                                + 2. * size_info.padding_x()
                        },
                        SplitDirection::Down => {
                            MIN_PANE_LINES as f32 * size_info.cell_height()
                                + 2. * size_info.padding_y()
                        },
                    };
                    let ratio =
                        clamp_ratio((position - start) / (extent - DIVIDER_PX).max(1.), extent, min_px);
                    let icon = match drag.direction {
                        SplitDirection::Right => CursorIcon::ColResize,
                        SplitDirection::Down => CursorIcon::RowResize,
                    };

                    let path = drag.path.clone();
                    let project_index = self.active_project;
                    let tab_index = self.projects[project_index].active_tab;
                    let tab = &mut self.projects[project_index].tabs[tab_index];
                    if let Some(split) = tab.root_mut().split_at_path_mut(&path) {
                        split.ratio = ratio;
                    }
                    self.relayout_tab(project_index, tab_index);
                    self.display.window.set_mouse_cursor(icon);
                    return true;
                }

                // Hovering a divider shows the resize cursor; consume so the terminal doesn't
                // immediately reset it.
                if self.mouse_grab_pane.is_none() {
                    if let Some(divider) = self.divider_at_point(x, y) {
                        let icon = match divider.direction {
                            SplitDirection::Right => CursorIcon::ColResize,
                            SplitDirection::Down => CursorIcon::RowResize,
                        };
                        self.display.window.set_mouse_cursor(icon);
                        return true;
                    }
                }

                false
            },
            WindowEvent::MouseInput { state: ElementState::Pressed, button, .. } => {
                let (x, y) = (self.mouse.x as f32, self.mouse.y as f32);

                // Grab the divider before pane focus: its zone straddles both neighbours.
                if *button == MouseButton::Left {
                    if let Some(divider) = self.divider_at_point(x, y) {
                        self.pane_drag = Some(PaneDrag {
                            path: divider.path.clone(),
                            direction: divider.direction,
                            span: divider.span,
                        });
                        return true;
                    }
                }

                // Click-to-focus: focus the pressed pane but let the press through, so one
                // click both focuses and acts (selection start, mouse-mode report).
                if let Some(id) = self.pane_at_point(x, y) {
                    self.mouse_grab_pane = Some(id);
                    if self.active_tab().focused().terminal_id != id {
                        self.focus_pane_by_id(id);
                    }
                }

                false
            },
            WindowEvent::MouseInput { state: ElementState::Released, .. } => {
                self.mouse_grab_pane = None;
                // Only the release that ends a divider drag is consumed.
                self.pane_drag.take().is_some()
            },
            WindowEvent::Focused(false) => {
                self.pane_drag = None;
                self.mouse_grab_pane = None;
                false
            },
            _ => false,
        }
    }

    /// Reflect the active tab's focused pane title in the window title.
    fn sync_window_title(&mut self) {
        let tab = self.active_tab();
        let title = tab.title().to_owned();
        let preserve_title = tab.focused().preserve_title;
        if !preserve_title && self.config.window.dynamic_title && !title.is_empty() {
            self.display.window.set_title(title);
        }
    }

    /// Delete the project at `index` and all its tabs. The window always keeps at least one
    /// project, so a request to delete the sole project is ignored. Does not touch the folder on
    /// disk — only removes it from this window's project list.
    pub fn close_project(&mut self, index: usize) {
        if index >= self.projects.len() || self.projects.len() <= 1 {
            return;
        }

        // Dropping the project drops its tabs, shutting down their PTYs.
        self.projects.remove(index);

        // Keep `active_project` valid and pointed at a sensible project.
        if index < self.active_project {
            self.active_project -= 1;
        } else if self.active_project >= self.projects.len() {
            self.active_project = self.projects.len() - 1;
        }

        // Focus the now-active project's tab and load its session list.
        let project = &self.projects[self.active_project];
        project.tabs[project.active_tab].apply_focus_flag(true);
        let active = self.active_project;
        self.refresh_project_sessions(active);

        self.display.pending_update.dirty = true;
        self.dirty = true;
    }

    /// Focus the tab at `index` in the active project, if it exists.
    pub fn select_tab(&mut self, index: usize) {
        if index < self.active_project().tabs.len() {
            self.set_active_tab(index);
        }
    }

    /// Focus the tab with the given tab id within the active project, if it still exists.
    pub fn select_tab_by_id(&mut self, tab_id: u64) {
        let index = self.active_project().tabs.iter().position(|tab| tab.tab_id == tab_id);
        if let Some(index) = index {
            self.set_active_tab(index);
        }
    }

    /// Request that the tab with the given tab id close, if it still exists.
    ///
    /// Exits every pane of the tab; each exit emits an `Exit` event tagged with its terminal id,
    /// which the event loop turns into a [`Self::close_pane`] — the last one removes the tab
    /// (respawning a fresh shell when it was the project's last tab). Targeting by id (rather
    /// than index) keeps this correct even if tabs shift between click and handling.
    pub fn request_close_tab(&mut self, tab_id: u64) {
        for project in &self.projects {
            if let Some(tab) = project.tabs.iter().find(|tab| tab.tab_id == tab_id) {
                tab.for_each_pane(&mut |pane| pane.terminal.lock().exit());
                return;
            }
        }
    }

    /// Focus the last tab of the active project.
    pub fn select_last_tab(&mut self) {
        self.set_active_tab(self.active_project().tabs.len() - 1);
    }

    /// Focus the next tab in the active project, wrapping around.
    pub fn select_next_tab(&mut self) {
        let project = self.active_project();
        let next = (project.active_tab + 1) % project.tabs.len();
        self.set_active_tab(next);
    }

    /// Focus the previous tab in the active project, wrapping around.
    pub fn select_previous_tab(&mut self) {
        let project = self.active_project();
        let prev = (project.active_tab + project.tabs.len() - 1) % project.tabs.len();
        self.set_active_tab(prev);
    }

    /// Focus the tab at `index` within the active project.
    fn set_active_tab(&mut self, index: usize) {
        // Tab switches always return to the terminal, even from a focused viewer tab.
        self.display.unfocus_viewer();

        let project_index = self.active_project;
        if index == self.projects[project_index].active_tab {
            return;
        }

        // A divider drag or mouse grab belongs to the layout being left behind.
        self.pane_drag = None;
        self.mouse_grab_pane = None;

        // Carry the focus state to the newly-active tab so its cursor renders correctly.
        let project = &mut self.projects[project_index];
        let active = project.active_tab;
        let focused = project.tabs[active].focused().terminal.lock().is_focused;
        project.tabs[index].apply_focus_flag(focused);
        project.active_tab = index;

        // Reflect the newly-focused tab's title in the window title.
        let title = project.tabs[index].title().to_owned();
        let preserve_title = project.tabs[index].focused().preserve_title;
        if !preserve_title && self.config.window.dynamic_title && !title.is_empty() {
            self.display.window.set_title(title);
        }

        // The newly-active terminal must be resized to the current viewport and redrawn.
        self.display.pending_update.dirty = true;
        self.dirty = true;
    }

    /// Apply layout changes after a tab was created, closed, or switched.
    ///
    /// Relayouts the now-active tab's panes to the current viewport — covering tab switches
    /// where the window size itself did not change, which the regular resize path would miss —
    /// and requests a redraw.
    pub fn sync_tabs(&mut self) {
        let old_is_searching = self.active_tab().focused().search_state.history_index.is_some();
        self.refresh_layout(old_is_searching);
        self.dirty = true;
        if self.display.window.has_frame {
            self.display.window.request_redraw();
        }
    }

    /// Apply pending display updates and relayout the active tab's panes.
    ///
    /// `old_is_searching` reports whether the focused pane was searching before the triggering
    /// events; starting a search scrolls the viewport minimally to keep the origin visible.
    fn refresh_layout(&mut self, old_is_searching: bool) {
        let project_index = self.active_project;
        let tab_index = self.projects[project_index].active_tab;

        // Capture the search-nudge inputs from the focused pane before any resize.
        let (cursor_at_bottom, origin_at_bottom) = {
            let pane = self.projects[project_index].tabs[tab_index].focused_mut();
            let terminal = pane.terminal.lock();
            let num_lines = terminal.screen_lines();
            let cursor_at_bottom = terminal.grid().cursor.point.line + 1 == num_lines;
            let origin_at_bottom = if terminal.mode().contains(TermMode::VI) {
                terminal.vi_mode_cursor.point.line == num_lines - 1
            } else {
                pane.search_state.direction == Direction::Left
            };
            (cursor_at_bottom, origin_at_bottom)
        };

        if self.display.pending_update.dirty {
            self.display.handle_update(&self.message_buffer, &self.config);
        }
        self.relayout_tab(project_index, tab_index);

        // Scroll on search start to make sure origin is visible with minimal viewport motion.
        let pane = self.projects[project_index].tabs[tab_index].focused_mut();
        let new_is_searching = pane.search_state.history_index.is_some();
        if !old_is_searching && new_is_searching {
            let mut terminal = pane.terminal.lock();
            let display_offset = terminal.grid().display_offset();
            if display_offset == 0 && cursor_at_bottom && !origin_at_bottom {
                terminal.scroll_display(Scroll::Delta(1));
            } else if display_offset != 0 && origin_at_bottom {
                terminal.scroll_display(Scroll::Delta(-1));
            }
        }
    }

    /// Recompute every pane rect of the given tab from the display's content area and resize
    /// each pane whose grid dimensions changed (Term, PTY and the pane's stored [`SizeInfo`]).
    fn relayout_tab(&mut self, project_index: usize, tab_index: usize) {
        let content = self.display.content_rect();
        let window_size = self.display.size_info;
        let padding = self.config.window.padding(self.display.window.scale_factor as f32);
        let is_active = project_index == self.active_project
            && tab_index == self.projects[self.active_project].active_tab;

        let tab = &mut self.projects[project_index].tabs[tab_index];
        let focused_id = tab.focused().terminal_id;
        let (rects, dividers) = tab.compute_layout(content);

        debug!(
            "relayout tab {}: content=({}, {}, {}x{}) panes={:?}",
            tab.tab_id,
            content.x,
            content.y,
            content.w,
            content.h,
            rects
                .iter()
                .map(|(id, r)| format!("#{id}:({},{},{}x{})", r.x, r.y, r.w, r.h))
                .collect::<Vec<_>>(),
        );

        let mut focused_size = None;
        for (id, rect) in rects {
            let Some(pane) = tab.root_mut().pane_mut(id) else { continue };
            let mut size_info = SizeInfo::for_pane(&window_size, rect, padding.0, padding.1);
            // An open search bar occupies the pane's bottom row.
            if pane.search_state.history_index.is_some() {
                size_info.reserve_lines(1);
            }

            let mut terminal = pane.terminal.lock();
            if terminal.columns() != size_info.columns()
                || terminal.screen_lines() != size_info.screen_lines()
            {
                pane.notifier.on_resize(size_info.into());
                terminal.resize(size_info);
                pane.search_state.clear_focused_match();
            }
            drop(terminal);

            pane.size_info = size_info;
            if id == focused_id {
                focused_size = Some(size_info);
            }
        }
        tab.dividers = dividers;

        // Keep the damage tracker shaped like the focused pane's grid (the partial-damage path
        // only runs single-pane, where the focused pane is the whole content area). Resize
        // whenever the tracker's dimensions differ from the focused pane — not only when *this*
        // relayout resized the focused terminal. Switching tabs/projects can make a differently
        // sized pane active without touching its (already correctly sized) terminal; gating on a
        // resize this pass would leave the tracker shaped for the previous pane, and the partial
        // damage path would then index past its shorter line array.
        if is_active {
            if let Some(size_info) = focused_size {
                let dimensions = (size_info.screen_lines(), size_info.columns());
                if self.display.damage_tracker.dimensions() != dimensions {
                    self.display.damage_tracker.resize(dimensions.0, dimensions.1);
                }
            }
        }

        self.dirty = true;
    }

    /// Update the terminal window to the latest config.
    pub fn update_config(&mut self, new_config: Rc<UiConfig>) {
        let old_config = mem::replace(&mut self.config, new_config);

        // Apply ipc config if there are overrides.
        self.config = self.window_config.override_config_rc(self.config.clone());

        self.display.update_config(&self.config);
        for project in &self.projects {
            for tab in &project.tabs {
                tab.for_each_pane(&mut |pane| {
                    pane.terminal.lock().set_options(self.config.term_options())
                });
            }
        }

        // Reload cursor if its thickness has changed.
        if (old_config.cursor.thickness() - self.config.cursor.thickness()).abs() > f32::EPSILON {
            self.display.pending_update.set_cursor_dirty();
        }

        if old_config.font != self.config.font {
            let scale_factor = self.display.window.scale_factor as f32;
            // Do not update font size if it has been changed at runtime.
            if self.display.font_size == old_config.font.size().scale(scale_factor) {
                self.display.font_size = self.config.font.size().scale(scale_factor);
            }

            let font = self.config.font.clone().with_size(self.display.font_size);
            self.display.pending_update.set_font(font);
        }

        // Always reload the theme to account for auto-theme switching.
        self.display.window.set_theme(self.config.window.theme());

        // Update display if either padding options or resize increments were changed.
        let window_config = &old_config.window;
        if window_config.padding(1.) != self.config.window.padding(1.)
            || window_config.dynamic_padding != self.config.window.dynamic_padding
            || window_config.resize_increments != self.config.window.resize_increments
        {
            self.display.pending_update.dirty = true;
        }

        // Update title on config reload according to the following table.
        //
        // │cli │ dynamic_title │ current_title == old_config ││ set_title │
        // │ Y  │       _       │              _              ││     N     │
        // │ N  │       Y       │              Y              ││     Y     │
        // │ N  │       Y       │              N              ││     N     │
        // │ N  │       N       │              _              ││     Y     │
        if !self.active_tab().focused().preserve_title
            && (!self.config.window.dynamic_title
                || self.display.window.title() == old_config.window.identity.title)
        {
            self.display.window.set_title(self.config.window.identity.title.clone());
        }

        let opaque = self.config.window_opacity() >= 1.;

        // Disable shadows for transparent windows on macOS.
        #[cfg(target_os = "macos")]
        self.display.window.set_has_shadow(opaque);

        #[cfg(target_os = "macos")]
        self.display.window.set_option_as_alt(self.config.window.option_as_alt());

        // Change opacity and blur state.
        self.display.window.set_transparent(!opaque);
        self.display.window.set_blur(self.config.window.blur);

        // Update hint keys.
        self.display.hint_state.update_alphabet(self.config.hints.alphabet());

        // Update cursor blinking.
        let event = Event::new(TerminalEvent::CursorBlinkingChange.into(), None);
        self.event_queue.push(event.into());

        self.dirty = true;
    }

    /// Get reference to the window's configuration.
    #[cfg(unix)]
    pub fn config(&self) -> &UiConfig {
        &self.config
    }

    /// Clear the window config overrides.
    #[cfg(unix)]
    pub fn reset_window_config(&mut self, config: Rc<UiConfig>) {
        // Clear previous window errors.
        self.message_buffer.remove_target(LOG_TARGET_IPC_CONFIG);

        self.window_config.clear();

        // Reload current config to pull new IPC config.
        self.update_config(config);
    }

    /// Add new window config overrides.
    #[cfg(unix)]
    pub fn add_window_config(&mut self, config: Rc<UiConfig>, options: &ParsedOptions) {
        // Clear previous window errors.
        self.message_buffer.remove_target(LOG_TARGET_IPC_CONFIG);

        self.window_config.extend_from_slice(options);

        // Reload current config to pull new IPC config.
        self.update_config(config);
    }

    /// Draw the window.
    pub fn draw(&mut self, scheduler: &mut Scheduler, proxy: &EventLoopProxy<Event>) {
        self.display.window.requested_redraw = false;

        if self.occluded {
            return;
        }

        self.dirty = false;

        // Force the display to process any pending display update.
        self.display.process_renderer_update();

        // Request immediate re-draw if visual bell animation is not finished yet.
        if !self.display.visual_bell.completed() {
            // We can get an OS redraw which bypasses kairos's frame throttling, thus
            // marking the window as dirty when we don't have frame yet.
            if self.display.window.has_frame {
                self.display.window.request_redraw();
            } else {
                self.dirty = true;
            }
        }

        // Redraw the window using the currently active tab.
        let tab_bar = self.tab_bar_info();
        let project_index = self.active_project;
        let idx = self.projects[project_index].active_tab;

        // Clear with the focused pane's background color (the panes share the theme background;
        // per-pane OSC overrides surface through their own cell backgrounds).
        let background_color = {
            let pane = self.projects[project_index].tabs[idx].focused();
            let terminal = pane.terminal.lock();
            terminal.colors()[NamedColor::Background as usize]
                .map(Rgb)
                .unwrap_or(self.display.colors[NamedColor::Background])
        };

        let project = &mut self.projects[project_index];
        // The project pane only renders for projects with a root folder; passing `None` hides it.
        let pane_data = if project.root.is_some() { Some(&project.pane) } else { None };
        let tab = &mut project.tabs[idx];
        let visible = tab.visible_pane_ids();
        let single_pane = visible.len() == 1 && tab.pane_count() == 1;
        let focused_id = tab.focused().terminal_id;

        self.display.begin_frame(background_color, &self.config, !single_pane);
        for id in visible {
            let Some(pane) = tab.root_mut().pane_mut(id) else { continue };
            let terminal = pane.terminal.lock();
            self.display.draw_pane(
                terminal,
                pane.size_info,
                &mut pane.search_state,
                id == focused_id,
                single_pane,
                &self.config,
            );
        }
        let divider_rects: Vec<_> = tab.dividers.iter().map(|divider| divider.rect).collect();
        self.display.end_frame(
            scheduler,
            &self.message_buffer,
            &self.config,
            &tab_bar,
            pane_data,
            &divider_rects,
        );

        // Dispatch tab-bar actions egui produced this frame through the normal event path (so they
        // reuse the existing create/select/close handling).
        let actions = self.display.take_chrome_actions();
        let window_id = self.display.window.id();
        if actions.create {
            let _ = proxy.send_event(Event::new(EventType::CreateTab, window_id));
        }
        if actions.create_claude {
            let _ = proxy.send_event(Event::new(EventType::CreateClaudeTab, window_id));
        }
        if let Some(id) = actions.select {
            let event = Event::new(EventType::SelectTab(TabSelection::Id(id)), window_id);
            let _ = proxy.send_event(event);
        }
        if let Some(id) = actions.close {
            let _ = proxy.send_event(Event::new(EventType::CloseTab(id), window_id));
        }
        if actions.copy {
            let _ = proxy.send_event(Event::new(EventType::Copy, window_id));
        }
        if actions.paste {
            let _ = proxy.send_event(Event::new(EventType::Paste, window_id));
        }
        if let Some(direction) = actions.split_pane {
            let _ = proxy.send_event(Event::new(EventType::SplitPane(direction), window_id));
        }
        if actions.close_pane {
            let _ = proxy.send_event(Event::new(EventType::ClosePane, window_id));
        }
        if actions.toggle_pane_zoom {
            let _ = proxy.send_event(Event::new(EventType::TogglePaneZoom, window_id));
        }
        if actions.focus_next_pane {
            let _ = proxy.send_event(Event::new(EventType::FocusNextPane, window_id));
        }
        if let Some(index) = actions.select_project {
            let _ = proxy.send_event(Event::new(EventType::SelectProject(index), window_id));
        }
        if let Some(index) = actions.close_project {
            let _ = proxy.send_event(Event::new(EventType::CloseProject(index), window_id));
        }
        if let Some(idx) = actions.open_claude_session {
            // Resolve the index against the same cache the frame was rendered from.
            if let Some(session) = self.active_project().sessions.get(idx) {
                let project_index = self.active_project;
                let session_id = session.id.clone();
                let event = Event::new(
                    EventType::OpenClaudeSession { project_index, session_id },
                    window_id,
                );
                let _ = proxy.send_event(event);
            }
        }
        if actions.create_project {
            // We're on the main thread inside the winit handler, so a blocking native folder
            // dialog is fine (it's modal). Dispatch the result through the event path.
            if let Some(dir) = rfd::FileDialog::new()
                .set_title("选择项目文件夹")
                .set_parent(self.display.window.winit_window())
                .pick_folder()
            {
                let _ = proxy.send_event(Event::new(EventType::CreateProject(dir), window_id));
            }
        }

        if actions.toggle_sidebar {
            self.display.toggle_sidebar();
            self.dirty = true;
        }
        if actions.toggle_pane {
            // Refresh on reveal so the pane never shows stale git data.
            if self.display.toggle_project_pane() {
                self.refresh_pane_git();
            }
            self.dirty = true;
        }
        if let Some(path) = actions.pane_toggle_dir {
            self.projects[self.active_project].pane.toggle_dir(&path);
            self.display.pending_update.dirty = true;
            self.dirty = true;
        }
        if let Some(path) = actions.pane_toggle_diff {
            self.toggle_pane_change(&path);
        }
        if let Some(path) = actions.pane_preview {
            self.toggle_pane_preview(&path);
        }
        if let Some(split) = actions.viewer_layout {
            self.display.set_viewer_layout(split);
            self.ensure_diff_data(false);
            self.dirty = true;
        }
        if actions.viewer_toggle_difft {
            // Re-enabling difft retries the tool even after a recorded failure.
            if self.display.toggle_viewer_difft() {
                self.projects[self.active_project].pane.clear_difft_missing();
            }
            self.ensure_diff_data(false);
            self.dirty = true;
        }
        if actions.viewer_toggle_md_source {
            self.display.toggle_md_source();
            if let Some(path) = self.display.previewed_file() {
                self.reload_preview(&path);
            }
            self.dirty = true;
        }
        if actions.pane_refresh {
            self.refresh_pane_dir();
        }
        if actions.open_settings {
            let family = self.config.font.normal().family.clone();
            let size_pt = self.config.font.size().as_pt();
            let program = self.config.terminal.shell.as_ref().map(|p| p.program().to_owned());
            let state = SettingsState::build(&family, size_pt, program.as_deref());
            self.display.open_settings_panel(state);
            self.dirty = true;
        }
        if let Some(state) = actions.settings {
            self.apply_settings(&state);
        }

        // The chrome's reserved size changed; recompute the terminal layout so it fits beside it.
        if actions.layout_changed {
            self.sync_tabs();
        }

        // Keep a low-frequency git refresh ticking while the pane is shown for a rooted project,
        // so external changes (commits, edits from other tools) appear without interaction.
        let timer_id = TimerId::new(Topic::PaneGitRefresh, window_id);
        let want_timer = self.display.pane_visible() && self.active_project().root.is_some();
        if want_timer && !scheduler.scheduled(timer_id) {
            scheduler.schedule(
                Event::new(EventType::PaneGitRefresh, window_id),
                Duration::from_secs(5),
                true,
                timer_id,
            );
        } else if !want_timer && scheduler.scheduled(timer_id) {
            scheduler.unschedule(timer_id);
        }
    }

    /// Apply a settings-panel snapshot: update the font live (affecting all tabs), switch the shell
    /// for tabs opened afterwards, and persist all three values to the on-disk config.
    fn apply_settings(&mut self, state: &SettingsState) {
        let family = state.family().to_owned();

        // `display.font_size` is in pixels (points × scale factor); the config stores points.
        let scale = self.display.window.scale_factor as f32;
        self.display.font_size = FontSize::new(state.size_pt).scale(scale);
        let render_font =
            self.config.font.clone().with_family(family.clone()).with_size(self.display.font_size);
        self.display.pending_update.set_font(render_font);

        // Keep the config as the source of truth (point size) for reloads and font-size reset.
        let config = Rc::make_mut(&mut self.config);
        config.font =
            config.font.clone().with_family(family.clone()).with_size(FontSize::new(state.size_pt));

        // Shell affects only tabs spawned after this point.
        let shell = state.shell().cloned();
        if let Some(shell) = &shell {
            config.terminal.shell = Some(program_from_preset(shell));
        }
        self.dirty = true;

        // Persist (best effort): a failure only means the change won't survive a restart.
        let choice = shell.as_ref().map(|s| ShellChoice { program: &s.program, args: &s.args });
        if let Err(err) =
            persist::write_settings(&self.config, &family, state.size_pt, choice.as_ref())
        {
            error!("Failed to persist settings to config file: {err}");
        }
    }

    /// Copy the focused pane's selection to the system clipboard.
    pub fn copy_active_selection(&self, clipboard: &mut Clipboard) {
        // A focused viewer tab (file preview / diff) owns the selection; prefer its text.
        if let Some(text) = self.display.viewer_selection_text().filter(|s| !s.is_empty()) {
            clipboard.store(ClipboardType::Clipboard, text);
            return;
        }
        if let Some(text) = self
            .active_tab()
            .focused()
            .terminal
            .lock()
            .selection_to_string()
            .filter(|s| !s.is_empty())
        {
            clipboard.store(ClipboardType::Clipboard, text);
        }
    }

    /// Paste `text` into the focused pane, using bracketed paste when the terminal requests it.
    pub fn paste_to_active(&mut self, text: &str) {
        let project_index = self.active_project;
        let tab_index = self.projects[project_index].active_tab;
        let pane = self.projects[project_index].tabs[tab_index].focused_mut();
        let bracketed = pane.terminal.lock().mode().contains(TermMode::BRACKETED_PASTE);
        if bracketed {
            pane.notifier.notify(b"\x1b[200~"[..].to_vec());
            // Filter escapes that could break out of bracketed paste, and normalize newlines.
            let payload: String = text
                .chars()
                .filter(|c| *c != '\x1b' && *c != '\x03')
                .collect::<String>()
                .replace("\r\n", "\r")
                .replace('\n', "\r");
            pane.notifier.notify(payload.into_bytes());
            pane.notifier.notify(b"\x1b[201~"[..].to_vec());
        } else {
            let payload = text.replace("\r\n", "\r").replace('\n', "\r");
            pane.notifier.notify(payload.into_bytes());
        }
    }

    /// Snapshot of the chrome contents for rendering (project list + active project's tabs).
    fn tab_bar_info(&self) -> TabBarInfo {
        let project = self.active_project();
        // Prefer the focused pane's live working directory (where resolvable), falling back to
        // the project root, so the status bar tracks where the user actually is.
        let cwd = self
            .pane_working_directory(self.active_tab().focused())
            .or_else(|| project.root.clone());
        TabBarInfo {
            titles: project.tabs.iter().map(|tab| tab.title().to_owned()).collect(),
            ids: project.tabs.iter().map(|tab| tab.tab_id).collect(),
            active: project.active_tab,
            project_names: self.projects.iter().map(|p| p.name.clone()).collect(),
            active_project: self.active_project,
            project_sessions: project
                .sessions
                .iter()
                .map(|s| ClaudeSessionRow { label: s.label.clone() })
                .collect(),
            git_info: cwd.as_deref().and_then(git_branch_line),
        }
    }

    /// Process events for this terminal window.
    pub fn handle_event(
        &mut self,
        #[cfg(target_os = "macos")] event_loop: &ActiveEventLoop,
        event_proxy: &EventLoopProxy<Event>,
        clipboard: &mut Clipboard,
        scheduler: &mut Scheduler,
        event: WinitEvent<Event>,
    ) {
        match event {
            WinitEvent::AboutToWait
            | WinitEvent::WindowEvent { event: WindowEvent::RedrawRequested, .. } => {
                // Skip further event handling with no staged updates.
                if self.event_queue.is_empty() {
                    return;
                }

                // Continue to process all pending events.
            },
            event => {
                self.event_queue.push(event);
                return;
            },
        }

        // Remember whether the focused pane was searching before processing events.
        let old_is_searching = self.active_tab().focused().search_state.history_index.is_some();

        // Process each staged event against its target pane. PTY-generated events are routed to
        // the originating pane (which may be in a background tab); everything else targets the
        // active tab's focused pane.
        let events: Vec<_> = self.event_queue.drain(..).collect();
        for queued in events {
            // Offer window events to the native chrome first. If it consumes one (e.g. a click on
            // the tab bar), it must not also reach the terminal.
            if let WinitEvent::WindowEvent { event: window_event, .. } = &queued {
                if self.display.handle_chrome_event(window_event) {
                    self.dirty = true;
                    continue;
                }

                // Then the pane layer: divider hover/drag, click-to-focus and the mouse grab.
                if self.handle_pane_mouse_event(window_event) {
                    self.dirty = true;
                    continue;
                }
            }

            // Events from an already-closed pane have no valid target; drop them.
            let Some(target) = self.event_target_pane(&queued) else { continue };
            self.process_event(
                target,
                #[cfg(target_os = "macos")]
                event_loop,
                event_proxy,
                clipboard,
                scheduler,
                queued,
            );
        }

        // Post-processing (display updates, hint highlighting, redraw) operates on the focused
        // pane together with the shared display.
        if self.display.pending_update.dirty {
            self.refresh_layout(old_is_searching);
            self.dirty = true;
        }

        if self.dirty || self.mouse.hint_highlight_dirty {
            // Mouse-hover hints follow the pane under the pointer, not the focused one.
            let project_index = self.active_project;
            let idx = self.projects[project_index].active_tab;
            let tab = &mut self.projects[project_index].tabs[idx];
            let hover_id = self
                .hovered_pane
                .filter(|id| tab.contains(*id))
                .unwrap_or_else(|| tab.focused().terminal_id);
            let pane = tab.pane_mut(hover_id).expect("hovered pane");
            let size_info = pane.size_info;
            let terminal = pane.terminal.lock();
            self.dirty |= self.display.update_highlighted_hints(
                &terminal,
                &self.config,
                &self.mouse,
                self.modifiers.state(),
                &size_info,
            );
            self.mouse.hint_highlight_dirty = false;
        }

        // Don't call `request_redraw` when event is `RedrawRequested` since the `dirty` flag
        // represents the current frame, but redraw is for the next frame.
        if self.dirty
            && self.display.window.has_frame
            && !self.occluded
            && !matches!(event, WinitEvent::WindowEvent { event: WindowEvent::RedrawRequested, .. })
        {
            self.display.window.request_redraw();
        }
    }

    /// Determine which `(project, tab, pane)` a staged event should be dispatched to.
    ///
    /// PTY-generated events carry the id of their originating terminal so they reach the right
    /// pane even when its tab or project is not active. All other events target the active
    /// project's active tab's focused pane.
    ///
    /// Returns `None` when the originating pane no longer exists: a closed pane's PTY can still
    /// have events (e.g. its final `Title`) queued, and falling back to the focused pane would
    /// apply them to an unrelated terminal — renaming someone else's tab. Such events are
    /// dropped instead.
    fn event_target_pane(&self, event: &WinitEvent<Event>) -> Option<(usize, usize, u64)> {
        if let WinitEvent::UserEvent(user_event) = event {
            if let Some(terminal_id) = user_event.terminal_id() {
                for (pi, project) in self.projects.iter().enumerate() {
                    if let Some(ti) = project.tabs.iter().position(|t| t.contains(terminal_id)) {
                        return Some((pi, ti, terminal_id));
                    }
                }
                return None;
            }
        }

        let project_index = self.active_project;
        let tab_index = self.active_project().active_tab;
        let tab = self.active_tab();

        // Mouse events target the pane under the pointer (sticking to the press-grabbed pane
        // mid-drag); keyboard and everything else targets the focused pane.
        if let WinitEvent::WindowEvent { event: window_event, .. } = event {
            let under_cursor = || match window_event {
                WindowEvent::CursorMoved { position, .. } => {
                    self.pane_at_point(position.x as f32, position.y as f32)
                },
                WindowEvent::MouseInput { .. }
                | WindowEvent::MouseWheel { .. }
                | WindowEvent::Touch(_) => {
                    self.pane_at_point(self.mouse.x as f32, self.mouse.y as f32)
                },
                _ => None,
            };
            if matches!(
                window_event,
                WindowEvent::CursorMoved { .. }
                    | WindowEvent::MouseInput { .. }
                    | WindowEvent::MouseWheel { .. }
                    | WindowEvent::Touch(_)
            ) {
                let id = self
                    .mouse_grab_pane
                    .filter(|id| tab.contains(*id))
                    .or_else(under_cursor)
                    .unwrap_or_else(|| tab.focused().terminal_id);
                return Some((project_index, tab_index, id));
            }
        }

        Some((project_index, tab_index, tab.focused().terminal_id))
    }

    /// Process a single event against the pane at `(project_index, tab_index, pane_id)`.
    fn process_event(
        &mut self,
        target: (usize, usize, u64),
        #[cfg(target_os = "macos")] event_loop: &ActiveEventLoop,
        event_proxy: &EventLoopProxy<Event>,
        clipboard: &mut Clipboard,
        scheduler: &mut Scheduler,
        event: WinitEvent<Event>,
    ) {
        let (project_index, tab_index, pane_id) = target;
        let is_active_tab = project_index == self.active_project
            && tab_index == self.projects[self.active_project].active_tab;
        let tab = &mut self.projects[project_index].tabs[tab_index];
        let is_focused_pane = is_active_tab && tab.focused().terminal_id == pane_id;
        let tab_id = tab.tab_id;
        let Some(pane) = tab.pane_mut(pane_id) else { return };
        let size_info = pane.size_info;
        let mut terminal = pane.terminal.lock();

        let context = ActionContext {
            cursor_blink_timed_out: &mut self.cursor_blink_timed_out,
            prev_bell_cmd: &mut self.prev_bell_cmd,
            message_buffer: &mut self.message_buffer,
            inline_search_state: &mut pane.inline_search_state,
            search_state: &mut pane.search_state,
            modifiers: &mut self.modifiers,
            notifier: &mut pane.notifier,
            display: &mut self.display,
            mouse: &mut self.mouse,
            touch: &mut self.touch,
            dirty: &mut self.dirty,
            occluded: &mut self.occluded,
            terminal: &mut terminal,
            #[cfg(not(windows))]
            master_fd: pane.master_fd,
            #[cfg(not(windows))]
            shell_pid: pane.shell_pid,
            preserve_title: pane.preserve_title,
            tab_title: &mut pane.title,
            is_active_tab: is_focused_pane,
            size_info,
            tab_id,
            config: &self.config,
            event_proxy,
            #[cfg(target_os = "macos")]
            event_loop,
            clipboard,
            scheduler,
        };
        let mut processor = input::Processor::new(context);
        processor.handle_event(event);
    }

    /// ID of this terminal context.
    pub fn id(&self) -> WindowId {
        self.display.window.id()
    }

    /// Write the ref test results to the disk.
    pub fn write_ref_test_results(&self) {
        // Dump grid state.
        let mut grid = self.active_tab().focused().terminal.lock().grid().clone();
        grid.initialize_all();
        grid.truncate();

        let serialized_grid = json::to_string(&grid).expect("serialize grid");

        let size_info = &self.active_tab().focused().size_info;
        let size = TermSize::new(size_info.columns(), size_info.screen_lines());
        let serialized_size = json::to_string(&size).expect("serialize size");

        let serialized_config = format!("{{\"history_size\":{}}}", grid.history_size());

        File::create("./grid.json")
            .and_then(|mut f| f.write_all(serialized_grid.as_bytes()))
            .expect("write grid.json");

        File::create("./size.json")
            .and_then(|mut f| f.write_all(serialized_size.as_bytes()))
            .expect("write size.json");

        File::create("./config.json")
            .and_then(|mut f| f.write_all(serialized_config.as_bytes()))
            .expect("write config.json");
    }

}
