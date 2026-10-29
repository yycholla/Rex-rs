# Legacy local Linux server

This private backend is retained for development regression checks. The desktop
now connects to the official Rex server through `src/rex/`; it does not use or
autostart this backend. The notes below describe legacy behavior.

The server owns terminals independently of the desktop. Shared layout commands,
connection-scoped terminal authority, automation and durable metadata build on
that boundary. This implements our Linux design, informed by the Rex audit's
declarations; it does not implement the official Rex protocol.

## Ownership and transport

`server/host.rs` creates `TerminalWorkspace` and all Ghostty handles on its owner
thread. PTY workers retain bounded IO queues. Socket workers decode requests and
encode replies independently; the owner loop only receives owned values. All
layout mutations use existing core Commands, and input/resize use existing
terminal methods. Buttons and shortcuts enqueue the same requests.

Messages use a four-byte big-endian length followed by JSON. Protocol version is
4. This adds native cursor presentation and extended underline kinds/colors to
owned frames and compact rows. Clients must restart/update alongside the server. Every mutation
names the server incarnation established by attachment.
The server serializes accepted requests and increments its global revision when
workspace state, a frame or process status changes. Unchanged polls and no-op
layout commands keep the revision. `known_revision` requests a full snapshot
when the client's cache differs; it is not an optimistic concurrency condition.

Snapshots contain owned workspace state, terminal frames, process statuses and
shell PIDs and a metadata-recovery indicator. The client validates membership, workspace focus/ID invariants,
frame dimensions/cursors and the total cell budget before publishing a cache.
UI methods only enqueue bounded work or drain that cache; socket waits, startup
and reconnect run on a worker. Desktop `Poll` opts into compact per-connection
row patches; the worker validates and merges them against an acknowledged
revision before publishing a complete owned cache. Unchanged rows/frames retain
Arc identity. Hidden desktop frames stay at their last delivered text while
status, PID and workspace metadata remain current. The server continues parsing
hidden output but defers cell-frame conversion until that pane is visible.
Revealing a tab or unzooming catches up without requiring another output byte.
Explicit `Snapshot` requests refresh every dirty pane and return full JSON.

The server keeps output parsing and terminal history running with no clients.
Reconnect obtains the current visible screen and layout, with valid cached
frames for hidden panes; there is no output-event log,
byte replay protocol or terminal-content checkpoint. Row patches are cache updates,
not an output replay log. A failed mutation
whose delivery is uncertain is reported and never replayed. Queued mutations
from an old incarnation are rejected if a restarted server reuses pane IDs.

## Endpoint and lifetime

The default socket is `$XDG_RUNTIME_DIR/rex-rs/server.sock`. Without that variable,
it uses the system temporary directory's `rex-rs-<effective-uid>/server.sock`.
The immediate parent must be a real directory owned by this user with mode 0700;
the socket and lock are mode 0600. An exclusive file lock coordinates startup.
Stale socket cleanup refuses active listeners, foreign files and symlinks, and
final cleanup only removes the socket inode this server created.

The desktop starts its own executable with `--server --socket PATH` when needed,
with detached stdio and a separate Unix session where possible. It initializes
one Development session only when the attached server is new and empty. An
intentionally saved empty workspace stays empty after recovery. Subsequent
desktop connections retain existing work. A desktop exit only disconnects.
After an established server exits, the current client retries attachment; start
a replacement server explicitly or reopen the desktop to launch one.

Run a headless server with `cargo run --locked --no-default-features --features
ghostty --bin rex-server` under `mise exec --`. `--socket PATH` selects a custom
endpoint for headless clients/tests. `--state-file PATH` selects private metadata,
and `--ephemeral` disables metadata storage/recovery. `--shutdown` requests explicit shutdown of
that endpoint and receives acceptance before the server performs final cleanup.
The server signals all owned shell/verified foreground groups and waits up to two
seconds total for reaping. This wait is exclusive to final server shutdown;
ordinary pane close remains asynchronous. Arbitrary processes that deliberately
leave the owned session/process groups are outside the cleanup guarantee.

## Durable metadata

The normal endpoint stores `$XDG_STATE_HOME/rex-rs/workspace.json`, or
`$HOME/.local/state/rex-rs/workspace.json` when XDG state is unset. A custom socket
defaults to `<socket-filename>.workspace.json` beside that socket. The public
`serve` embedding helper remains ephemeral; command-line server startup enables
storage unless `--ephemeral` is supplied.

