# Input and rendering performance

Audit reports and `docs/audits/` evidence referenced below are local development
artifacts and are not distributed with this repository.

## Native Rex client

The native-action transport change was checked with one warmed, isolated
input-to-cache run, 40 samples per grid. p50/p95 were 18.06/18.15 ms at 80×24,
23.64/23.88 ms at 220×120 and 29.53/30.53 ms at 350×160. No regression was observed
against the preceding Linux baseline; the differences do not establish a speedup.
The harness polls the cache about every millisecond and excludes GUI/compositor
presentation. Exact source/build identities and logs are retained locally under
`audits/rex-20261028/actions-parity/latency/`.

The configuration/keymap integration received the same single warmed guard run.
p50/p95 were 17.09/18.19 ms at 80×24, 23.69/24.81 ms at 220×120 and
29.43/31.52 ms at 350×160. This did not show a regression against the preceding
Linux baseline, and does not establish a speedup. It excludes the UI, compositor,
display and long-running Lua actions. Current source/build identities and results
are retained locally under `audits/rex-20261028/configuration-parity/latency/`.

The move/swap/tab-order integration received one run of the same warmed guard:
p50/p95 were 18.06/19.11 ms at 80×24, 23.69/24.77 ms at 220×120 and
30.46/31.61 ms at 350×160. No regression was observed against the Linux baseline;
this does not establish a speedup or measure GUI/display latency. Exact identities
and logs are retained locally under `audits/rex-20261028/management-parity/latency/`.

Detached-block inventory retains metadata without local terminal streams or
replica frames; restoring obtains a fresh snapshot from Rex. The 16-stream limit
applies to placed terminal blocks, while combined placed/detached inventory is
bounded to 1,024 records. Periodic sweeps start roughly every two seconds and
refresh one session per worker turn using paired view/list requests. Opening the
browser or requesting Refresh also updates its inventory. These calls share the
control/terminal worker, so large workspaces and slow inventory responses remain
part of that worker's workload.

The final detached/overview/close integration passed one run of the same warmed
guard: p50/p95 were 17.09/18.17 ms at 80×24, 23.68/24.78 ms at 220×120 and
29.52/30.85 ms at 350×160. No regression was observed; this does not establish a
speedup. The run does not measure overview-open metadata traffic, heavy inventory
churn, slow responses, GUI/compositor/display latency or noisy-neighbor contention.
Exact identities and logs are retained
under `audits/rex-20261028/detached-parity/latency/`.

Overview activity queries now use a separate nonblocking request lane on the
existing connection, with one metadata admission per worker turn after stream
service. Fake-control tests hold metadata replies pending while independently
delivering input, and cover cancellation, overload and disconnect completion.
Native desktop fixtures exercise the live metadata methods and overview input
routing; these checks are not a sustained-load latency benchmark.

Local search and selection now run on each replica's Ghostty owner thread. Query
edits coalesce by pane in a generation-tagged lane, and cancellation stays admissible
when edit slots are full. Native search advances incrementally on worker polls,
including hidden/exited terminals. Metadata-only count/selection changes wake cache
consumers without requiring a new cell-frame Arc. Hidden annotation extraction reads
native row identity and owned spans rather than copying glyph cells; hidden frame
materialization remains deferred. Visible frames are paired with annotations after
materialization. Local query/navigation/selection updates bypass the normal frame
cadence; ordinary output retains the 16 ms materialization gate.

Copy work is bounded by a 262,144-cell selection span and a 4 MiB output buffer,
with explicit errors instead of truncation. The owner worker still performs native
search ticks, annotation extraction and formatting, so these bounds do not establish
input-latency isolation under long histories or repeated copies. The native smoke
checks offscreen search, softwrap/Unicode copy, client isolation and metadata-only
publication without process input, size/owner or PID changes. A search/selection
performance guard completed on the verified source with two visible panes on the
same worker: a 350×160 producer seeded 2,000 dense rows and continued at a target
50 matching rows/second while an 80×24 echo pane received 40 sequential samples.
The query alternated between `q` and `qq` every five samples. Input-to-cache latency
was 68.89 ms p50, 140.78 ms p95 and 150.21 ms maximum. Query updates took
105.76–196.72 ms, navigation 91.15–137.14 ms and cancellation during matching output
17.93 ms. A separate catch-up step completed with 780,640 matches. Search and
navigation injected no process input; only the expected echo input was observed.

