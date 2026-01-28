#!/usr/bin/env bash
set -euo pipefail

fail=0

check_url() {
  local name="$1"
  local value="${!name:-}"
  if [[ -z "$value" ]]; then
    return 0
  fi

  if [[ "$value" =~ [[:space:]] ]]; then
    echo "ERROR: $name contains whitespace"
    fail=1
    return 0
  fi

  if [[ ! "$value" =~ ^https?:// ]]; then
    echo "ERROR: $name must start with http:// or https://"
    fail=1
    return 0
  fi
}

# Add vars here if this repo uses them in env files/scripts
check_url MAINNET_RPC_URL
check_url TESTNET_RPC_URL

exit "$fail"
