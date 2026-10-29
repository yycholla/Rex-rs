# Architecture

The production desktop uses the existing Rex server:

```text
desktop → ui → rex client worker → Rex control + terminal WebSockets
                 └ owned presentation views + terminal frames → ui
```

`rex/transport.rs` owns the local Unix WebSocket on a dedicated IO thread, with
request correlation, bounded event buffering, revision negotiation and reconnect
identity. Its blocking facade serves the terminal worker; cloned control handles
provide asynchronous replies and nested requests. Requests are matched by ID,
including out-of-order responses whose sequence watermarks can decrease. Ordinary
requests have 64 slots; eight separate priority reply slots prevent callback
completion from being blocked by ordinary saturation. Unsupported cross-server
callbacks receive prompt errors, and uncertain mutations are never replayed.
`rex/workspace.rs` validates authoritative session views independently
of the local workspace mutation engine. Rex string IDs map losslessly to stable
presentation IDs; per-session revisions reject stale views. Its presentation
layout preserves arbitrary fractional ratios and explicitly retains unsupported
layers and block metadata.

`rex/inventory.rs` pairs `session.view` with `session.list_blocks` so detached
blocks retain their aliases, labels and ownership outside placed window layouts.
The model validates and publishes paired results together; the two server reads
are separate requests, not a transactional snapshot. Refreshes run after relevant
events and commands, on browser opening or explicit refresh, and through periodic
sweeps. A sweep starts roughly every two seconds and services one session per
worker turn, so this is not a hard freshness deadline. Combined placed/detached
inventory is bounded to 1,024 block records.

`rex/terminal.rs` opens ticketed terminal data channels, restores the binary
Ghostty snapshot through `terminal/ghostty.rs`, and feeds subsequent VT output.
Nonce-authenticated APCs apply resize/theme/exit changes. Exact-byte data resume
preserves the decoder; a refused or unsafe resume requires a fresh snapshot.
The display replica never sends device query replies to the process: Rex already
owns terminal emulation. Snapshot import and every Ghostty FFI call stay in
`terminal/ghostty.rs`; handles never leave their owner worker.

`rex/client.rs` provides the nonblocking cache, bounded command queue and UI
notifications. It reboots attachment after control loss, preserves mappings on
same-instance reconnect and rejects queued work from a previous connection
generation or instance. Keyboard, committed text, paste and workspace mutations
share one ordered control path. Size ownership is per block, independent of input
rights. Terminal viewports and local history are client presentation state.

`core/interaction.rs` carries owned search/selection commands and viewport annotations.
`terminal/ghostty/interaction.rs` owns native search, persistent selection and gesture
handles on the terminal worker. Searches are incremental literal matches with ASCII
case ignored, bounded to 1,024 query bytes. Copy formats the native selection with
logical soft wraps unwrapped and whitespace trimmed, rejecting spans above 262,144
cells or output above 4 MiB without truncation.

`rex/client/interaction.rs` admits local operations without terminal input or ownership
RPCs. Query edits use a separate per-pane coalescing lane and monotonic generations;
older updates/navigation cannot replace newer intent. Cancellation has reserved
admission beyond the normal edit-slot limit and remains admissible while disconnected.
Selection release/cancel has deferred admission when the ordered input queue is full.
Copy uses a worker future, then the owning UI window installs the clipboard text.

The stream owner ticks native interaction even for hidden or exited replicas. Owned
annotations publish independently of cell-frame Arc changes. Hidden replicas publish
search status/counts without collecting native match lists or selection spans, while
cell materialization stays deferred. Revealing forces complete span/frame publication
even for a completed query with no new VT output. Visibility is applied before snapshot
restore and local operations, so a queued search uses the current layout. Hiding also
stops active drag/autoscroll while preserving native selection; late hidden Press/Drag
events are discarded, while release/cancel/clear and history navigation remain valid.
Spans with a mismatched viewport are cleared rather than attached to an older frame.
Visible frame materialization precedes full annotation extraction. Visible match
endpoints use viewport-relative lookup and local ordering, with Screen-space fallback
for long/offscreen matches; native history matching and highlight coverage are unchanged. Pointer commands carry the displayed viewport
token, and native selection rejects stale tokens. The find overlay in
`ui/workspace/find.rs` consumes text/composition locally and does not change grid size.

Exact data resume retains native handles and selection. Fresh snapshots construct new
handles from owned query/generation intent and clear selection. Same-instance control
reconnect retains that intent until streams restore; a new server instance, block close
or detach retires local interaction state. Disconnected local history is unavailable.
These states are independent for each client and do not mutate Rex's authoritative
terminal, process or size owner.