This dense workload exposes substantial synchronous worker cost. Highlight
extraction converts native match endpoints, including covering-page matches, and
native feed/tick, index destruction and copy have no hard per-call time ceiling.
At that milestone hidden searches also extracted spans before publication discarded
mismatched ones. The follow-up below removes that hidden work. Native feed/tick,
match collection and query/index replacement remain optimization candidates. The historical quiet guard is a
different workload, so this run establishes neither a matched regression nor a
speedup. It excludes GUI/compositor/display latency. The initial fixture stopped
before search because its ready predicate checked only the bottom five rows;
that failure and its all-visible-row correction are retained alongside the one
corrected measurement run in
audits/rex-20261028/search-selection-parity/ (local audit evidence).
The full gate passed 238 tests, strict Clippy and all smoke checks, and seven
isolated desktop fixtures passed on the final binary.

A follow-up now skips native highlight-list/selection extraction for hidden replicas,
while retaining status/count/generation progress and viewport identity. Reveal restores
full spans without VT bytes; hidden drag/autoscroll is stopped without clearing the
native selection, including late queued pointer events. Visible geometry removes
redundant native ordering walks and prefers viewport-relative endpoint lookup, with
Screen-space fallback preserving clipped long matches.

The same dense two-visible-pane fixture was run once on the final follow-up source.
Input-to-cache p50/p95/max were 68.93/147.50/152.14 ms; cancellation took 15.88 ms.
It completed with 773,920 matches, observed only the expected 41 echo bytes and no
producer input, and left the server layout revision unchanged. The earlier run was
68.89/140.78/150.21 ms: these single-run observations do not establish a material
visible-search improvement or a general latency guarantee. Hidden extraction is
verified absent by backend/stream counters, not by a hidden-pane latency benchmark.
Full `mise run verify` passes 244 tests and all smoke checks; targeted search,
normal-close and detach/restore desktop fixtures pass on the final binary.
CPU sampling was denied before the profiling harness launched, so no dominant
stack is established. Source-only candidates include native covering-page match-list
cloning and synchronous search/index work. No system settings were changed.
Exact source/build identities, before/after inputs, failed sampling evidence and
GUI artifacts are retained locally in
the follow-up bundle (local audit evidence).

The terminal-presentation follow-up retains native cursor/underline metadata and
paints faint, decorations and blink from cached geometry. Row/glyph identity tests
cover cursor and paint-only style changes. X11 pixel samples establish shared
text blink and steady inactive views; render diagnostics are rate limited and
are corroborating observations, not complete preparation counters. Hidden and
exited views stop their animation task. This is not a dense animated-screen FPS
guarantee.

One run of the unchanged quiet native harness (40 sequential samples per grid)
measured p50/p95 5.36/7.50 ms at 80×24, 11.85/12.98 ms at 220×120 and
17.51/18.69 ms at 350×160. The guard passed; source and library hashes remained
unchanged. Earlier guards predate interaction/input-path changes, so the lower
observations cannot be attributed to the cursor/decorations change. These are
input-enqueue-to-cache times with approximately 1 ms polling; GUI blink paint,
compositor/display, noisy neighbors and metadata load are excluded. Exact inputs
and results are retained locally in
the presentation bundle (local audit evidence).

The desktop now uses `src/rex/`. The private-IPC measurements below remain
historical; they do not measure the new transport.

The native stream worker drains at most eight messages/256 KiB with a soft 1 ms
parsing slice, and materializes changed visible frames at a 16 ms cadence. Initial
frames and explicit history scrolling bypass that delay. Admitted hidden placed
panes retain VT and history state but defer owned cell copies until revealed.
Unchanged frames retain their Arc identity, and Ghostty row IDs preserve owned rows when scrolling.
The UI reuses prepared glyph rows by identity across viewport positions; both
caches are bounded to current/prior viewport rows rather than all scrollback.

Validation includes real Rex tests of title events, independent local history,
application wheel reporting, and hidden-state materialization; pure tests cover
moved-row reuse and font/background invalidation. A desktop pointer test against
an isolated X11 Rex fixture verified visible selection, history repaint and SGR
wheel delivery to an alternate-screen program. The old input/selection canvases
were auto-positioned after the text; explicit viewport edges now keep coordinates
and highlights aligned. These checks do not establish a general GUI FPS claim.

A headless colored-output workload at 220×120 confirmed the cost was dominated
by repeated frame materialization. Batching substantially reduces that stage's
CPU work; network latency, text shaping and GPU painting remain separate costs.
Use `REX_RS_RENDER_STATS=1 mise run dev` for the existing shaping/painting counters.

### Desktop shaping latency correction (2026-10-28)

Backend batching did not resolve desktop responsiveness. An isolated X11 test
with a persistent size owner held an actual 220×120 terminal, filled it with dense
text and shaped 119 rows (24,336 visible glyphs). The old binary needed 120
preparation slices: the 1 ms budget admitted only one slow row per display refresh.
GPUI was optimized, but its Linux font dependencies still compiled at level 0.

The development profile now optimizes cosmic-text, rustybuzz, swash and ttf-parser
at level 2. Preparation yields after 4 ms or 64 changed rows. A single shaping
call can exceed that budget; preparation still keeps the preceding complete
screen visible. This retains debug symbols/checks and changes no dependency pins.

