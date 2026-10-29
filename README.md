# Rex-rs

A native Linux terminal workspace in Rust, with GPUI for the interface and
[libghostty-vt](https://github.com/Uzaaft/libghostty-rs) for terminal emulation.
The desktop connects to the existing Rex server using its native control and
terminal streaming protocols. Rex owns sessions, processes and persistence.

## Development

```sh
mise trust
mise install
mise run verify
mise run dev
```

Rust and Zig are pinned in `mise.toml`; GPUI and the Ghostty wrapper are pinned
in `Cargo.toml`. Keep Cargo.lock in version control. The Ghostty wrapper is pinned
to commit `9ede35b8ea94a55ed3608408d67e102883129f1e` because Rex terminal streams
begin with binary snapshots that the released wrapper cannot restore. Its native
Ghostty revision requires Zig 0.17.0. The first build downloads native sources and
Zig dependencies. Cargo builds Ghostty in ReleaseSafe even in Rust debug builds.

The lockfile keeps libc at 0.2.186 because GPUI's transitive xattr 0.2.3 still
references the Linux ENOATTR alias removed by later libc releases. Recheck this
compatibility when updating GPUI or its dependencies.

GPUI also needs a graphics session, GPU drivers and Linux development libraries
for its selected platform backends. mise manages language toolchains; install
missing system development libraries through the distribution's package manager.

| Task | Purpose |
| --- | --- |
| `mise run verify` | Run the full integration gate: formatting, compile, Clippy, tests and smoke checks |
| `mise run dev` | Open the workspace shell |
| `mise run test:core` | Test pure state and frame primitives without external crates |
| `mise run check` | Check formatting and compile all targets |
| `mise run test` | Run unit tests plus Ghostty, input-protocol, PTY, server and CLI checks |
| `mise run smoke` | Exercise Ghostty without a graphics session |
| `mise run lint` | Run Clippy with warnings denied |

`test:core` uses rustc directly so it remains usable without downloading GUI or
FFI dependencies. It runs the same core unit tests included by `cargo test`.

Clippy's `all` and `pedantic` groups are enabled, with additional checks for
unwraps, expects, panic/debug macros and unfinished implementations. The lint
task treats warnings as errors. Keep allowances narrow and explain why they
are needed; `mise run verify` is the required check before integration.

## Foundation

```text
src/
  core/              Session/window/pane IDs, split trees, commands, validated grid sizes,
                     owned terminal frames and text-run batching
  terminal/          Ghostty adapter, bounded PTY workers, process lifetimes, cached frames
  rex/               Official Rex control connection, authoritative workspace views,
                     snapshot/VT stream decoding and nonblocking desktop cache
  server/            Legacy local backend retained for development regression checks
  ui/
    actions.rs       Keyboard actions and bindings
    theme.rs         Semantic colors, dimensions and terminal metrics
    components/      Buttons with tooltips, session rows, pane headers, empty/status
                     states, modals, drawers, segmented controls, reusable Unicode
                     text fields and a styled terminal surface
    terminal_view.rs Measured viewport, keyboard/IME input, selection and scrolling
    workspace.rs     Sessions, tabs, rename dialogs, focus and command dispatch
  main.rs            Application and window bootstrap
  bin/               Ghostty, input-protocol and real-PTY smoke executables
```

See [architecture](docs/architecture.md) for ownership, mutation boundaries,
close/focus behavior, and guidelines for extending the scaffold.
See [AGENTS.md](AGENTS.md) for parallel workstreams and integration responsibilities.

## Workspace controls

Sessions contain tabs, and each tab contains a split layout of terminal panes.
Rex creates the shells and retains them while clients detach. The desktop does
not create a session, launch a shell or claim another client's blocks on attachment.

| Shortcut | Action |
| --- | --- |
| Ctrl+Shift+N | New session |
| Ctrl+Shift+T | New tab |
| Ctrl+PageUp / PageDown | Previous / next tab |
| Ctrl+Shift+D / E | Split right / below |
| Ctrl+Tab | Focus the next pane |
| Ctrl+Alt+Arrow | Focus the neighboring pane in that direction |
| Ctrl+Shift+Z | Zoom / unzoom the focused pane |
| Ctrl+Shift+C / V | Copy selection / paste |
| Ctrl+Shift+F | Find in terminal history |
| Ctrl+Shift+R | Rename session |
| Ctrl+Alt+Shift+R | Rename tab |
| Ctrl+Shift+W | Close focused pane |
| Ctrl+Alt+Shift+W | Close selected tab |
| Ctrl+Alt+W | Close selected session |
| Ctrl+Shift+Q | Quit |

Click a session or tab to select it, click a terminal to focus it, and drag to
select text. Drag a split divider to resize it. Zoom and split ratios are shared
with other clients and automation. Scroll to browse history. Removing a tab's last
placed pane closes that tab. Detached blocks remain in their session until closed,
even when the session has no tabs. Rename dialogs support
Unicode text, Enter to save and Escape to cancel. Keyboard shortcuts for the
workspace are suspended while a rename dialog is open.

Use **Detach** in a pane header to remove it from its tab while its program keeps
running. Open **Detached blocks** from the session's sidebar badge to view its
inventory. **Open in new tab** restores the existing block; **Place beside…** uses
the Arrange chooser when the session has a tiled destination. **Close block**
terminates the block instead of detaching it. The browser refreshes on opening,
offers **Refresh**, and remains available in sessions without tabs.

## Connecting to Rex

Start or install the official Rex server separately. The desktop never launches
a replacement server when connection fails:

```sh
rex server status
mise run dev
# Or select an existing local server explicitly:
mise exec -- cargo run --locked --bin rex-rs -- --endpoint unix:///path/to/server.sock
```

Endpoint precedence is `--endpoint`, `REX_RS_ENDPOINT`, `REX_RS_REX_ENDPOINT`,
`REX_SERVER`, then `$XDG_RUNTIME_DIR/rex/server.sock` (or
`/run/user/<uid>/rex/server.sock` when XDG runtime is unset). Only local Unix
endpoints are supported. The implemented control API is revision 7; incompatible
servers produce an explicit error. The server serves its exact protocol at
`GET /llms.txt` over the same endpoint.

### Client automation

The desktop advertises 35 native actions through Rex, including session/tab/pane
commands, pane movement/swapping, tab reordering, font size, sidebar visibility,
copy/paste, terminal history search, key modes, detached blocks and configuration reload.
Use the desktop's client ID from `rex client ls`:

```sh
rex -C CLIENT actions
rex -C CLIENT do client.font.increase
rex -C CLIENT do client.sidebar.toggle
rex -C CLIENT do pane.split direction=left
rex -C CLIENT do client.tab.goto index=-1
rex -C CLIENT do pane.arrange
rex -C CLIENT do pane.move anchor_block_id=BLOCK direction=above ratio=0.3
rex -C CLIENT do pane.swap other_block_id=BLOCK
rex -C CLIENT do client.tab.move before_window_id=WINDOW
rex -C CLIENT do pane.detach
rex -C CLIENT do client.detached.toggle
rex -C CLIENT do pane.restore block_id=BLOCK
rex -C CLIENT do client.find.toggle
rex -C CLIENT do client.find.next
rex -C CLIENT do client.find.previous
```

Callbacks validate session/block targets, run in order and reply after the action
finishes. Layout actions use the same core command translator as buttons and
shortcuts, then fetch authoritative views. UI actions run on their owning window;
they preserve an open rename dialog's focus. Disconnected actions are not replayed.

Use **Arrange** in a pane header to move it left/right/above/below a pane in the
same session, or swap their places. The chooser includes panes in other tabs.
Escape cancels it. Drag a sidebar tab onto another tab to put it before that tab;
drop onto **New Tab** to move it to the end. Reordering preserves the active tab.
These operations keep processes, block identities and existing terminal contents.
Moving a tab's last pane closes the empty source tab. API move ratios specify the
left/top share; existing unaffected split ratios stay exact.

The desktop applies Rex's merged keymap, including configuration overrides and
removals, Lua/block actions, key sequences and stacked key modes. Escape cancels a
sequence or leaves a mode; exclusive modes consume unbound keys. Configuration
reload updates bindings without restarting the desktop. Diagnostics appear above
the terminal. Ctrl+Shift+Q remains reserved for quitting.

Load client-specific Lua sources in order with repeatable command-line options:

```sh
rex-rs --client-script ./keys.lua --client-script ./work.lua
```

Sources run on Rex through `client.set_script`. A failed load preserves the previous
scripts; successful loads refresh bindings immediately. Accepted sources are sent
again for a new server/client identity, without reexecuting them on ordinary
connection resume. Files are read at desktop startup; configuration reload does
not reread those files.

The pinned GPUI input API lacks actual hardware codes and erases Shift for symbol
keys. Physical bindings and shifted punctuation therefore remain unsupported by
the desktop adapter. Cross-server callbacks and callbacks requiring the worker
while it awaits the originating Lua action also remain outside current coverage;
the latter receive an explicit refusal rather than hanging. Copy now needs the
terminal worker for native selection formatting, so a nested Lua callback to
`client.copy` is refused while that worker waits for the originating Lua action.

### Development logs

`mise run dev` writes client connection lifecycle and errors to stderr, including
the endpoint, failed API method and full error chain. Terminal failures identify
the pane/block and include the resume failure before requesting a fresh snapshot.
Identical errors are printed at most once every five seconds while retries
continue. Logs use Unix timestamps and do not include protocol payloads or
reconnect credentials. There is no default client log file; capture a development
run with:

```sh
mise run dev 2>&1 | tee /tmp/rex-rs-dev.log
```

Successful workspace changes also log the API method, session/window IDs, returned
revision and request duration. For tab synchronization problems, correlate the
`workspace applied method=session.new_window` line with the server's events;
labels, terminal text and key contents are not logged.

The official Rex server runs separately. For a server started with `rex server
start`, follow its captured logs with `rex server logs --follow`. A foreground
`rex server run` writes logs to its own shell. For a systemd-managed server use
`journalctl --user -u rex -f` (user service) or `journalctl -u rex.service -f`
(system service). `rex server status` prints the current server's log file path.

The worker bootstraps and attaches, fetches newer authoritative views on events,
and restores each terminal's Ghostty snapshot before applying ordered VT output.
Optional Kitty image records are checksum-validated and omitted during import;
their text/history state remains available, while image rendering is unsupported.
Malformed snapshots stop retries for that pane until the desktop reconnects.
Ghostty stays on that worker; the UI receives owned frames. Keyboard requests go
through `client.key` so Rex Lua bindings can handle them, and paste uses block
events. Workspace shortcuts resolve through Rex's merged keymap before terminal
input. Keys and interactions waiting behind an action remain ordered, including
its returned native actions. Unbound text/IME commits retain GPUI's input-handler
path without a duplicate raw-key send. Broader real IME testing remains manual.

Quitting the desktop detaches without stopping Rex or its shells. Control loss
uses Rex client resume credentials and fresh bootstrap; data loss attempts exact
byte-count resume, then obtains a fresh snapshot if necessary. Mutations whose
delivery is uncertain are reported and never replayed automatically. Same-instance
reconnect preserves presentation identities; a new server instance resets views.

Rex ownership is per block and governs size/theme preferences. It does not grant
exclusive input access. The sidebar can explicitly take/release size ownership
for the active block. Other clients' changes appear through Rex events. When
another client changes the grid size, clicking, typing, pasting or scrolling in
the desktop terminal reclaims size ownership and restores its measured viewport.
Incoming output alone does not reclaim it. Scrollback and selection use this
client's restored terminal replica.

Sidebar window names fall back to the live terminal title for autogenerated
server labels; explicit labels remain unchanged and show the program title as a
secondary line. Terminal titles are fetched on attachment and updated from Rex
events. Drag to select text, including outside the viewport, and use Ctrl+Shift+C
to copy. Wheel events browse local history in a normal shell and go to applications
that use mouse reporting or the alternate screen. Hold Shift to force local
history browsing. Applications that request mouse tracking receive left/middle/right
presses, releases and their requested drag/hover motion. Hold Shift to select text
locally. Motion is coalesced by terminal cell, and click-only applications do not
queue hover events. Releasing a drag outside a pane or leaving the application
releases held terminal buttons. Focus-in/out reports follow actual pane/window
focus when the program enables terminal focus reporting.

Ctrl+Shift+F opens a local history search overlay without resizing the terminal.
Matching is literal, with ASCII case ignored; Unicode text is matched without
Unicode case folding. Queries are limited to 1,024 UTF-8 bytes. Enter selects an
older match, Shift+Enter a newer match, and Escape closes and cancels the search.
The overlay shows incremental counts while Ghostty searches this client's restored
history. Query, viewport and selection state remain independent between clients.
The local worker operations send no terminal input or ownership RPCs; a normal
terminal click can still restore measured size through the workspace interaction
policy described above.

Selection uses Ghostty's persistent history coordinates, including drag autoscroll,
word/line gestures and rectangular selection. Copy uses its native plain-text
formatter: soft wraps are unwrapped and whitespace is trimmed. Copies spanning
more than 262,144 cells or exceeding 4 MiB fail explicitly without truncation.
Exact data-channel resume retains query and selection; a fresh snapshot rebuilds
the owned query and clears selection. Same-server control reconnect restores query
intent when history becomes available. Detached/closed blocks and a new server
instance discard their local interaction state. Local history interaction is
unavailable while disconnected.

When a program exits, its final screen and exit status remain available for
scrolling and copying; the completed data stream is not resumed or retried.

Output is parsed in bounded batches and visible frames are materialized at display
cadence. Admitted hidden placed panes keep parsing output/history while deferring
frame copies.
Owned row identities and prepared glyphs are reused as rows scroll. See
[performance diagnostics](docs/performance.md) for the tested boundaries.

The current renderer supports tiled terminal layouts, including arbitrary split
fractions. Floating layers and unsupported block creators are disclosed explicitly
and never coerced into a different server layout. Empty sessions/windows are safe.
Up to 16 placed terminal streams are decoded at once, prioritizing the active
window; individual viewports are bounded to 65,536 cells and snapshots to 64 MiB.
Full nonterminal block rendering, floating-layer interaction, graphics and remote connection setup
remain future work. Detached blocks retain metadata without a local terminal
stream; placement restores their display from Rex's current snapshot. Managing a
nonterminal block's placement does not add a renderer for its contents.

`mise run verify` includes `rex-native-smoke`, which starts its own isolated
installed Rex server with temporary state and config. It checks native control,
terminal snapshots/output, ownership, input, cached desktop updates and process
lifetime without changing the user's sessions. The `rex` executable must be on
PATH. Unit tests include malformed messages, view validation, snapshot continuation
and callback/error ordering. Desktop interaction still needs manual verification.

The old private-protocol server and `rexctl` remain development regression fixtures;
they are not used or started by the desktop. See [legacy local server notes](docs/local-server.md),
[the migration plan](docs/rex-server-migration.md), [control protocol notes](docs/rex-control-protocol.md)
and [terminal protocol notes](docs/rex-terminal-protocol.md). Legacy performance
measurements in [performance.md](docs/performance.md) describe the custom backend;
they are not performance claims about this native Rex path.
