# Rex server client migration plan

Status: native desktop integration implemented on 2026-10-28. The isolated
interoperability smoke, full integration gate and Wayland desktop startup
validation pass. The remaining rendering/interaction scope is
tracked in [parity.md](parity.md). The phases below retain the implementation
plan and its acceptance criteria.

## Goal and scope

Make the GPUI desktop a client of the existing Rex server. Rex owns sessions,
layouts, processes, persistence and configuration. Reuse the desktop rendering,
selection and input presentation where its behavior matches the Rex protocol.

The first release targets the existing local Linux server. Remote connection
setup and full rendering of every block creator can follow. Existing floating
layers, empty layouts and unknown block kinds must be represented explicitly;
connecting must never reshape or delete them to fit our current model.

Rex is now the desktop default, and its executable no longer hosts or autostarts
the private backend. The custom server, storage, CLI and PTY code remain only as
development regression fixtures; they are not a second production backend.
Their final removal is separate cleanup after the new path has desktop acceptance.

## Evidence and open questions

Read-only discovery succeeded with `rex --autostart=false api call
server.version`, `rex --autostart=false whoami --json`, and `rex api describe`.
The observed build is Linux amd64, commit
`ac26dc62e1e92cc2e4eabde482e5400cf9a97961`, API revision 7. The observed local
endpoint is `unix:///run/user/1000/rex/server.sock`; discover/configure it rather
than hard-coding this user's path.

Confirmed API contracts:

- `session.bootstrap` initializes a connection and returns client identity,
  server instance identity, sessions and attached views. Apply its result before
  buffered events with a higher response-relative `seq`.
- `session.view` revisions are per session. Change notifications can arrive out
  of revision order and require fetching a newer view.
- IDs are typed strings. Views include layered layouts, nullable fields and
  fractional split ratios, beyond the current core workspace invariants.
- Block ownership arbitrates settings such as terminal size. It does not grant
  exclusive input rights. Normal attachment must explicitly use `request`, since
  `session.own_block` defaults to `take`.
- `data.connect` issues a short-lived, single-use ticket for a WebSocket at
  `/api/data`; the terminal subprotocol is `rex.data.terminal.v1`.
- Event backlog overflow closes the connection; bootstrap a fresh connection.

The initial discovery questions were resolved through the running server's
`GET /llms.txt` and native probes. They were: control connection path and framing, handshake,
revision negotiation, client resume identity, data HELLO/framing, initial screen
and history synchronization, resync, flow control and terminal query responses.
Also establish client callback/action replies, clipboard events and input/IME
payloads. A CLI subprocess for each request cannot preserve the persistent client
identity and ordered event stream needed by the desktop.
The native implementation now uses a pinned snapshot-capable Ghostty wrapper
and Zig 0.17.0. It supports control identity resume, full snapshot import and
exact byte-count data resume. Unsupported client callbacks receive prompt errors.
See the protocol notes for the implemented wire contracts.

## Phase 1: prove the protocol and freeze contracts

Lead and two discovery agents work in parallel, with read-only server access:

| Owner | Scope | Deliverable |
| --- | --- | --- |
| Lead | Integration design and shared contracts | Desktop snapshot/command/capability types and module boundaries |
| Protocol agent | Connection, bootstrap, events, auth, versioning and reconnect | Protocol notes and sanitized control fixtures |
| Terminal agent | Data handshake, content format, history, input and resize semantics | Terminal protocol notes, fixtures and decoder feasibility |

Gate: a small native Rust probe can connect to a dedicated disposable Rex test
session, obtain its view, display initial terminal content, receive new content,
and reconnect without duplicating or losing state. Discovery itself must not
create sessions, claim ownership or alter the user's current workspace. Probe
mutations are confined to clearly identified test resources, which it cleans up.

Choose the rendering path only after observing the terminal stream. If it is a
VT byte stream, establish its snapshot/replay/reset rules before feeding Ghostty.
If it carries structured terminal state, decode into owned frames instead. Plain
text `format` polling is useful for diagnostics, not the interactive data path.
Determine who answers terminal device queries so a local parser never sends
duplicate responses to a server-owned terminal.

Lead freezes these contracts before implementation agents begin:

- A pure owned desktop presentation snapshot, separate from the allocator-based
  local `Workspace` engine. It preserves Rex identities, labels, layers, empty
  states, focus, zoom, creator metadata and per-block capabilities.
- Typed remote identity handling with stable identities across same-instance
  reconnect. If numeric presentation IDs remain, use explicit lossless mappings,
  never hashes or truncated UUIDs.
- One cached desktop client boundary: command enqueueing, connection state,
  notifications, per-block status/capabilities and owned frames. IO stays off the
  GPUI thread and all queues and decoded allocations have explicit bounds.
- Ordered command translation shared by shortcuts and buttons. Selection local
  to a client is distinguished from server focus mutations. Do not predict Rex
  state by executing commands against a cloned local `Workspace`.
- Stream decoder/renderer ownership and resync behavior. Ghostty FFI remains in
  `terminal/ghostty.rs`, on its owning thread, sharing only owned frames.

## Phase 2: parallel implementation

Use three implementation agents plus the lead, matching the four available
concurrency slots. Proposed paths below become exclusive write sets when tasks
start; the lead publishes exact filenames after Phase 1. In a shared checkout,
agents must not format or revert files outside their write sets.

