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

KEY_FILE="$HOME/.config/mev-owner-attest/attest.key"
EXPECT_ADDR="0x581F5c5EC1d63BA08d6024E8b1cF88b83D57285b"
MSG="base-mev:p2-killreset:${EPOCH}:${NONCE}"

KEY="$(tr -d '[:space:]' < "$KEY_FILE")"

echo "== [1/3] derive and compare owner address =="
GOT_ADDR="$(cast wallet address --private-key "$KEY")"
echo "  derived: $GOT_ADDR"
echo "  expect : $EXPECT_ADDR"
if [ "$GOT_ADDR" != "$EXPECT_ADDR" ]; then
  echo "  owner address mismatch; aborting" >&2
  exit 1
fi

echo "== [2/3] sign =="
SIG="$(cast wallet sign --private-key "$KEY" "$MSG")"

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
