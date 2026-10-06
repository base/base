#!/bin/sh
set -e

# Setup generates upgrade-signal.env before nodes start; it is unavailable when Compose parses env_file.
if [ -f /genesis/l2/upgrade-signal.env ]; then
  set -a
  # shellcheck disable=SC1091
  . /genesis/l2/upgrade-signal.env
  set +a
else
  echo "missing /genesis/l2/upgrade-signal.env; starting without runtime upgrade signal" >&2
fi

exec /app/base "$@"
