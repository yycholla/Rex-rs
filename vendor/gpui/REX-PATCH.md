# Rex native Linux keyboard metadata patch

This is the crates.io `gpui` **0.2.2** package, copied unchanged except for
the files listed below.
Original crate SHA-256:
`979b45cfa6ec723b6f42330915a1b3769b930d02b2d505f9697f8ca602bee707`.
The original licenses, package version, `.cargo-checksum.json` and
`.cargo_vcs_info.json` are retained. The checksum file describes the original
published package, not this modified path dependency.

`KeyDownEvent` gains optional `NativeKey` metadata. This is an additive public
field but requires callers using struct literals to initialize `native_key`.
Existing `Keystroke` matching, hashing, text, and modifier normalization are
unchanged. Synthetic/replayed and macOS/Windows events set metadata to `None`.
Only X11/Wayland native keyboard events obtain metadata, from the active XKB
keymap/state. Wayland repeats preserve the originating event metadata. IME
commit events never fabricate hardware positions.

The metadata contains the actual XKB key name, active-layout level-zero symbol,
and modifiers before printable-symbol Shift removal. The application translates
recognized XKB names to physical binding syntax; unknown names have no candidate.
The metadata is not an additional text emission path.

The keyboard changes touch eight upstream source files: the event declaration, Linux shared helper,
X11/Wayland producers, and constructors in window/macOS/Windows code. The shared
Linux module contains a test using installed US/French XKB layouts. Application
mapping tests also cover shifted punctuation and absent/unknown metadata.

A toolchain compatibility correction in `src/taffy.rs` adds explicit `f32`
suffixes to existing grid-repeat literals, avoiding Rust 1.99 fallback warnings
without changing their values or layout behavior.

Modified source files:

- `src/interactive.rs`
- `src/window.rs`
- `src/platform/linux/platform.rs`
- `src/platform/linux/x11/client.rs`
- `src/platform/linux/wayland/client.rs`
- `src/platform/mac/events.rs`
- `src/platform/mac/window.rs`
- `src/platform/windows/events.rs`
- `src/taffy.rs`

The Linux Blade polychrome fragment shader clamps sampling to the image tile's
texel centers. This prevents transparent atlas padding from fading a stretched
one-pixel Kitty image. Interior bilinear filtering and image geometry remain
unchanged. Additional modified source: `src/platform/blade/shaders.wgsl`.
