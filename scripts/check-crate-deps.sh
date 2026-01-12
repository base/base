#!/usr/bin/env bash
set -euo pipefail

# Use cargo metadata to get structured dependency information
# --no-deps: only workspace members, not transitive deps
# --format-version 1: stable JSON format

echo "Checking that shared crates don't depend on client crates..."

# Find all violations using jq:
# 1. Select packages in crates/shared/
# 2. For each, find dependencies with paths pointing to crates/client/
# 3. Output violations as "shared_crate -> client_crate"
VIOLATIONS=$(cargo metadata --format-version 1 --no-deps | jq -r '
  [.packages[]
   | select(.manifest_path | contains("/crates/shared/"))
   | . as $pkg
   | .dependencies[]
   | select(.path)
   | select(.path | contains("/crates/client/"))
   | "\($pkg.name) -> \(.name)"
  ]
  | .[]
')

if [ -n "$VIOLATIONS" ]; then
    echo "ERROR: Found shared -> client dependency violations:"
    echo "$VIOLATIONS" | while read -r violation; do
        echo "  - $violation"
    done
    echo ""
    echo "Shared crates (crates/shared/) must not depend on client crates (crates/client/)"
    exit 1
fi

echo "Checking that shared crates don't depend on builder crates..."

# Check for shared -> builder violations
BUILDER_VIOLATIONS=$(cargo metadata --format-version 1 --no-deps | jq -r '
  [.packages[]
   | select(.manifest_path | contains("/crates/shared/"))
   | . as $pkg
   | .dependencies[]
   | select(.path)
   | select(.path | contains("/crates/builder/"))
   | "\($pkg.name) -> \(.name)"
  ]
  | .[]
')

if [ -n "$BUILDER_VIOLATIONS" ]; then
    echo "ERROR: Found shared -> builder dependency violations:"
    echo "$BUILDER_VIOLATIONS" | while read -r violation; do
        echo "  - $violation"
    done
    echo ""
    echo "Shared crates (crates/shared/) must not depend on builder crates (crates/builder/)"
    exit 1
fi

echo "All crate dependencies are valid"
