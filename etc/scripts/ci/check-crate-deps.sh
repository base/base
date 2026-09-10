#!/usr/bin/env bash
set -euo pipefail

exec python3 "$(dirname "$0")/check-crate-deps.py"