The desktop does not host PTYs, initialize sessions on attachment, save workspace
metadata, launch a custom server or stop Rex on exit. The local state engine,
PTY runtime and private server below remain development regression fixtures;
their persistence/controller semantics do not describe the native Rex path.

`rex/actions.rs` defines the strict registry of 35 native actions. `rex/action_worker.rs`
translates workspace actions through the same core command boundary used by
buttons, preserves request diagnostics and refreshes authoritative views before
completing actions. The bounded callback mailbox carries owned generation-tagged
requests to the owning GPUI window. Its dispatcher serializes callbacks, validates
targets and replies once with the result or a structured error. Font/sidebar and
clipboard installation stay on the UI thread; native selection formatting and
pending workspace actions share the ordered input worker. Modal rename focus is
preserved. Client declarations and merged keymaps are refreshed on connection and
configuration changes.

`rex/keymap.rs` resolves the server's binding order, availability, sequences,
repeat policy and stacked modes without IO. `ui/workspace/shortcuts.rs` applies
logical GPUI keys before text delivery. Its bounded lane retains keys, pointer
focus and workspace commands behind an action, then executes returned native
actions before later input. New-pane typing waits for terminal input readiness;
connection generation changes discard pending work. Focus-state notifications
bypass that lane so deactivation cleanup remains prompt. Buffered input is routed
to a newly opened rename dialog rather than leaking to a terminal.

Client scripts load with `client.set_script` through the worker. Only accepted
sources are retained; loads are retried for new server/client identities, not
ordinary resume, because top-level Lua may mutate server state. Script loads
explicitly refresh keymaps even without a keymap event. `client.action` uses a
32-second transport deadline and completes after authoritative refresh; returned
client actions run on the UI lane. Nested callbacks needing the same waiting
worker are promptly refused, including `client.copy`, whose selection formatter
runs there; pure UI callbacks use the independent reply lane.
Physical keys and shifted-symbol normalization still need platform input data
that the pinned GPUI API does not expose.

`ui/workspace/management.rs` supplies a pane destination chooser and typed tab
drag payloads. UI controls and declared move/swap/reorder actions translate the
same core Commands into `session.move_block`, `session.swap_blocks` and
`session.move_window`. Session membership, placed targets, self moves and ratio
bounds are validated before selection or mutation. Server-authoritative refresh
follows each change; block aliases, terminal stream replicas and UI entities stay
keyed by the same block identity across windows. Newly created move splits retain
the requested floating-point fraction, while untouched server splits keep their
IDs and ratios. The legacy pure core quantizes only its own new split into its
existing permille representation. `ui/workspace/detached.rs` lists detached blocks,
including those in sessions without windows, and offers restoration, placement
beside an existing tiled pane, and explicit closure. A detached source can move
beside a placed anchor without taking its focus; swaps require two placed blocks.

`rex/restore.rs` restores an existing block to a new tab using a disposable carrier:
`session.new_window` starts `/bin/true` with a retained completed anchor, then
`session.move_block` places the original beside it, and `block.close` removes only
the anchor. The original block keeps its identity and program. This sequence is
not atomic. If placement fails or its response is uncertain, cleanup attempts to
close only the known anchor, never its whole window, which may already contain
the original. Cleanup failures are reported and may leave the carrier visible;
authoritative placement is refreshed when possible and uncertain moves are not
automatically replayed.

Only placed terminal blocks enter the bounded stream/replica cache. Detaching
retires local input gestures and the display replica while Rex retains the
program and terminal history. Restoring opens a fresh snapshot stream. Metadata
for unsupported creators remains manageable without implying content rendering.

Session overview selection is queued as its own UI intent, so earlier terminal
actions cannot erase a later request to show the overview. Shared native/core
execution reveals terminals for terminal operations; presentation actions retain
the overview. Unbound queued typing has no terminal recipient while the overview
is visible. Inline rename state is retired when its session or tab disappears,
including changes made by another client.

`rex/insights.rs` fetches overview clients and up to 32 terminal process records
through a connection-local background lane. It admits at most one metadata RPC
per worker turn after stream service and polls responses without blocking the
terminal worker. Four refreshes may be queued, with one metadata RPC in flight.
Dropping a read-only refresh receiver cancels unissued queries and drains any
already admitted response; disconnect completes outstanding futures with errors.
Mutation operations continue once admitted, even if their receiver disappears.
This metadata cap does not provide complete process activity for larger sessions.

Closing a pane retires its queued UI events without reporting a missing-block
error. The worker distinguishes formerly known block aliases from unknown IDs;
if a request races a remote close, it confirms removal through authoritative
refresh before discarding that request. Failures for live blocks and explicit
invalid native-action targets remain errors. UI events from removed or detached
views are ignored before admission as well.

## Legacy core state

