#!/usr/bin/env bash
# Ubuntu 24.04 dependencies for GPUI's Linux backends and native Ghostty build.
set -euo pipefail
sudo apt-get update
sudo apt-get install --yes --no-install-recommends \
  build-essential clang cmake libclang-dev pkg-config \
  libasound2-dev libfontconfig1-dev libfreetype6-dev libssl-dev \
  libwayland-dev libx11-xcb-dev libxcb1-dev libxkbcommon-dev \
  libxkbcommon-x11-dev libvulkan1 mesa-vulkan-drivers \
  fonts-dejavu-core ncurses-bin
