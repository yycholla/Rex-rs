# Continuous integration

`CI / Linux build, lint and tests` runs on pushes to `main`, pull requests and
manual dispatch. It installs the Rust and Zig versions from `mise.toml`, builds
all targets, checks formatting and strict Clippy, and runs pure core tests, unit
tests and the headless Ghostty/input/PTY/private-server/CLI smoke executables.
`Linux release binary` builds `rex-rs` in release mode in parallel and uploads
`rex-rs-linux-x86_64` (a tarball of the binary and README, kept 14 days) to the
run's artifacts. Actions are pinned by commit; Cargo uses the committed lockfile. The Rust build
cache is saved only on `main`.

The hosted job does not run `rex-native-smoke` or graphical desktop tests. Official
[Rex Linux installation](https://www.superlogical.com/rex/docs/linux/installation)
currently requires testing-wave access, so CI does not invent a public download
or store installation credentials in the workflow.

To run native interoperability, provision a dedicated Linux x64 Actions runner
with labels `self-hosted`, `linux`, `x64`, `rex`. Install the official `rex` binary
on its PATH, the packages listed in `scripts/ci-dependencies.sh`, and the ordinary
GitHub runner prerequisites. Dispatch `CI` from `main` with `native_rex` enabled.
That job runs `mise run verify`, including the smoke suite's isolated Rex server;
it does not attach to an existing user session. Use a disposable runner without
personal sessions or credentials. Pull requests never schedule this runner.

A green hosted job establishes build/headless coverage, not native Rex or desktop
interaction coverage. The manual job fails if `rex` is absent. `mise run verify`
remains the full local integration gate.
