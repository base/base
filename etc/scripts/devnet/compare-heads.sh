#!/usr/bin/env bash
# Compare latest/safe/finalized block numbers between builder (sequencer) and
# client (validator) on the local devnet. Refreshes every 2 seconds.
#
# Usage: ./etc/scripts/devnet/compare-heads.sh

BUILDER=http://localhost:7545
CLIENT=http://localhost:8545

while true; do
    clear
    echo "=== builder (sequencer, no delay) ==="
    for label in latest safe finalized; do
        num=$(cast block "$label" --rpc-url "$BUILDER" 2>/dev/null | grep "^number" | awk '{print $2}')
        printf "  %-12s number %s\n" "$label" "${num:-N/A}"
    done

    echo
    echo "=== client (validator, with delay) ==="
    for label in latest safe finalized; do
        num=$(cast block "$label" --rpc-url "$CLIENT" 2>/dev/null | grep "^number" | awk '{print $2}')
        printf "  %-12s number %s\n" "$label" "${num:-N/A}"
    done

    echo
    echo "(refreshing every 2s — Ctrl-C to stop)"
    sleep 2
done
