#!/usr/bin/env bash
# THROWAWAY one-off measurement kit — P2P block-latency measurement.
# Installs and enables chrony on Ubuntu/Debian, then prints the current clock
# offset so the operator can confirm sub-few-ms sync. Cross-observer latency is
# only as trustworthy as the worst clock, so this is a gate: do not open the
# measurement window on a node whose offset is not small and stable.
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "Re-running with sudo..." >&2
  exec sudo -E "$0" "$@"
fi

if ! command -v apt-get >/dev/null 2>&1; then
  echo "ERROR: this script targets apt-based Ubuntu/Debian only." >&2
  exit 1
fi

export DEBIAN_FRONTEND=noninteractive

echo "==> Installing chrony"
apt-get update -y
apt-get install -y chrony

# On Debian/Ubuntu the service is 'chrony'; some minimal images name it 'chronyd'.
SERVICE="chrony"
if ! systemctl list-unit-files | grep -q '^chrony\.service'; then
  if systemctl list-unit-files | grep -q '^chronyd\.service'; then
    SERVICE="chronyd"
  fi
fi

echo "==> Enabling and starting ${SERVICE}"
systemctl enable "${SERVICE}"
systemctl restart "${SERVICE}"

echo "==> Forcing a quick step so we can read a meaningful offset"
# makestep: step the clock if off by any amount, for the next 3 updates.
chronyc makestep >/dev/null 2>&1 || true
# Give chrony a moment to talk to sources.
sleep 5

echo
echo "==> chronyc tracking (confirm 'System time' offset is sub-few-ms):"
chronyc tracking

echo
echo "==> chronyc sources:"
chronyc sources -v || true

echo
echo "NOTE: If 'System time' offset is large or unstable, wait a minute and re-run"
echo "      'chronyc tracking'. Do NOT open the measurement window until the offset"
echo "      is small (target < a few ms) and stable across a couple of checks."