| Owner | Exclusive files | Work |
| --- | --- | --- |
| Agent A: control transport | New `src/rex/transport.rs`, `src/rex/protocol.rs`, transport fixtures/tests | Endpoint connection, handshake, request correlation, ordered writes, bootstrap/events, bounded queues, disconnect and reconnect |
| Agent B: workspace adapter | New `src/rex/workspace.rs`, workspace fixtures/tests | View validation, ID mapping, session revisions, command translation, ownership/capabilities, unsupported state handling |
| Agent C: terminal data | New `src/rex/terminal.rs`, terminal fixtures/tests; `src/terminal/ghostty.rs` only if required by the frozen design | Ticket/stream lifecycle, decode and resync, frame caching, input/paste/resize/history integration and response routing |
| Lead: app integration | `src/ui/workspace.rs`, `src/ui/terminal_view.rs`, `src/ui/actions.rs`, `src/main.rs`, shared contracts/exports | Backend selection, notifications, UI reconciliation, errors, ownership UI and wiring |

The lead alone owns `Cargo.toml`, `Cargo.lock`, `mise.toml`, `src/lib.rs`, module
exports, shared core IDs/types, `src/core/frame.rs` and `src/core/grid.rs`.
Agents request changes to these contracts through the lead. Dependency or
contract changes are announced to all affected owners before integration.

Input must respect Rex key/config handling (`client.key` where appropriate),
instead of bypassing Lua with locally encoded bytes for every key. Establish
ordering for keyboard, Unicode commits, IME, paste, focus and layout commands.
Scrolling/selection follow the proven data/history contract. Replace the global
controller UI with per-block size/theme ownership; never reuse it as an input
authorization rule. Remove the custom backend's metadata-recovery message from
the Rex path.

Each handoff includes changed files, behavior, checks run, unresolved questions
and required integration work. Agents may run isolated checks; the lead schedules
Cargo commands sequentially to avoid lock contention.

## Phase 3: integration and acceptance

Integrate a thin vertical slice first: attach, show an existing terminal, type,
resize, detach and reopen. Then complete workspace commands and external-change
synchronization. Use explicit connection errors rather than launching the custom
server when a Rex connection fails.

Required acceptance cases:

1. Initial attachment creates no sessions, starts no duplicate shells and does
   not take ownership from an existing Rex client. Empty servers remain empty.
2. Sessions/tabs/splits, labels, focus, zoom and resize agree with Rex CLI changes;
   stable identities preserve terminal entities through same-instance reconnect.
3. Empty windows, floating layers, detached blocks and unknown creators are
   represented safely, with unsupported interactions disclosed in the UI.
4. Interactive typing, Unicode/IME, paste, Lua keybindings, terminal modes,
   alternate screen, scrollback and selection work under the observed protocol.
5. Per-block ownership and competing sizes match Rex behavior. Attaching to one
   block does not disable unrelated terminals or steal all session blocks.
6. Desktop quit/reopen preserves server-owned shells and their PIDs. Server
   restart clears invalid caches, reboots attachment and reports actual state;
   it makes no promise of live process recovery beyond Rex's own behavior.
7. Bootstrap/event races, stale session views, dropped control/data connections,
   expired tickets, malformed/oversized frames, slow consumers and noisy panes
   remain bounded and recover without duplicate output or UI-thread blocking.
8. Mutations with uncertain delivery are reported and never blindly replayed.
   Reconnect cannot accidentally target a replacement instance with stale work.

Use protocol fixtures and a local mock peer for deterministic failure tests,
plus disposable real-Rex sessions for interoperability. Keep testing of the
user's existing workspace read-only. Run `mise run verify` sequentially after
integration and manually verify the desktop under a graphics session. Report
exactly which checks passed and any unverified behavior.

Assign an independent agent review after implementation handoffs, focusing on
protocol fidelity, lifecycle/ownership and bounded IO; it receives no overlapping
write ownership. The lead fixes findings and reruns the relevant checks.

## Phase 4: cutover and retirement

After acceptance, make Rex the desktop default, document endpoint discovery and
requirements, and update README, architecture, parity and agent instructions.
Remove the custom `--server` launch path and redundant backend modules/binaries,
metadata storage and benchmarks once their replacement coverage is in place.
Update feature/dependency boundaries and verification tasks together; preserve
useful core/rendering tests and dependency pins unrelated to this migration.

Done means the Linux desktop uses Rex for its full supported workspace lifecycle,
external Rex changes appear correctly, reconnect retains live server work, the
integration gate passes, and remaining visual/protocol limitations are explicit.

## Implementation validation (2026-10-28)

- `mise run verify` passed: formatting, all-target compilation, strict Clippy,
  110 unit tests, Ghostty/input/PTY/private-server/CLI regressions and native Rex
  interoperability.
- `mise run test:core` passed all 20 pure Rust tests.
- The native smoke starts an isolated installed Rex server and checks snapshots,
  Unicode, keys/paste, resize, ownership, authoritative external changes, shell
  lifetime, control/data resume and the actual cached desktop client. A bounded
  Unix proxy forcibly interrupts that client's control tunnel; reconnect retains
  client identity, pane aliases and shell PID and receives fresh state/output.
- The built desktop started under Wayland against a separate isolated server.
  Both terminal viewports in a one-third split submitted measured dimensions,
  and terminating the desktop left the same shell PIDs alive. This is a startup
  and viewport test, not a screenshot-based visual audit or full interaction test.
- `git diff --check` passed.

Remaining scope: floating-layer interaction, nonterminal rendering, detached-block
management, mouse reporting, remote endpoints, full client actions, real IME
interaction and broader stress/visual checks. The existing GPUI workspace
shortcuts are reserved before terminal keys reach Rex Lua. Legacy server code
remains only for regression tests; the desktop does not execute it.
