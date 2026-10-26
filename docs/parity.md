# Rex parity tracker

The target is Rex-like behavior on Linux, including a local backend. We are
reimplementing the application, not translating the compiled Mac binaries.
The inspected DMG contains a macOS Go server plus a native Swift UI; it does
not provide Linux executables or the original source.

Rex's public [CLI documentation](https://www.superlogical.com/rex/docs/automate/cli)
defines sessions containing windows/tabs, with layouts of terminal blocks.
The server retains work independently of attached clients. Its
[keyboard documentation](https://www.superlogical.com/rex/docs/customize/keyboard-shortcuts)
also describes pane zoom, resizing and configurable actions. These are reference
behaviors, not evidence of protocol compatibility with this implementation.

| Area | Current implementation | Remaining work |
| --- | --- | --- |
| Sessions, tabs and panes | Opaque IDs, independent tab layouts, focus, create/close, rename | Durable backend state and labels |
| Splits | Nested left/right and top/bottom splits, sibling collapse, zoom | Drag resizing, directional focus and moving panes |
| Local terminals | Real PTYs, default shell, bounded worker queues, exit status | Per-session launch profiles and working-directory inheritance |
| Terminal state | Ghostty VT engine, screen frames, colors, graphemes, cursor | Full decorations, graphics and optimized drawing |
| Input | Keyboard encoder, Unicode commits, IME hooks, paste encoder | Broader keyboard-protocol/IME and terminal application testing |
| Viewport | Measured font metrics, PTY resize and scrollback | Search, selection across history, soft-wrap-aware copy |
| Clipboard | Visible selection copy, bracketed paste | Primary selection, richer copy/link handling |
| Process lifecycle | Asynchronous reaping and verified shell/foreground group cleanup | Daemon ownership across client disconnects |
| Persistence | Background tabs continue within the running application | Server process, attach/reconnect, desktop restarts |
| Automation | Internal typed commands | User CLI, machine-readable API, Lua configuration/actions |
| Remote and shared work | Not implemented | Transport, authentication, multiple clients and sharing |
| Visual fidelity | Native GPUI workspace, sidebar, tabs and terminal panels | Compare against running Rex screenshots and refine spacing/styles |

## Next workstreams

1. Put the existing terminal workspace behind a local server process. Keep PTYs
   and Ghostty state on their existing owner, with bounded IPC commands and owned
   snapshots. A disconnect must not run pane-close cleanup.
2. Add attach/reconnect tests: start a long-running shell, disconnect the UI,
   reconnect, and verify the same process, terminal state, tabs and labels.
3. Add a user-facing CLI using the same server commands, then persistence of
   server metadata and configuration. Do not call re-created shells a restored
   live session.
4. Add split ratios/directional navigation, terminal search and improved copy.
5. Evaluate remote protocol and collaboration separately. No official Rex
   server access is required for local behavioral parity; actual Rex protocol
   compatibility requires additional evidence and testing.

The integration gate is `mise run verify`. Live desktop interaction tests are
manual; stress and process-lifecycle checks run through the PTY smoke executable.
