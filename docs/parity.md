# Rex client coverage

Audit reports and `docs/audits/` evidence referenced below are local development
artifacts and are not distributed with this repository.

The Linux GPUI desktop now connects to the installed official Rex server using
control API revision 7 and `rex.data.terminal.v1`. The server's live `GET /llms.txt`
is the protocol reference. The older Rex 1478 audit (local audit evidence)
describes historical binary evidence and the former private backend; its protocol
compatibility limitations do not describe the new native client path.

The current priority is functional parity. Visual styling is a separate workstream
for the forthcoming UI iteration tool. Responsiveness is a regression check for
each functional change; matching macOS response times is not a completion gate.
Keep Rust and the current renderer while extending behavior through the official
server. Ghidra 12.1.2 is now available to REA and Linux perf is installed; use
decompilation for specific unresolved reference behavior, rather than delaying
features whose contracts the live server already establishes.

| Area | Current implementation | Remaining work |
| --- | --- | --- |
| Sessions/windows/blocks | Authoritative views and inventory, stable identities, create/close/rename, focus/zoom, same-session move/swap across tabs, drag tab reorder, detached browser/restoration and windowless sessions | Richer drag placement and accessibility |
| Splits | Fraction-preserving tiled trees and draggable dividers through Rex commands | Floating-layer rendering/interaction |
| Terminal state | Ghostty snapshot import, history, VT/theme/resize/exit, native cursor shape/color/blink, underline variants/colors, faint/strike/overline and text blink | Broader mixed-script/full-screen/graphics and platform coverage |
| Input | Ordered Rex keys with server Lua handling, committed Unicode/IME text, bracketed paste, application focus events and mode-aware mouse buttons/drag/hover | Real IME testing, physical/shifted-symbol key data, configurable Quit and broader terminal input protocols |
| Viewport/clipboard | Measured sizes, per-block size ownership, local history scroll, native persistent selection/autoscroll, logical soft-wrap/Unicode copy, history search and application wheel reporting with Shift override | Program clipboard integration, hyperlinks, broader resize/ownership/IME checks and dense-search optimization |
| Process lifetime | Rex owns processes; desktop detach/reopen and block detach/restore retain the original shell PID | Recovery beyond Rex server guarantees is outside client ownership |
| Reconnect | Control token resume and bootstrap; data byte-count resume or fresh snapshot; stale work rejected | Broader sustained-output and crash stress testing |
| Shared clients | External changes via events; per-block size ownership; input independent of ownership | Richer ownership presentation and remote connection setup |
| Unsupported state | Empty/windowless sessions and detached inventory are supported; floating layers and unknown creators disclosed | Full nonterminal rendering and floating-layer interaction |
| Automation | Official Rex CLI/API, 35 native actions, client scripts, merged shortcuts/modes/sequences, ordered Lua/block actions and real results | Platform key-data gaps, recorded shortcut settings, command palette, nested worker callbacks and cross-server routing |
| Verification | Native isolated Rex smoke plus existing regression/unit gates | Manual desktop interaction, accessibility and visual fidelity checks |

## Functional milestones

The order below groups dependencies and keeps behavior independent of the final
visual design. Each milestone needs native isolated tests plus targeted desktop
checks; source implementation alone does not establish complete parity with the
reference application.