`core/workspace.rs` owns sessions, windows/tabs, pane identities and binary split
trees. Every visible control and shortcut dispatches the same typed Command.
IDs are opaque, monotonically allocated, and never reused. Session and window
fields are private, so views cannot directly invalidate focus or layout.

Splits have their own stable IDs and first-child ratios in permille (100..900).
Closing a leaf removes its parent split identity and preserves surviving subtree
IDs/ratios. Directional focus computes geometric neighbors from those ratios.
Zoom belongs to each window's core state and is shared by the desktop and CLI;
focus changes clear incompatible zoom, while tab switches retain each tab's zoom.

A window always has an active pane. A session can have no windows when it retains
detached panes; its active-window, active-pane and layout accessors then return
`None`. New panes/tabs receive focus. Closing a split leaf promotes its sibling.
Removing a window's last placed pane closes that window; the session remains
while detached resources exist. Closing its last resource removes the session.
An empty workspace is valid. Tab switching preserves each tab's layout and focused
pane. `pane_ids()` includes hidden and detached panes so runtime reconciliation
retains their terminals. Legacy restore allocates only a window ID and reuses the
detached pane's runtime; serialized detached membership defaults to an empty list
for older metadata.

`Workspace::is_valid` checks decoded state before presentation can index into it.
Serialization derives are enabled with the terminal feature; rustc-only core
tests retain no external dependencies. `core/grid.rs` validates nonzero dimensions before FFI. `core/input.rs` carries
platform-neutral input. `core/frame.rs` contains owned cells, grapheme clusters,
colors, underline kinds/resolved colors and cursor shape/color/blink/wide-tail state; frames can cross threads. Adjacent equal-style
cells are batched into positioned text runs.

## Terminal runtime and legacy hosting

`terminal/ghostty.rs` is the sole Ghostty FFI adapter. It owns terminal state,
reusable render iterators, key encoding and PTY query responses. Ghostty objects
stay on their owner thread (the native client worker, or legacy server loop); never
implement unsafe Send/Sync for them.

`terminal/pty.rs` uses portable-pty. Blocking reads and writes run on workers
with bounded queues, while the reaper waits for child exit separately. Process
groups must be verified as belonging to the spawned PTY session before cleanup;
never signal the application's own group. Drop requests cleanup without joining
workers on the UI thread. Closed panes remain in a bounded retirement list until
their reapers finish. Explicit final server shutdown signals all live/retired
processes first, then waits for reaping within one shared deadline.

`terminal/workspace.rs` couples pane IDs to Ghostty state, optional PTYs and
cached Arc frames. `new` creates a headless workspace for VT tests; `live` adds
shells. A candidate workspace and new resources are prepared before committing
layout changes, so spawn failures preserve existing terminals. Polling feeds
output into Ghostty, snapshots changed terminals and queues device responses.
Exit publication waits for output EOF so final output is not lost. Output service
uses a 32 KiB total budget, 8 KiB per-pane slices and a soft 4 ms cutoff. The
focused pane goes first; background service rotates, with status/error checks
continuing after the output budget is exhausted. Queued bytes retain order and
backpressure.

The server opts into deferred hidden frame materialization. Hidden Ghostty
terminals still parse output, retain history and answer device queries, but do not
copy cells into presentation frames until visible. Tab/zoom changes refresh dirty
newly visible panes even without new output. Explicit `Snapshot` requests refresh
all dirty frames for authoritative automation captures. Standalone
`TerminalWorkspace` users retain eager frame behavior by default.

`server/host.rs` owns the runtime independently of desktop connections. Bounded
IO workers pass owned requests to one owner loop; no socket read/write or child
wait blocks desktop presentation. Revisions cover layout, frames and status;
server incarnation IDs prevent commands targeting reused IDs after restart.
`server/client.rs` reconnects on a worker and supplies a nonblocking validated
cache. Disconnect never reconciles panes away. See [local-server.md](local-server.md)
for the protocol boundary and [parity.md](parity.md) for remaining persistence work.

Desktop IPC uses per-connection compact row updates. `server/updates.rs` packs cell
attributes, checks the acknowledged baseline and reconstructs frames on the client
worker, retaining untouched row Arcs. Complete reset, resize, pane membership and
workspace changes are validated before publication. The UI compares identities
and a local state generation, avoiding complete-cell equality and idle layout
reconciliation. Connection baselines retain hidden text at its last delivered
frame while status/PID/layout stay current; newly visible panes catch up. Reset,
new-pane and resized frames remain complete. `Snapshot` remains available as full
JSON for automation and forces materialization of hidden frames.

Each socket connection has a server-assigned identity. A controller lease gates
terminal input/resize/scroll; explicit takeover revokes the previous owner and
disconnect releases the lease. Shared core workspace commands remain available
to all trusted local clients. A new controller reannounces its measured viewport.

