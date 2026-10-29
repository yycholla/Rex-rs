#!/usr/bin/env bash
# Public-runner checks. Native Rex requires a separately provisioned runner.
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
for smoke in ghostty-smoke input-smoke pty-smoke server-smoke cli-smoke; do
  cargo run --locked --no-default-features --features ghostty --bin "$smoke"
done