| Order | Milestone | Completion checks |
| --- | --- | --- |
| 1 | Client actions and configuration | Implemented: 35 strict declarations, shared dispatch/validated targets, FIFO callbacks/results, generation rejection and bounded transport. Merged configuration bindings/removals, Lua/block `client.action` and its returned native actions, startup `client.set_script`, sequence prefixes, stacked/exclusive/one-shot modes and diagnostics now reach the UI. Keys, pointer focus and layout commands queue behind action chains; new-pane typing waits for readiness, buffered modal input stays in rename. Still partial: GPUI erases shifted-symbol/physical key data; Quit is reserved, recorded shortcut settings and nested worker callbacks remain. Broader Wayland/IME and recovery checks still needed. |
| 2 | Terminal focus and mouse input | Implemented: ordered `block.event` focus-in/out, mouse buttons/drag/hover by requested mode, Shift local selection, outside release and held-button cleanup. Native fixtures cover modes 1000/1002/1003/1004, modifiers, coordinates, mode-only changes and held gestures across reconnection. The isolated X11 desktop fixture exercises actual pointer/focus routing. Broader Wayland/IME and protocol coverage remains ongoing. |
| 3 | Pane and tab management | Implemented move/swap and tab reorder through shared core commands and declared actions, a pane Arrange chooser and sidebar tab dragging (drop before a tab or onto New Tab for the end). Isolated native checks cover shell PIDs/output/owners, same/cross-tab moves, closing an empty source tab, focus/zoom, exact unaffected split IDs/ratios, no-op reorder, invalid targets, replica convergence and reconnect aliases. Actual X11 chooser/drag checks pass. Detached discovery, new-tab restoration, placement beside a tiled anchor and explicit closure are implemented; their current validation status is below. Richer placement gestures and broader accessibility/Wayland checks remain. |
| 4 | Selection, copy, search and metadata | Implemented native persistent selection across local history, autoscroll, logical soft-wrap/Unicode copy and incremental literal search with older/newer navigation. Verified generation cancellation, isolated replicas, same-instance query restoration, exact data resume, exited history, native/API and desktop clipboard routing. Still missing: hyperlinks and program clipboard events. Dense-search worker optimization and broader interaction checks remain. |
| 5 | Terminal fidelity | Implemented native cursor shape/color/blink/wide-tail and all underline kinds/colors in owned frames, faint/strike/overline and paint-only text/cursor blink. Direct VT, real Rex snapshot/stream and X11 pixel fixtures pass; glyph geometry remains reusable. Styled missing-glyph fallback now retries suitable upright faces while preserving resolved glyphs; broader mixed-script/full-screen/graphics and platform checks remain open. |
| 6 | Rich layouts and block capabilities | Render/interact with floating layers while preserving Rex layout semantics. Detached placement is covered by pane management. Dispatch nonterminal creators by advertised capability; retain explicit unsupported states for creators without a renderer. |
| 7 | Remote attachment | Add configured HTTP/WebSocket/Tailscale endpoints and identity/error handling without moving process ownership into the client. Test revoked access, dropped streams, new-server instances and exact data resume. |

Small independent fixes, such as focus reporting, can land alongside the client
action foundation. A command palette should consume the same action registry
rather than introducing another command path. Pane/tab commands can initially be
invoked through actions and existing controls before new drag/drop or styling.

The pinned Ghostty wrapper already exposes tracked selection, formatting and
bounded search primitives. Reuse those on each terminal's owner worker, publishing
owned highlights and text, instead of building a second scrollback/search engine.
Search should advance in bounded steps; its available matching semantics do not
justify promising regex or full Unicode case folding. Keep selection anchors
valid across scrolling, output, reflow and history pruning.

## Evidence and regression boundary

The current audit (local audit evidence) and
`audits/rex-20261028/server-api.json` retain source findings, live API schemas and
isolated timing harnesses. The live `client.key` schema returns `handled`; it
does not return client actions. `client.action` has its own result contract.
The current input path already lets server Lua handle keys. Native action execution
and callbacks are implemented, along with merged keymap resolution and script
loading. Avoid describing this as a missing Lua runtime. Native actions are
registered with `client.keymap`; configuration/block actions use `client.action`
and its ordered returned `client_actions`.