`server/storage.rs` writes versioned, validated workspace metadata through private
temporary files, file sync, atomic rename and directory sync. State-changing
commands are acknowledged only after save succeeds. A failed save reports that
memory changed; dirty state is retried even on an identical no-op command.
Startup restores layout through `TerminalWorkspace::restore`, constructing new
shells on the owner thread. Snapshots disclose metadata recovery; no saved PID,
output or executable command is used. `server/cli.rs` uses the same versioned
transport for `rexctl`, without creating a second mutation path.

## GPUI

`ui/theme.rs` supplies semantic colors and defaults. `ui/components` contains
buttons, session rows, headers, text fields, empty/status states and terminal
surfaces. Components do not manage processes or mutate core state.

`ui/terminal_view.rs` owns focus, measured font metrics, viewport bounds, pointer
routing and text-input composition. Native selection state stays on the worker. It emits owned events to `ui/workspace.rs`. Resize
notifications are deferred and deduplicated. The workspace owns subscriptions,
reconciles views with cached pane IDs and awaits worker cache notifications. Mutations
are visible after server acknowledgement. A terminal keeps its entity while its frame remains in the bounded admitted
cache; eviction releases its view. Rex keeps its process across desktop disconnect.
Changing Rex server instance clears local pane entities so reused IDs do not
inherit old selection, input composition or zoom.

The root provides Workspace actions; rename dialogs switch to a modal context
so underlying shortcuts cannot operate on terminals. Dialogs restore terminal
focus when submitted or cancelled. Unicode fields use UTF-16 platform ranges
and grapheme-aware editing.

Each terminal stages one pending frame, preparing at most 64 changed rows or
roughly 4 ms per render before yielding. The last complete prepared frame remains
visible; incoming frames coalesce, while resize/font changes invalidate
incompatible work. Selection gestures and highlights use annotations paired to the
displayed viewport; copying uses the native persistent selection. Cursor and IME
use the displayed frame.
Hidden views do no preparation. One shaping call cannot be preempted, and budgets
are per pane rather than a global UI deadline. GPUI final drawing remains on the
UI thread. Its shared text layout cache has opposing frame/shaping lock orders,
so it must not be used concurrently for background shaping.

Terminal application pointer modes are copied from the worker independently of
frame materialization. The UI routes clicks, drag and hover according to those
modes, retaining Shift for local selection and coalescing motion within cells.
The worker rechecks the current mode before sending ordered `block.event` calls.
Held application buttons are released on focus loss; actual pane focus plus window
activation drives deduplicated terminal focus reports. Input readiness is distinct
from retained display frames, so same-instance reconnect can show old content
without accepting input before its stream is restored.

The terminal surface retains prepared rows by immutable row identity and paints
row backgrounds, glyphs and underlines in separate `Window::paint_layer` batches.
Do not issue unbatched per-cell glyph primitives: that repeats GPUI spatial-order
queries for every character. Cursor-only frames reuse row shaping and the prepared
row vector. Paint-only decoration flags, underline colors and faint composition
reuse shaped glyph geometry. Underline variants, strike and overline are batched
by paint properties. Faint uses 50% effective foreground/background composition;
intrinsic-color emoji use a cell-clipped half-background wash because the painter
has no emoji tint parameter. Invisible text suppresses its decorations as well.

Cursor painting follows native block/bar/underline/hollow and explicit color,
falling back to the frame foreground. A filled block repaints cached glyphs
through its cell mask using a contrasting effective cell background (or black/
white when needed for legibility); wide-tail coordinates normalize to the head.
Unfocused visible cursors are steady hollow blocks, a local presentation policy.

`TerminalView` supplies paint-only 500 ms blink phases. Text uses a process-shared
monotonic epoch; cursor phases reset on actual input, native cursor changes and
focus/activation. A retained cancellable task exists only while a visible active
view has blink demand. Hidden, inactive, disconnected, failed and exited views
paint steady and stop recurring timers; history/selection readiness stays separate.
Workspace rendering supplies exact tiled/zoomed visibility, excluding overview.
Frame/row data is never rewritten merely to animate a phase. Profiling is optional
through `REX_RS_RENDER_STATS`; see
[performance.md](performance.md) for measured stage timings and limits.

## Extension rules

- Change core invariants and tests before wiring new workspace controls.
- Keep transport and process cleanup out of rendering and text components.
- Preserve ownership, cached state and focus across tab/session switching.
- Report recoverable errors with context; do not silently replace failed shells.
- Keep Cargo.lock and dependency/toolchain pins coordinated by the integration owner.
- Use disjoint agent write sets and run `mise run verify` after integration.
