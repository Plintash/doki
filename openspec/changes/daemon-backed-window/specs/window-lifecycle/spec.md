# Spec Delta

## Purpose

Lets the desktop window be disposable: closing it is a native close that keeps
the app running, activating the app rebuilds a window from daemon state and a
small desktop snapshot, and no durable work or unsaved edit is lost in between.

## ADDED Requirements

### Requirement: Window close is a native close

On macOS, with no right-panel tab on screen, Cmd-W and the window's close
control SHALL close the window through AppKit's standard close path, and the
application MUST remain running in the Dock. While the right panel is visible
with a tab open, Cmd-W closes that tab first and leaves the window open; a
hidden panel keeps its tabs but does not capture Cmd-W. Closing the last window
on other platforms keeps quitting the app.

#### Scenario: Close while fullscreen

- **WHEN** the user presses Cmd-W with no right-panel tab open while the window
  is in native fullscreen
- **THEN** the window closes without leaving an empty black fullscreen Space

#### Scenario: Close a right-panel tab first

- **WHEN** the user presses Cmd-W while the right panel is visible with a tab
  open
- **THEN** that tab closes and the window stays open

#### Scenario: A hidden panel does not capture Cmd-W

- **WHEN** the user presses Cmd-W while the right panel is hidden and it still
  holds a tab
- **THEN** the window closes and the hidden tab is left alone

#### Scenario: Close while windowed

- **WHEN** the user closes the only window
- **THEN** the window disappears and the app stays reachable in the Dock

### Requirement: Activating the app rebuilds or focuses the window

Activation with no window open SHALL build a fresh main window. Activation with
a window open SHALL bring that window forward without rebuilding it.

#### Scenario: Dock activation after close

- **WHEN** the app has no window and the user clicks the Dock icon
- **THEN** a new main window opens and hydrates from daemon state

#### Scenario: Dock activation with a window open

- **WHEN** the app already has a window and the user clicks the Dock icon
- **THEN** the existing window is activated and no second window is created

#### Scenario: Notification activation

- **WHEN** the user clicks a task notification while no window is open
- **THEN** a window opens with that task selected

### Requirement: Durable work survives window close

Closing the window MUST NOT stop an in-flight agent turn, lose transcript or
draft state, or terminate an accepted terminal process.

#### Scenario: Close during streaming

- **WHEN** the user closes the window while an agent turn is streaming
- **THEN** the turn continues in the daemon, and reopening shows the completed
  transcript

#### Scenario: Close with a running terminal

- **WHEN** the user closes the window while a terminal is running a process
- **THEN** the process keeps running and reopening reattaches to it

### Requirement: A rebuild restores the desktop snapshot

A rebuilt window SHALL restore the last window frame and display, theme,
language and font-size preferences, sidebar and right-panel visibility and
widths, and the last selected project and task. Per-task right-panel surface
descriptors (terminal identities, browser tabs by URL, file and diff paths)
MAY be restored.

#### Scenario: Reopen returns to the previous task and layout

- **WHEN** the user closes a window showing a selected task with the sidebar
  hidden and the right panel showing a terminal
- **THEN** the rebuilt window shows the same task, sidebar visibility and panel
  width, and reattaches the terminal surface

### Requirement: In-page state is not restored

A rebuild MUST NOT restore browser page content, form values, in-page scroll or
navigation history, editor cursor, scroll or selection, file-tree expansion,
transcript scroll position, transient overlays, or animation state. Browser
tabs themselves are restored by URL, and a tab whose URL was never observed
comes back blank.

#### Scenario: Scroll position resets

- **WHEN** the user scrolls the transcript, closes the window, and reopens it
- **THEN** the transcript opens at its default position

#### Scenario: Browser tabs return by URL

- **WHEN** the user closes a window that had a browser tab open on a page
- **THEN** the rebuilt right panel contains a browser tab navigated to that URL,
  without any guarantee for form values or in-page scroll

### Requirement: The standard full screen command works from any surface

On macOS the app SHALL own the system's full screen command: a Window-menu item
carrying the ⌃⌘F equivalent, so ⌃⌘F and the system's Globe-F shortcut both
toggle full screen from any focused surface and survive a menu bar rebuilt in
another language. Windows and Linux SHALL bind F11.

#### Scenario: Globe-F toggles full screen

- **WHEN** the user presses Globe-F while any surface has focus
- **THEN** the window enters full screen, and pressing it again leaves

#### Scenario: The command survives a language change

- **WHEN** the menu bar is rebuilt because the language changed
- **THEN** the Window menu still carries the full screen item and its shortcut

### Requirement: Unsaved edits block close

Closing the window while any file editor holds unsaved changes SHALL present a
confirmation before the window closes. Cancelling MUST keep the window and the
unsaved buffers intact.

#### Scenario: Cancel a close with dirty editors

- **WHEN** the user closes the window while an editor is dirty
- **THEN** a confirmation appears, and choosing cancel leaves the window and
  the edited contents exactly as they were

#### Scenario: Close with clean editors

- **WHEN** the user closes the window with no unsaved edits
- **THEN** the window closes without a confirmation
