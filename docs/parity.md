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
| Input | Ordered Rex keys with server Lua handling, committed Unicode/IME text, bracketed paste, application focus events and mode-aware mouse buttons/drag/hover | Real IME testing and broader terminal input protocols |
| Viewport/clipboard | Measured sizes, per-block size ownership, local history scroll, native persistent selection/autoscroll, logical soft-wrap/Unicode copy, history search and application wheel reporting with Shift override | Broader clipboard/link desktop, resize/ownership and IME checks |
| Process lifetime | Rex owns processes; desktop detach/reopen and block detach/restore retain the original shell PID | Recovery beyond Rex server guarantees is outside client ownership |
| Reconnect | Control token resume and bootstrap; data byte-count resume or fresh snapshot; stale work rejected | Broader sustained-output and crash stress testing |
| Shared clients | External changes via events; per-block size ownership; input independent of ownership | Richer ownership presentation and remote connection setup |
| Unsupported state | Empty/windowless sessions and detached inventory are supported; floating layers rendered and unknown creators disclosed | Full nonterminal rendering and broader floating-layer desktop validation |
| Automation | Official Rex CLI/API, 37 native actions, client scripts, merged shortcuts/modes/sequences, ordered Lua/block actions and real results | Recorded shortcut settings and nested mutating callbacks |
| Verification | Native isolated Rex smoke plus existing regression/unit gates | Manual desktop interaction, accessibility and visual fidelity checks |

## Functional milestones

The order below groups dependencies and keeps behavior independent of the final
visual design. Each milestone needs native isolated tests plus targeted desktop
checks; source implementation alone does not establish complete parity with the
reference application.

| Order | Milestone | Completion checks |
| --- | --- | --- |
| 1 | Client actions and configuration | Implemented: 37 strict declarations, shared dispatch/validated targets, FIFO callbacks/results, generation rejection and bounded transport. Merged configuration bindings/removals, Lua/block `client.action` and its returned native actions, startup `client.set_script`, sequence prefixes, stacked/exclusive/one-shot modes and diagnostics now reach the UI. Keys, pointer focus and layout commands queue behind action chains; new-pane typing waits for readiness, buffered modal input stays in rename. Physical keys, shifted symbols, configurable Quit and a command palette are implemented in the current integration; recorded shortcut settings and nested mutating callbacks remain. Broader Wayland/IME and recovery checks still needed. |
| 2 | Terminal focus and mouse input | Implemented: ordered `block.event` focus-in/out, mouse buttons/drag/hover by requested mode, Shift local selection, outside release and held-button cleanup. Native fixtures cover modes 1000/1002/1003/1004, modifiers, coordinates, mode-only changes and held gestures across reconnection. The isolated X11 desktop fixture exercises actual pointer/focus routing. Broader Wayland/IME and protocol coverage remains ongoing. |
| 3 | Pane and tab management | Implemented move/swap and tab reorder through shared core commands and declared actions, a pane Arrange chooser and sidebar tab dragging (drop before a tab or onto New Tab for the end). Isolated native checks cover shell PIDs/output/owners, same/cross-tab moves, closing an empty source tab, focus/zoom, exact unaffected split IDs/ratios, no-op reorder, invalid targets, replica convergence and reconnect aliases. Actual X11 chooser/drag checks pass. Detached discovery, new-tab restoration, placement beside a tiled anchor and explicit closure are implemented; their current validation status is below. Richer placement gestures and broader accessibility/Wayland checks remain. |
| 4 | Selection, copy, search and metadata | Implemented native persistent selection across local history, autoscroll, logical soft-wrap/Unicode copy and incremental literal search with older/newer navigation. Verified generation cancellation, isolated replicas, same-instance query restoration, exact data resume, exited history, native/API and desktop clipboard routing. Hyperlinks and program clipboard events are implemented in the current integration. Dense-search worker optimization is recorded below; broader interaction checks remain. |
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
Physical key resolution now receives Linux XKB position metadata from the vendored
GPUI patch; shifted punctuation retains raw modifiers. Client script paths are
explicit startup options, not an inferred reference-client configuration convention.
Nested callbacks needing ordered mutations during Lua execution are refused to
prevent a cycle. The pending operation pump services existing streams and local
selection copies while awaiting Lua; newly queued mutations remain ordered.

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

The styled CJK follow-up now retries unresolved slanted clusters with suitable
upright faces, retaining bold weight, whole graphemes and unaffected glyph geometry.
The fallback is intentionally upright; synthetic italic and broad mixed-script
fidelity remain open. Normal rows avoid probing, resolved styled rows skip retry
partitioning, and paint-only changes retain cached geometry.

