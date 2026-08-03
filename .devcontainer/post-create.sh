#!/usr/bin/env bash
set -euo pipefail

if ! command -v just >/dev/null 2>&1; then
  cargo install just --version 1.51.0 --locked
fi

sp1_version=v6.3.0
sp1_version_file="$HOME/.sp1/.base-version"

if [[ ! -x "$HOME/.sp1/bin/cargo-prove" ]] ||
  [[ "$(cat "$sp1_version_file" 2>/dev/null || true)" != "$sp1_version" ]]; then
  curl --proto '=https' --tlsv1.2 -sSfL https://sp1up.succinct.xyz | bash
  "$HOME/.sp1/bin/sp1up" --version "$sp1_version"
  printf '%s\n' "$sp1_version" > "$sp1_version_file"
fi
