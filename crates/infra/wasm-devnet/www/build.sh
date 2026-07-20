#!/usr/bin/env bash
# Build the WASM package for the browser devnet UI and serve it locally with the
# COOP/COEP headers required for SharedArrayBuffer (needed by the atomics-enabled
# wasm32 target this crate builds for).
set -euo pipefail
cd "$(dirname "$0")/.."
wasm-pack build --target web --no-opt . --out-dir www/pkg
cd www
python3 serve.py
