# Architecture

The application uses pure state, terminal runtime, and GPUI modules:

```text
main → ui → terminal → core
         └───────────→ core
```

## Core state

`core/workspace.rs` owns sessions, windows/tabs, pane identities and binary split
trees. Every visible control and shortcut dispatches the same typed Command.
IDs are opaque, monotonically allocated, and never reused. Session and window
fields are private, so views cannot directly invalidate focus or layout.

A session always has an active window, and a window always has an active pane.
New panes/tabs receive focus. Closing a split leaf promotes its sibling. Closing
a window's last pane closes that window; closing a session's last window closes
the session. An empty workspace is valid. Tab switching preserves each tab's
layout and focused pane. `pane_ids()` includes hidden tabs and sessions so runtime
reconciliation never destroys their terminals.

`core/grid.rs` validates nonzero dimensions before FFI. `core/input.rs` carries
platform-neutral input. `core/frame.rs` contains owned cells, grapheme clusters,
colors, styles and cursor positions; frames can cross threads. Adjacent equal-style
cells are batched into positioned text runs.

## Terminal runtime

`terminal/ghostty.rs` is the sole Ghostty FFI adapter. It owns terminal state,
reusable render iterators, key encoding and PTY query responses. Ghostty objects
stay on the UI owner thread; never implement unsafe Send/Sync for them.

`terminal/pty.rs` uses portable-pty. Blocking reads and writes run on workers
with bounded queues, while the reaper waits for child exit separately. Process
groups must be verified as belonging to the spawned PTY session before cleanup;
never signal the application's own group. Drop requests cleanup without joining
workers on the UI thread.

`terminal/workspace.rs` couples pane IDs to Ghostty state, optional PTYs and
cached Arc frames. `new` creates a headless workspace for VT tests; `live` adds
shells. A candidate workspace and new resources are prepared before committing
layout changes, so spawn failures preserve existing terminals. Polling feeds
output into Ghostty, snapshots changed terminals and queues device responses.
Exit publication waits for output EOF so final output is not lost.

The desktop currently owns this runtime directly. The next major boundary is a
server process with bounded commands/snapshots. Client disconnect must be distinct
from closing a pane. See [parity.md](parity.md) before implementing persistence.

## GPUI

`ui/theme.rs` supplies semantic colors and defaults. `ui/components` contains
buttons, session rows, headers, text fields, empty/status states and terminal
surfaces. Components do not manage processes or mutate core state.

`ui/terminal_view.rs` owns focus, measured font metrics, viewport bounds, selection
and text-input composition. It emits owned events to `ui/workspace.rs`. Resize
notifications are deferred and deduplicated. The workspace owns subscriptions,
reconciles views with runtime pane IDs and polls bounded PTY output. A terminal
keeps its entity and process when its session or tab is hidden.

The root provides Workspace actions; rename dialogs switch to a modal context
so underlying shortcuts cannot operate on terminals. Dialogs restore terminal
focus when submitted or cancelled. Unicode fields use UTF-16 platform ranges
and grapheme-aware editing.

## Extension rules

- Change core invariants and tests before wiring new workspace controls.
- Keep transport and process cleanup out of rendering and text components.
- Preserve ownership, cached state and focus across tab/session switching.
- Report recoverable errors with context; do not silently replace failed shells.
- Keep Cargo.lock and dependency/toolchain pins coordinated by the integration owner.
- Use disjoint agent write sets and run `mise run verify` after integration.