The resolver implements documented precedence, mode stacks and Escape cancellation.
Sequence mismatch drops the consumed prefix and retries the current key. There is
no invented sequence timeout. One-shot modes leave after a resolved shortcut,
except explicit exit actions handle their own pop. Those edge policies are local
choices where the available reference contract does not establish more detail.
Physical key resolution is tested in the pure resolver but cannot yet be supplied
by the GPUI adapter; shifted punctuation has the same platform-data limitation.
Client script paths are explicit startup options, not an inferred reference-client
configuration convention. Nested callbacks needing the worker during Lua execution
are refused to prevent a cycle, and remain a parity gap. Long-running Lua calls
also occupy the terminal worker until the RPC returns; terminal decoding during
such actions still needs an asynchronous worker operation pump. Ordinary typing
uses the existing fast path.

For each milestone, check ordinary typing, scrolling, tab changes and dense-output
responsiveness against the current Linux baseline. Keep hidden-frame deferral,
row/glyph reuse, bounded queues and uncertain-mutation handling intact. Run
broader performance profiling when a regression appears; a language rewrite or
macOS timing comparison is not a prerequisite for continuing parity.

The native action foundation passed `mise run verify` with 157 unit tests and
all smoke executables. The isolated X11 desktop check exercised action discovery,
font/sidebar changes, clipboard round-trip, target rejection, session/tab/split
commands, configuration reload and rename-focus preservation. Artifacts and
source identities are retained locally in `audits/rex-20261028/actions-parity/`.

The desktop never initializes a session on attachment, starts a private server,
writes its own workspace metadata or stops Rex. Legacy private-protocol server
modules and smoke tests remain development regression fixtures. Their global
controller lease and metadata-only restart recovery are not native Rex semantics.

`mise run verify` runs formatting, compilation, strict Clippy, unit tests, legacy
regressions and the isolated native interoperability smoke sequentially. The Rex
binary must be installed on PATH. See [migration plan](rex-server-migration.md),
[control protocol](rex-control-protocol.md) and [terminal protocol](rex-terminal-protocol.md).

The configuration integration passed `mise run verify` with 175 unit tests and
all smoke executables. Rebuilt isolated X11 desktop fixtures checked configured
binding overrides/removals, Lua false-return precedence, native/Lua/client-script
sequences, Escape cancellation, exclusive and one-shot modes, startup scripts,
ordered returned actions, ordinary typing exactly once, rapid typing into a new
tab, buffered input into a Lua-queued rename dialog, dialog closure, live keymap
reload and prior clipboard/context/focus behavior. A real burst-input test found
and verified the fix for a dialog that stayed open after a deferred rename saved.
The callback FIFO smoke now observes first admission before submitting the second
concurrent call; `client.perform` is outside the server's ordered request lane.
Artifacts and source identities are retained locally in
`audits/rex-20261028/configuration-parity/`. X11/lavapipe coverage does not establish
Wayland or real IME behavior, button-first rapid-input behavior, or sustained-output
responsiveness during long Lua actions. Full parity remains in progress.

The move/swap/tab-order integration passed `mise run verify` with 188 unit tests
and all smoke executables. The isolated native suite verifies process IDs,
output markers and size owners, exact split topology/fractions, focus/zoom,
source-tab closure, invalid-operation atomicity, two-client convergence and
control resume. A rebuilt private X11 fixture verifies actual tab drag/drop
(before a tab and at the end), chooser move/swap, process retention, modal typing
isolation, Escape and the Arrange header button. Both previous action/clipboard
and dynamic-shortcut/burst-input desktop fixtures pass against the rebuilt app.
Drag payloads now carry server instance and connection generation so aliases
reused after replacement cannot target another window; a focused regression
checks that UI boundary. Buffered picker input bypasses terminal readiness waits.
Evidence and source identities are retained locally in
`audits/rex-20261028/management-parity/`. Nonterminal/floating layout interaction
and broader accessibility remain open.

The detached-block integration now separates inventory from window placement.
Detaching the last pane preserves a windowless session and its running block;
restoration and placement beside a tiled anchor reuse the original block. Detached
blocks do not consume local stream slots, and unsupported creators remain explicit
when placed. New-tab restoration uses a disposable carrier because the live server
cannot create a tab around an existing block directly; uncertain placement and
carrier cleanup errors are reported without replaying the original move.

