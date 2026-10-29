# Rex-rs agent guide

Build a Linux terminal workspace in Rust with GPUI and libghostty-vt.
Read [README.md](README.md) and [docs/architecture.md](docs/architecture.md) first.
The desktop uses the official Rex server through `src/rex/`; it does not host
processes or initialize sessions on attachment. Read docs/rex-server-migration.md,
docs/rex-control-protocol.md and docs/rex-terminal-protocol.md for protocol and
ownership details. The private local server remains a development fixture;
docs/local-server.md describes that legacy path.

## Boundaries

- `core/` is pure Rust state and owned data. No GPUI, Ghostty, IO, or async runtime.
- `rex/terminal.rs` owns data streams and display-replica lifetimes;
  `rex/client.rs` caches their owned frames by PaneId.
- `terminal/ghostty.rs` owns all Ghostty FFI interaction. Its handles stay on their
  owning thread; never implement unsafe Send/Sync. Share owned frames instead.
- `rex/client.rs` owns the bounded desktop cache/worker. Rex is authoritative for
  layouts and processes; local display replicas never send device query replies.
- `terminal/workspace.rs` owns legacy terminal lifetimes and frame caches by PaneId.
  `terminal/pty.rs` owns bounded IO workers and process cleanup; never block the
  UI thread on reads, writes, or child waits.
- `ui/components/` contains composable presentation. Use semantic theme tokens;
  keep workspace mutations, shell IO and process management out of components.
- Buttons and shortcuts dispatch the same core Commands. Preserve stable IDs.
- Keep the crate modular; add abstractions or dependencies for concrete needs.

## Parallel development

Use subagents for substantive work that can be split into independent tasks,
including focused review. The lead owns integration and the final report.

The lead establishes interface changes and assigns explicit file ownership before
starting agents. Prefer separate worktrees; in a shared checkout, keep write sets
disjoint. Agents should not revert or format another agent's in-progress files.

| Workstream | Typical ownership |
| --- | --- |
| Rex control transport | `src/rex/transport.rs` and `src/rex/protocol.rs` |
| Rex view adaptation | `src/rex/workspace.rs` |
| Rex terminal streams | `src/rex/terminal.rs` |
| Workspace behavior | `src/core/workspace.rs` and its tests |
| Terminal backend/transport | `src/terminal/` and `src/bin/ghostty-smoke.rs` |
| UI primitives | `src/ui/components/` and `src/ui/theme.rs` |
| Application integration | `src/ui/workspace.rs`, `src/ui/actions.rs`, `src/main.rs` |

The integration owner coordinates changes to `src/core/frame.rs`, `src/core/grid.rs`,
module exports, Cargo.toml, Cargo.lock and mise.toml. Request needed interface or
dependency changes through that owner rather than editing shared files in parallel.
Notify other owners when a contract changes. Each handoff lists changed files,
behavior, checks run, blockers, and follow-up integration work. The lead reviews
the combined result and runs the full checks after integrating.

## Checks

- `mise run verify` is the integration gate: formatting, compilation, strict Clippy,
  tests, Ghostty smoke checks and isolated native Rex interoperability, run
  sequentially to avoid Cargo lock contention. Native smoke requires `rex` on PATH.
- `mise run test:core`: pure state/frame tests, no external crate downloads.
- `mise run check`: formatting and compilation of all targets.
- `mise run test`: unit tests and headless Ghostty/input/real-PTY smoke checks.
- `mise run lint`: Clippy; `mise run dev`: manual desktop verification.
- Keep Clippy `all` and `pedantic` enabled; lint warnings fail verification.
  Fix findings rather than weakening project settings. Any narrow allowance
  needs a local reason, including test-only unwraps and framework callbacks.
- Clippy's MSRV must match Cargo.toml. Unknown lint names are errors.
- Test behavioral invariants and boundary cases, not copies of implementation.
- Keep Cargo.lock once generated. Do not change toolchain/dependency pins casually.
- If dependencies or graphics are unavailable, report exactly what passed and
  what remains unverified. Never describe an uncompiled preview as working.
