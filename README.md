# Rex-rs

A Linux terminal workspace scaffold in Rust, with GPUI for the interface and
[libghostty-vt](https://github.com/Uzaaft/libghostty-rs) for terminal emulation.
This is an independent implementation. Rex server compatibility is not implemented.

## Development

```sh
mise trust
mise install
mise run test:core
mise run check
mise run smoke
mise run dev
```

Rust and Zig are pinned in `mise.toml`; GPUI and the Ghostty wrapper are pinned
in `Cargo.toml`. Cargo.lock records the tested dependency versions; keep it in
version control. The first full build needs network access to Cargo packages,
Ghostty's pinned source, and Zig dependencies. Use the project's Zig 0.15.2 toolchain rather than
system Zig 0.17. The published Ghostty bindings currently build a Ghostty
revision requiring Zig 0.15.2; the repository's newer development README
describes a different toolchain.

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
| `mise run test` | Run unit tests and the Ghostty integration smoke executable |
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
  core/              Session/pane IDs, split trees, commands, validated grid sizes,
                     owned terminal frames and text-run batching
  terminal/          Ghostty adapter, terminal lifetime management, cached frames
  ui/
    actions.rs       Keyboard actions and bindings
    theme.rs         Semantic colors, dimensions and terminal metrics
    components/      Buttons, session rows, pane headers, empty/status states,
                     and a terminal preview surface
    workspace.rs     Composition, focus and command dispatch
  main.rs            Application and window bootstrap
  bin/ghostty-smoke.rs
```

See [architecture](docs/architecture.md) for ownership, mutation boundaries,
close/focus behavior, and guidelines for extending the scaffold.
See [AGENTS.md](AGENTS.md) for parallel workstreams and integration responsibilities.

## Workspace controls

Buttons and shortcuts use the same command paths:

| Shortcut | Action |
| --- | --- |
| Ctrl+Shift+N | New session |
| Ctrl+Shift+D | Split focused pane to the right |
| Ctrl+Shift+E | Split focused pane below |
| Ctrl+Tab | Focus the next pane |
| Ctrl+Shift+W | Close the focused pane |
| Ctrl+Alt+W | Close the selected session |
| Ctrl+Shift+Q | Quit |

Click a session to select it; click a pane to focus it. Closing the last pane
closes its session. When no sessions remain, the shell shows an empty state.
The core also supports renaming sessions; a rename control is not wired yet.

## Current limits and validation

Panes are explicitly previews with sample output, not interactive shells.
The adapter preserves graphemes, physical cells, colors, styles and cursor
position. The preview batches styled text runs and uses provisional monospace
metrics. It does not yet implement PTYs, measured font layout, viewport resizing,
selection, clipboard, full text decorations, image rendering or persistent sessions.

Validated with `mise run check`, `mise run test`, and `mise run lint`:
nine unit tests pass, plus the headless Ghostty smoke checks for VT parsing,
colors, erase-line handling, resizing, query responses and pane lifetimes.
The desktop preview has been launched and its initial layout visually checked
on Wayland. Complete interaction testing remains to be done.

## Next milestone

Measure terminal font metrics, connect viewport size to Ghostty resize, and add
PTY output/input with explicit process cleanup. Keep transport outside components
and keep Ghostty objects on their owning thread. Persistence and remote Rex
sessions can follow a working local terminal.