The full integration gate passes 269 tests and all smoke executables. Isolated
X11 traces and pixel checks verify regular/italic/bold/bold-italic CJK, nonmissing
replacement glyphs, two-cell width and preservation of neighboring text, combining,
emoji, Cyrillic and existing Tamil fallback glyphs. The prior cursor/decoration
presentation fixture also passes. The fixture does not establish actual ligature
shaping or the precise family/style of the Tamil fallback face. Evidence is kept
locally under `docs/audits/rex-20261028/styled-cjk/`.

An untraced dense guard uses two 51×35 panes and three redraws of 30 rows. Recorded
fallback-frame preparation CPU is approximately 0.95–7.27 ms, versus 0.23–1.37 ms
for regular rows; the largest recorded fallback slice is 6.28 ms. These are
rate-limited preparation samples with repeated row content, not display/input
latency, a historical speed comparison, or a bound on diverse cold text. Fallback
still performs a second shape for unresolved rows; dense search and broader
rendering responsiveness remain separate follow-up work.

The dense-search worker follow-up reuses owned visible/selected spans during
history-only progress and avoids redundant feeds on due visible refreshes. Mutation,
query/navigation, viewport and visibility changes still force fresh native spans.
The full gate passes 271 tests and all smoke checks. A matched two-visible-pane
stress run measured input-to-cache p50/p95 39.43/94.22 ms versus 59.44/143.31 ms
before this change; cancellation remained about 9.5 ms. The final search completed
with 755,440 matches and preserved the expected input and layout invariants.
These are single-run workload observations, not display latency or a general bound.
Query replacement still took roughly 95–169 ms, and synchronous native search work
remains open. Exact inputs and results are retained locally in
`docs/audits/rex-20261028/search-worker/`.

The rebuilt isolated X11 search fixture also passes: keyboard/native actions,
Unicode/soft-wrap and cross-history copy, Shift override, release/blur cleanup and
unchanged server input/size/owner/PID. Broader Wayland/IME and general desktop load
remain unverified.

## Integration follow-up (2026-10-29)

The current implementation adds floating-layer rendering and move/resize/stacking
controls, a registry-backed command palette and configurable Quit, Linux native
physical/shifted-symbol shortcut metadata, explicit TCP/TLS endpoints, OSC 8
hyperlink activation, authoritative program-clipboard writes, and a pending Lua
operation pump. The pump keeps existing terminal streams and nested local copy
responsive while preserving ordering for mutations. The combined `mise run verify`
gate passes formatting, all-target compilation, strict Clippy, 329 tests and all
Ghostty, input, PTY, private-server, CLI and native Rex smoke checks.
`mise run test:core` separately passes all 34 tests. Earlier pass counts above
refer to their recorded revisions.

Isolated X11 desktop checks pass floating move/resize/stacking, palette and Quit,
German physical-key and shifted-symbol bindings, OSC 8 activation, and inline
and chunked program clipboard writes. Graphics checks pass native snapshot
restoration, raw RGBA and PNG, crop and stacking, replacement/deletion, enlarged
one-pixel opacity, Shift-wheel history, animation and reattachment with unchanged
shell PID. Local evidence is retained under
`docs/audits/rex-20261029/floating-ui/` and
`docs/audits/rex-20261029/graphics-ui/`; the captured API-7 graphics snapshot is
tracked in `tests/fixtures/graphics-rex-api7.bin` with provenance alongside it.
These checks do not establish exhaustive graphics protocol or compositor parity.

Remaining scope includes nested mutating callbacks, nonterminal creator renderers,
recorded shortcut settings, semantic accessibility, and real compositor/IME
interaction coverage.
Stream handshakes, recovery and post-action refresh still contain synchronous
worker work; the pending-operation pump does not make the entire worker async.
The GPUI patch preserves text delivery but synthetic tests do not establish real
IME behavior. TLS uses system trust and hostname verification; interactive login
and Tailscale account selection are not implemented.

Named-server routing and bounded Kitty graphics rendering are implemented in this
integration. Native snapshot support is backported without changing the wrapper
revision. The saved creator inventory contains terminal and onboarding;
onboarding exposes state/set_step but supplies no walkthrough content or assets,
so its faithful rendering remains a reference-evidence gap.

An isolated API-7 probe confirmed why nested layout/input callbacks remain refused:
while `client.action` waited on its callback, `session.set_label` completed, but a
valid `session.new_split` remained queued. The outer action timed out at about
5,003 ms and the split then mutated the workspace at about 5,004 ms. Admitting
such requests on the originating connection would therefore risk a mutation
after the action reported failure. Local selection-copy callbacks are independent
of this server queue and are covered by the native smoke test.