Metadata format version 1 stores only the core workspace: labels, tab order,
layouts, split IDs/ratios, focus, per-window zoom and the allocator. It never stores
PIDs, terminal output, arbitrary executable commands or shell jobs. Restart opens
fresh PTYs/shells for the saved panes and sets `restored_metadata` in snapshots.
The desktop shows “Recovered layout · new shells.” Terminal size is measured again
by the controller; prior terminal history is not recovered.

Files are bounded to 1 MiB, owned by this user and mode 0600 in a private 0700
directory. A separate lifetime lock prevents competing metadata writers. Saves
validate state, sync a unique temporary file, atomically rename it, then sync the
directory. Corrupt, unsafe, oversized or unsupported metadata stops startup with
an error and is not silently overwritten. A pre-rename failure preserves the old
file; a directory-sync failure after rename reports uncertain durability of the
valid new bytes.

If a save fails after a command changes live state, the reply explicitly says it
was applied in memory but not durably saved. Dirty metadata remains pending;
retrying even the identical no-op workspace command retries persistence. Server
shutdown also retries dirty metadata and can be rejected until that save succeeds.
Successful layout-command acknowledgement includes completion of its save.

## Controller and viewport policy

The first connection to request control gets the controller lease. Other clients
observe the controller's canonical terminal size, screen and scroll position;
their input, resize and history-scroll requests are rejected. Shared workspace
commands, including labels, layout, zoom and focus, remain available to all trusted
local clients. This is a single-user local policy, not authorization for remote
or untrusted users.

Explicit takeover replaces the controller; the previous connection immediately
loses terminal authority at server dispatch. Disconnect releases only that
connection's lease. Observers use Take Control rather than silently competing
for terminal size, and the new controller remeasures its viewport. Local copy and
selection remain available in observer mode. Per-client focus and independent
history viewports are future options rather than part of the current policy.

## Automation

`rexctl --help` lists typed commands. Put `--socket PATH`, `--json` and
`--take-control` before the command. `status`/`list` return labels, layouts, stable
IDs, process status, revisions and recovery information without starting a server
or initializing sessions. `snapshot` returns the full owned state; `capture PANE`
returns the current screen. `input PANE TEXT` sends exact bytes without an added
newline. Mutation results and `--json` failures use machine-readable JSON; failures
exit nonzero. The CLI never retries a mutation with uncertain delivery.

Terminal commands acquire a free lease and respect an existing controller unless
`--take-control` explicitly requests takeover. Their lease ends when the CLI
connection closes; a desktop that was displaced remains an observer until it
requests control. Layout commands share the desktop's core Commands, including
directional focus, zoom and split ratios. The `call` command accepts a typed JSON
local operation. Public API schema discovery and event streaming remain future work.

## Current limits and policy

Limits are 16 connections, 32 pending server requests, 64 queued client commands,
16 active panes, 65,536 aggregate visible terminal cells, 16 MiB encoded messages,
128 Unicode characters per label, and 64 KiB raw/key input per request. Paste
allows 65,524 UTF-8 bytes to leave space for bracketed-paste markers. Oversized
messages/grids are rejected before allocation or terminal resize. Terminal
history keeps the configured 10,000-line default. Closed processes awaiting
cleanup are bounded separately at 64.

Clients share workspace focus; terminal viewport/input follows the lease policy
above. Expected-revision command preconditions remain open. There is no remote
listener, live process recovery across server death, Lua/configuration loader,
pane moving/floating or terminal-content persistence.

## Verification

`server-smoke` launches an actual server child and checks offline output, retained
shell PIDs/layout/labels, multiple clients, explicit pane cleanup, invalid IPC,
asynchronous cache/input, ignored-HUP foreground jobs and their background child,
and restart incarnation guards. It also verifies controller takeover/release,
shared layouts without restarting shells, fresh-shell metadata recovery and
dirty-save retries after a protected destination failure. `cli-smoke` exercises
public CLI commands and JSON/errors through separate processes. Unit tests cover
endpoint/storage safety, no-op revisions, layout geometry, limits and malformed
peer snapshots. Both are included in `mise run verify`.

Full UI interaction still requires manual desktop checks. Same-incarnation forced
socket interruption of a surviving client, abrupt server-crash orphan cleanup,
long-running stress and rendering performance need additional coverage.

See [performance.md](performance.md) for measured debug-build input latency,
row-update payloads, row-layer painting and the protocol-upgrade procedure.
