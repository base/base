#!/usr/bin/env bash
# Owner kill-reset attestation signing. The owner runs this script directly.
set -euo pipefail
export PATH="$HOME/.foundry/bin:$PATH"

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <engagement_epoch> <nonce>" >&2
  exit 2
fi

EPOCH="$1"
NONCE="$2"
if ! [[ "$EPOCH" =~ ^[0-9]+$ && "$NONCE" =~ ^[0-9]+$ ]]; then
  echo "engagement_epoch and nonce must be unsigned decimal integers" >&2
  exit 2
fi

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
RESET_CONTEXT="$(
  cargo run --quiet --manifest-path "$ROOT_DIR/Cargo.toml" \
    -p base-kill-reset-bin -- --prepare "$EPOCH" "$NONCE"
)"
mapfile -t RESET_LINES <<< "$RESET_CONTEXT"
if [ "${#RESET_LINES[@]}" -ne 2 ]; then
  echo "kill-reset binary returned an invalid preparation response" >&2
  exit 1
fi
MSG="${RESET_LINES[0]}"
EXPECT_ADDR="${RESET_LINES[1]}"
echo "  message: $MSG"

echo "== [1/3] derive and compare owner address =="
GOT_ADDR="$(cast wallet address --interactive)"
echo "  derived: $GOT_ADDR"
echo "  expect : $EXPECT_ADDR"
if [ "${GOT_ADDR,,}" != "${EXPECT_ADDR,,}" ]; then
  echo "  owner address mismatch; aborting" >&2
  exit 1
fi

echo "== [2/3] sign =="
SIG="$(cast wallet sign --interactive "$MSG")"

echo "== [3/3] verify =="
cast wallet verify --address "$EXPECT_ADDR" "$MSG" "$SIG"
SIG_HEX="${SIG#0x}"
if ! [[ "$SIG_HEX" =~ ^[0-9a-f]{130}$ ]]; then
  echo "cast returned a non-canonical signature; aborting" >&2
  exit 1
fi

echo ""
echo "================ KILL-RESET SIGNATURE ================"
echo "$SIG_HEX"
echo "======================================================"