| Same dense desktop fixture | Previous binary | Updated binary |
| --- | ---: | ---: |
| Preparation CPU | 761.3 ms | 43.2 ms |
| Preparation slices | 120 | 11 |
| Cold glyph painting CPU | 68.5 ms | 69.1 ms |

The updated preparation completed in 95.2 ms wall time; warm selection painting
took 2.0 ms. First-use glyph atlas work remains a separate cost. The pointer test
confirmed selection on this dense screen. These are stage measurements from one
machine, not sustained FPS or full key-to-display measurements. Earlier 24-row
pointer fixtures established functionality but did not test large-terminal speed.

Separately, 40-sample native Rex input-to-cache tests measured p50/p95 of
18.1/19.2 ms at 80×24, 23.4/25.5 ms at 220×120 and 30.8/33.0 ms at 350×160.
Those exclude GUI work. Render diagnostics now include `prepare_wall_us` alongside
CPU time and slice count so time spent across refreshes is visible.


Measured 2026-10-26 in the development build used by `mise run dev`.
The development profile retains debug symbols and runtime checks. Application
code now uses optimization level 1; GPUI, JSON and integer formatting use level 2.
No compiler or dependency versions were changed. The original measurements below
used unoptimized application code; follow-up measurements are listed separately.

## Input and transport

The original desktop transport serialized every visible cell as verbose JSON
after any screen change. A single character or cursor movement on a 220×120
terminal sent 6,917,574 bytes, taking about 214 ms to encode and 159 ms to decode.
New row allocations also defeated cheap identity checks in the rendering cache.

The desktop now opts into compact connection-scoped row updates. The socket
worker reconstructs owned frames, retaining unchanged row/frame Arcs before
publishing the cache. Cursor changes send no text rows, and sparse updates omit
unchanged panes and workspace metadata. Baseline mismatches and malformed patches
are rejected; reconnect performs a complete compact reset. CLI `Snapshot` remains
the bounded full JSON representation.

The server wakes on queued requests instead of imposing a fixed sleep, polls PTY
output after dispatch and the client briefly polls faster after input. Queue
bounds, explicit terminal authority and non-replay of uncertain mutations remain.
The UI no longer scans complete frames for equality or walks/reconciles every pane
on each queued keystroke or idle poll.

| Development-build workload | Before | After |
| --- | ---: | ---: |
| 80×24 input → client cache, p50 | 48.63 ms | 2.14 ms |
| 80×24 input → client cache, p95 | 49.72 ms | 2.20 ms |
| 220×120 input → client cache, p50 | 387.96 ms | 2.13 ms |
| 220×120 input → client cache, p95 | 390.94 ms | 2.14 ms |
| 20 queued characters, 80×24 | 672.91 ms | 9.64 ms |
| 20 queued characters, 220×120 | 7,468.21 ms | 17.02 ms |
| 220×120 one-row update payload | 6,917,574 bytes | 6,301 bytes |
| 220×120 cursor-only payload | 6,917,574 bytes | 356 bytes |
| 220×120 complete reset payload | 6,917,574 bytes | 714,190 bytes |

These are 20-sample local measurements, not universal latency guarantees. Input
timings use a real PTY running `stty raw -echo; ...; exec cat`, a background
`WorkspaceClient`, and one-millisecond cache checks. They end when echoed output
reaches the client cache, excluding GPUI/compositor/display latency. Codec data
uses a filled synthetic screen. Large complete redraws still cost more than
single-row changes, and busy hidden panes still participate in updates.

Reproduce with:

```sh
mise exec -- cargo run --locked --no-default-features --features ghostty --bin server-bench
```

`--legacy-codec` selects the old full-JSON codec measurements for comparison.
Timing diagnostics are separate from the integration gate; regression tests use
payload sparsity, row identity and correctness rather than machine-specific
timing thresholds.

## Graphical painting

The custom glyph painter initially bypassed GPUI's `Window::paint_layer`, which
its native text painter uses. Each glyph consequently entered the scene's spatial
ordering tree separately. A real Wayland probe at 208×116, with roughly 23,500
visible glyphs and changing row colors, sampled **645–1,327 ms** of CPU painting.

Terminal rows now batch backgrounds, glyphs and underlines in separate painting
layers, preserving their draw order while avoiding per-glyph ordering queries.
With this batching and optimized GPUI in the dev profile, the same workload
sampled **2.686–2.689 ms** of CPU painting. Those original measurements used
unoptimized application code. These are sampled paint-stage CPU durations, not total frame times
or GPU/display-latency measurements; cold font/atlas work and full-row preparation
can add time.

