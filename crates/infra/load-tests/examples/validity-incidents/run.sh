#!/usr/bin/env bash
# Runs one validity-incident scenario against Base Sepolia: the flood plus measured user traffic.
#
#   SEPOLIA_RPC=https://... ./run.sh s1-resting-flood-15s
#   SEPOLIA_RPC=https://... ./run.sh baseline     # user traffic only
#
# The funder key is read from FUNDER_KEY, or else from FUNDER_KEY_FILE (default
# ~/.sepolia-funder). The funder must be a plain EOA, not an EIP-7702 delegated account.
#
# The flood funds its senders first; user traffic starts once flooding begins so the two
# never share the funder's nonce. Results go to results/<scenario>-<utc timestamp>/.
set -euo pipefail

scenario="${1:?usage: run.sh <scenario|baseline>}"
: "${SEPOLIA_RPC:?SEPOLIA_RPC is required}"
: "${FUNDER_KEY_FILE:=$HOME/.sepolia-funder}"
if [ -z "${FUNDER_KEY:-}" ]; then
    [ -r "$FUNDER_KEY_FILE" ] || { echo "set FUNDER_KEY or put the funder key in $FUNDER_KEY_FILE" >&2; exit 1; }
    FUNDER_KEY="$(tr -d ' \n\r' < "$FUNDER_KEY_FILE")"
fi
export FUNDER_KEY

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../../../../.." && pwd)"
bin="${LOAD_TESTER_BIN:-$repo/target/release/base-load-tester}"
[ -x "$bin" ] || (cd "$repo" && cargo build --release -p base-load-tester-bin)

out="$here/results/$scenario-$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$out"
render() { sed "s#__SEPOLIA_RPC__#$SEPOLIA_RPC#g" "$here/$1" > "$out/$1"; }

render user-traffic-sepolia.yaml
if [ -n "${USER_DURATION:-}" ]; then
    sed -i.bak "s/^duration: .*/duration: \"$USER_DURATION\"/" "$out/user-traffic-sepolia.yaml"
    rm -f "$out/user-traffic-sepolia.yaml.bak"
fi
flood_pid=""
# Logs echo the endpoint, which may embed a key; scrub it from results on exit.
redact() {
    grep -rl -F "$SEPOLIA_RPC" "$out" 2>/dev/null | while read -r f; do
        python3 -c 'import sys; p, s = sys.argv[1], sys.argv[2]; t = open(p).read(); open(p, "w").write(t.replace(s, "<redacted-rpc>"))' "$f" "$SEPOLIA_RPC"
    done
}
cleanup() { [ -n "$flood_pid" ] && kill "$flood_pid" 2>/dev/null || true; redact; }
trap cleanup EXIT

if [ "$scenario" != "baseline" ]; then
    render "$scenario.yaml"
    date -u +%FT%TZ > "$out/flood-start"
    "$bin" validity-flood "$out/$scenario.yaml" --output "$out/flood-summary.json" \
        > "$out/flood.log" 2>&1 &
    flood_pid=$!
    until grep -q "flood progress" "$out/flood.log"; do
        kill -0 "$flood_pid" 2>/dev/null || { cat "$out/flood.log"; exit 1; }
        sleep 2
    done
    echo "flood running; starting user traffic (logs: $out)"
fi

date -u +%FT%TZ > "$out/user-start"
LOAD_TEST_OUTPUT="$out/user-summary.json" "$bin" "$out/user-traffic-sepolia.yaml" \
    > "$out/user.log" 2>&1 || echo "user traffic exited with $?" >&2
date -u +%FT%TZ > "$out/user-end"

if [ -n "$flood_pid" ]; then
    wait "$flood_pid" || echo "flood exited with $?" >&2
    flood_pid=""
fi
date -u +%FT%TZ > "$out/end"
# The rendered configs hold the endpoint key; keep only the scenario name in results.
rm -f "$out"/*.yaml
echo "done: $out"