The detached integration passed `mise run verify` with 216 unit tests and all
smoke executables. Native checks cover detached-only bootstrap/resume, stable
aliases/PIDs/output/owners, metadata changes without layout events, carrier
cleanup, invalid targets and two-client convergence. The rebuilt private X11
fixture passes header detach, browser typing/Escape isolation, new-tab restore
from an inactive windowless session, placement without taking anchor focus, and
explicit close. Existing move/swap/tab-drag, action/clipboard and dynamic-shortcut
desktop fixtures also pass against that binary. The Linux self-spawning smoke
fixtures additionally pass after unlinking their running executable pathname.
Evidence and source identities are retained locally under
`audits/rex-20261028/detached-parity/`. These checks do not establish floating or
nonterminal rendering, Wayland/IME behavior, broader accessibility, or heavy
inventory-load responsiveness.

The final UI integration additionally verifies overview reveal through shortcuts
and native actions, queued overview intent, same-session detached restoration,
and removal of inline rename state when another client closes its tab/session.
Normal close-button, shortcut and external-close desktop checks report no missing
block errors and preserve usable surviving panes. The native suite strictly polls
local/external close races with queued resize, focus, scroll, pointer and text
events; explicit invalid action contexts still refuse. Known retired aliases are
handled as normal lifecycle events, while live-target failures remain errors.
Overview metadata polling is nonblocking and bounded; process activity beyond
the current 32-record refresh cap remains incomplete.

The history-search/selection milestone passed the full gate with 238 unit tests
and all smoke executables, plus seven isolated X11 desktop fixtures. The final
search fixture verifies 135-byte Unicode/soft-wrap clipboard roundtrips, 95-line
history selection, Shift override, release/blur cleanup and unchanged Rex size,
owner, process and raw input during search. Evidence and exact source identities
are retained locally in the search/selection bundle (local audit evidence).

Its dense-search guard seeded 2,000 rows at 350 columns, continued matching output
and replaced queries while another pane received input. Forty samples measured
68.89/140.78 ms p50/p95 input-to-cache latency; cancellation took 17.93 ms. The
search eventually completed with 780,640 matches. This exposes synchronous native
highlight/search work to optimize; no macOS, display-latency or matched quiet
baseline claim follows. The full parity objective remains open, including terminal
fidelity, hyperlink/program-clipboard integration, rich layers/creators, remote
attachment, platform key data and accessibility.

The subsequent search follow-up removes hidden highlight extraction and stops
hidden drag/autoscroll, preserving completed selection and full highlights on reveal
without new VT output. Native tests also cover hide/navigation/reveal across tabs.
The final gate passes 244 tests; targeted search, close and detached desktop checks
pass. The two-visible-pane dense stress guard remains about 69/148 ms p50/p95, so
visible-search responsiveness remains open. The next confirmed terminal fidelity
requirements are recorded in the source audit (local audit evidence).

The visibility/gesture follow-up bundle (local audit evidence)
retains exact final-source identities, the before/after measurement and all targeted
GUI checks. Userspace CPU sampling was denied before launch; no dominant-stack
attribution is established and no OS settings were changed.

The terminal presentation milestone passes `mise run verify` with 263 tests and all
smoke executables. Eight X11 fixtures pass on the final binary: native decoration/
cursor pixel and blink checks, plus existing search, close, overview, detached,
management, actions/clipboard and dynamic-shortcut regressions. The quiet input-cache
guard passes at all three grids; it excludes GUI/display timing and does not isolate
a speedup due to this change. Evidence is retained in
the presentation bundle (local audit evidence).

Styled CJK fallback remains a concrete pre-existing gap: old/new italic glyph pixels
are identical despite correct UTF-8 and installed regular CJK coverage. The pinned
shaper filters regular faces from italic fallback candidates. See
the next investigation (local audit evidence); a live missing-glyph
trace and targeted fallback implementation still need verification.