Prepared rows and their containing vector remain cached across cursor/selection
repaints. Unchanged row identity avoids text/cell scans. Glyph colors are prepared
once; offscreen glyphs and decoration spans are culled conservatively, retaining
combining-mark/ligature overhang and unknown-bound fallbacks.

The isolated probe used a private server/runtime/state directory, plain `/bin/sh`,
a three-second dense colored workload, Unicode output and guarded typing into
only its own window. The final terminal state contained the Unicode line and
executed typing marker; the window and server were closed afterward.

Inspect preparation and painting counters with:

```sh
REX_RS_RENDER_STATS=1 mise run dev
```

Reports include reused/prepared rows, shaped lines, painted/culled glyph counts,
`prepare_cpu_us` and `paint_cpu_us`, sampled about once per second. Normal rendering
does not run the profiling counter loop.

## Remaining debug overhead and update scheduling

A second pass found that optimized GPUI alone left substantial application/JSON
overhead. On the same 220×120 synthetic full-screen benchmark, a fresh comparison
measured encoding at 21.81 ms and decoding at 30.62 ms before the profile changes;
afterward they measured 10.19 ms and 4.21 ms. The 20-character queued-input test
fell from 15.94 ms to 7.41 ms; single-character input-to-cache stayed near 2.12 ms.
These remain stage measurements, excluding presentation/display latency.

The UI previously checked its cache on another independent 16 ms timer. It now
awaits worker notifications for frames, errors and connection/control changes,
removing that extra timer delay and its publication-rate cap. Empty idle replies
do not wake the UI. Continuous terminal output uses a 4 ms worker polling interval,
returning to 16 ms after 100 ms of inactivity; input retains its 1 ms interval.
Old unconsumed snapshots are freed outside the mailbox lock.

Color/decor-only row changes reuse positioned glyph geometry as well as shaping.
Text, width, font style, invisibility and metrics still invalidate it. Geometry
retention adds bounded storage for one geometry record per visible glyph. Cluster
lookup now uses a linear byte map. The isolated Wayland probe passed typing and
Unicode checks with these changes; a full frame/display-latency comparison has
not been measured.

## Busy-pane containment

The server now defers hidden frame conversion as well as hidden IPC text updates.
Hidden terminals continue parsing output and retaining history; revealing a pane
refreshes its frame. Explicit CLI snapshots refresh every dirty pane. PTY service
has a global 32 KiB budget, 8 KiB per-pane slices and a soft 4 ms time budget,
with focused-first and rotating background service. Unconsumed bytes remain
queued with backpressure, and exit still follows final output.

Visible panes stage preparation independently, yielding after roughly 4 ms or
64 changed rows. They display the last complete screen while work is pending;
newer targets coalesce. Selection/copy/IME follow that displayed screen. A single
shaping call may exceed the budget, and final painting still uses GPUI's UI
thread. This contains work; it does not provide hard thread/process isolation.

An isolated stress test flooded a 220×120 pane while a separate 80×24 pane echoed
input. With a hidden neighbor, p50 input-to-cache fell from approximately 32 ms
before deferred frame conversion to 2.12 ms afterward. The same final run measured:

| Noisy neighbor | Echo p50 | Echo p95 | 20 queued characters |
| --- | ---: | ---: | ---: |
| Hidden session | 2.12 ms | 2.13 ms | 6.36 ms |
| Visible split | 29.58 ms | 30.63 ms | 294.89 ms |

Visible busy panes still share Ghostty frame-conversion and connection codec
work. Dedicated terminal owner workers and independent publication lanes would
be needed for stronger isolation of those costs. These figures exclude display
latency, use 20 samples and are diagnostics rather than test thresholds.

```sh
mise exec -- cargo run --locked --no-default-features --features ghostty --bin server-bench -- --contention
```

An isolated Wayland check also opened a new tab with the keyboard and executed
typed input there while the original terminal continuously redrew changing text.
Both the new pane and exact output marker were verified. `REX_RS_RENDER_STATS`
now reports `terminal-prepare` slice count, total CPU time and maximum slice time.

## Updating an already running server

Incremental frames use local protocol version 3. Close old desktop windows and,
after saving live shell work, explicitly stop the previous server before running
the updated desktop:

```sh
mise exec -- cargo run --locked --no-default-features --features ghostty --bin rex-server -- --shutdown
mise run dev
```

The shutdown command can negotiate the preceding version 2 for this upgrade.
It is never automatic: stopping the server terminates its shells/jobs. Saved
workspace metadata recovers layouts and labels with fresh shells, not live jobs.

Final validation passed `mise run verify`: formatting, all-target compilation,
strict Clippy, **87 unit tests**, and all five Ghostty/input/PTY/server/CLI smoke
executables. Timing benchmarks and the isolated Wayland workload were additional
diagnostics rather than pass/fail timing thresholds.
