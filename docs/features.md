# Features

This document gives an overview over Kairos's features beyond its terminal
emulation capabilities. To get a list with supported control sequences take a
look at [Kairos's escape sequence support](./escape_support.md).

## Vi Mode

The vi mode allows moving around Kairos's viewport and scrollback using the
keyboard. It also serves as a jump-off point for other features like search and
opening URLs with the keyboard. By default you can launch it using
<kbd>Ctrl</kbd> <kbd>Shift</kbd> <kbd>Space</kbd>.

### Motion

The cursor motions are setup by default to mimic vi, however they are fully
configurable. If you don't like vi's bindings, take a look at the configuration
file to change the various movements.

### Selection

One useful feature of vi mode is the ability to make selections and copy text to
the clipboard. By default you can start a selection using <kbd>v</kbd> and copy
it using <kbd>y</kbd>. All selection modes that are available with the mouse can
be accessed from vi mode, including the semantic (<kbd>Alt</kbd> <kbd>v</kbd>),
line (<kbd>Shift</kbd> <kbd>v</kbd>) and block selection (<kbd>Ctrl</kbd>
<kbd>v</kbd>). You can also toggle between them while the selection is still
active.

## Search

Search allows you to find anything in Kairos's scrollback buffer. You can
search forward using <kbd>Ctrl</kbd> <kbd>Shift</kbd> <kbd>f</kbd> (<kbd>Command</kbd> <kbd>f</kbd> on macOS) and
backward using <kbd>Ctrl</kbd> <kbd>Shift</kbd> <kbd>b</kbd> (<kbd>Command</kbd> <kbd>b</kbd> on macOS).

### Vi Search

In vi mode the search is bound to <kbd>/</kbd> for forward and <kbd>?</kbd> for
backward search. This allows you to move around quickly and help with selecting
content. The `SearchStart` and `SearchEnd` keybinding actions can be bound if
you're looking for a way to jump to the start or the end of a match.

### Normal Search

During normal search you don't have the opportunity to move around freely, but
you can still jump between matches using <kbd>Enter</kbd> and <kbd>Shift</kbd>
<kbd>Enter</kbd>. After leaving search with <kbd>Escape</kbd> your active match
stays selected, allowing you to easily copy it.

## Hints

Terminal hints allow easily interacting with visible text without having to
start vi mode. They consist of a regex that detects these text elements and then
either feeds them to an external application or triggers one of Kairos's
built-in actions.

Hints can also be triggered using the mouse or vi mode cursor. If a hint is
enabled for mouse interaction and recognized as such, it will be underlined when
the mouse or vi mode cursor is on top of it. Using the left mouse button or
<kbd>Enter</kbd> key in vi mode will then trigger the hint.

Hints can be configured in the `hints` and `colors.hints` sections in the
Kairos configuration file.

## Selection expansion

After making a selection, you can use the right mouse button to expand it.
Double-clicking will expand the selection semantically, while triple-clicking
will perform line selection. If you hold <kbd>Ctrl</kbd> while expanding the
selection, it will switch to the block selection mode.

## Opening URLs with the mouse

You can open URLs with your mouse by clicking on them. The modifiers required to
be held and program which should open the URL can be setup in the configuration
file. If an application captures your mouse clicks, which is indicated by a
change in mouse cursor shape, you're required to hold <kbd>Shift</kbd> to bypass
that.

## Multi-Window

Kairos supports running multiple terminal emulators from the same Kairos
instance. New windows can be created either by using the `CreateNewWindow`
keybinding action, or by executing the `kairos msg create-window` subcommand.

## Split Panes

Every tab can host multiple terminals at once, arranged as arbitrarily nested
horizontal and vertical splits (tmux-style). Splits, the pane layout and each
pane's working directory are part of session persistence, so they are restored
on the next launch.

Default bindings on Windows/Linux (macOS in parentheses):

- Split right: <kbd>Ctrl</kbd> <kbd>Shift</kbd> <kbd>d</kbd>
  (<kbd>Command</kbd> <kbd>d</kbd>)
- Split down: <kbd>Alt</kbd> <kbd>Shift</kbd> <kbd>d</kbd>
  (<kbd>Command</kbd> <kbd>Shift</kbd> <kbd>d</kbd>)
- Close pane (the tab when it is the last pane): <kbd>Ctrl</kbd>
  <kbd>Shift</kbd> <kbd>w</kbd> (<kbd>Command</kbd> <kbd>Shift</kbd>
  <kbd>w</kbd>)
- Move focus between panes: <kbd>Alt</kbd> <kbd>Arrows</kbd>
  (<kbd>Command</kbd> <kbd>Option</kbd> <kbd>Arrows</kbd>)
- Resize the focused pane one cell at a time: <kbd>Alt</kbd> <kbd>Shift</kbd>
  <kbd>Arrows</kbd> (<kbd>Command</kbd> <kbd>Ctrl</kbd> <kbd>Arrows</kbd>)
- Maximize/restore the focused pane: <kbd>Ctrl</kbd> <kbd>Shift</kbd>
  <kbd>Enter</kbd> (<kbd>Command</kbd> <kbd>Shift</kbd> <kbd>Enter</kbd>)

Panes can also be focused by clicking into them and resized by dragging the
divider between them. The command palette (<kbd>Ctrl</kbd> <kbd>Shift</kbd>
<kbd>p</kbd>) offers the same operations: 向右分屏, 向下分屏, 关闭当前 Pane,
最大化/还原 Pane and 焦点切换到下一个 Pane.

Note that the default <kbd>Alt</kbd> <kbd>Arrows</kbd> and <kbd>Alt</kbd>
<kbd>Shift</kbd> <kbd>Arrows</kbd> bindings shadow the escape sequences some
terminal applications use for word jumps; rebind the `FocusPane*` /
`ResizePane*` actions if you rely on those.
