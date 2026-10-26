#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
test_binary="$(mktemp "${TMPDIR:-/tmp}/rex-core-tests.XXXXXX")"
trap 'rm -f -- "$test_binary"' EXIT
rustc --edition=2024 --forbid unsafe_code --test src/core/mod.rs -o "$test_binary"
"$test_binary"
