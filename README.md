# Rex-rs

A native Linux terminal workspace in Rust, with GPUI for the interface and
[libghostty-vt](https://github.com/Uzaaft/libghostty-rs) for terminal emulation.
This is an independent implementation. Rex server compatibility is not implemented.

## Development

```sh
mise trust
mise install
mise run verify
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
| `mise run test` | Run unit tests plus Ghostty, input-protocol and real-PTY checks |
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
  ui/
    actions.rs       Keyboard actions and bindings
    theme.rs         Semantic colors, dimensions and terminal metrics
    components/      Buttons, session rows, pane headers, empty/status states,
                     reusable Unicode text fields and a styled terminal surface
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
The default shell comes from an executable absolute `$SHELL`, with `/bin/sh` as
fallback. It starts in the application's working directory. Each pane owns a real
PTY; hidden tabs and sessions continue running while the app remains open.

| Shortcut | Action |
| --- | --- |
| Ctrl+Shift+N | New session |
| Ctrl+Shift+T | New tab |
| Ctrl+PageUp / PageDown | Previous / next tab |
| Ctrl+Shift+D / E | Split right / below |
| Ctrl+Tab | Focus the next pane |
| Ctrl+Shift+Z | Zoom / unzoom the focused pane |
| Ctrl+Shift+C / V | Copy selection / paste |
| Ctrl+Shift+R | Rename session |
| Ctrl+Alt+Shift+R | Rename tab |
| Ctrl+Shift+W | Close focused pane |
| Ctrl+Alt+Shift+W | Close selected tab |
| Ctrl+Alt+W | Close selected session |
| Ctrl+Shift+Q | Quit |

Click a session or tab to select it, click a terminal to focus it, and drag to
select text. Scroll to browse history. Closing a tab's last pane closes that
tab; closing a session's last tab closes that session. Rename dialogs support
Unicode text, Enter to save and Escape to cancel. Keyboard shortcuts for the
workspace are suspended while a rename dialog is open.

## Backend and current limits

Shell transport, terminal parsing and GPUI presentation are separate modules.
IO workers use bounded queues. Closing a pane cleans up its shell and verified
foreground process group without waiting for child exit on the UI thread.
Terminal responses go back to the PTY, and measured viewport changes resize
both the terminal state and the PTY. The Ghostty encoder handles key/paste modes.

This is not full Rex parity yet. The current backend lives inside the desktop
process: quitting closes its PTYs, and sessions do not survive restart. A local
server with attach/reconnect is the next major backend milestone. Official Rex
server protocol compatibility is not implemented, and the macOS server bundled
in the DMG cannot run natively on Linux.

Selection is currently viewport-local, and copied rows do not yet distinguish
soft wraps from explicit newlines. Mouse reporting to terminal applications,
full graphics/decorations, terminal search, resizable split ratios, Lua config,
CLI automation, remote access and collaboration remain work ahead.

See [the parity tracker](docs/parity.md) for the implementation boundary and next
workstreams. Core and text-field unit tests plus the terminal integration smoke
executables are included in `mise run verify`. Desktop interaction checks on
Wayland cover typing, Unicode, tabs, splits, focus and zoom. Real IME composition
and full terminal application compatibility still need broader manual testing.
