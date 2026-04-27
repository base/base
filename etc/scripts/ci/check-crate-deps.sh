#!/usr/bin/env bash
set -euo pipefail

# Disallowed crate dependency rules: "source:target"
# Crates in crates/<source>/ must not depend on crates in crates/<target>/
DISALLOWED_DEPS=(
  "utilities:client"
  "utilities:batcher"
  "utilities:builder"
  "utilities:consensus"
  "utilities:execution"
  "client:infra"
  "utilities:infra"
  "utilities:proof"
  "utilities:succinct"
  "common:client"
  "common:batcher"
  "common:builder"
  "common:consensus"
  "common:execution"
  "common:infra"
  "common:proof"
  "common:succinct"
  "builder:infra"
  "builder:proof"
  "consensus:infra"
  "batcher:infra"
  "batcher:proof"
  "execution:infra"
  "consensus:proof"
  "execution:proof"
  "proof:infra"
)

# Allowed exceptions: "dep_name" entries here are excluded from all rules.
# These are foundational consensus protocol crates that are local path deps under crates/consensus/.
ALLOWED_DEPS=(
  "base-consensus-engine"
)

# Build a jq filter string for allowed deps
ALLOWED_FILTER=$(printf '"%s",' "${ALLOWED_DEPS[@]}")
ALLOWED_FILTER="[${ALLOWED_FILTER%,}]"

# Fetch cargo metadata once, ensuring Cargo.lock is in sync
METADATA=$(cargo metadata --format-version 1 --no-deps --locked)

FOUND_VIOLATIONS=false

for rule in "${DISALLOWED_DEPS[@]}"; do
  SOURCE="${rule%%:*}"
  TARGET="${rule##*:}"

  VIOLATIONS=$(echo "$METADATA" | jq -r --argjson allowed "$ALLOWED_FILTER" "
    [.packages[]
     | select(.manifest_path | contains(\"/crates/$SOURCE/\"))
     | . as \$pkg
     | .dependencies[]
     | select(.path)
     | select(.path | contains(\"/crates/$TARGET/\"))
     | select(.name as \$n | \$allowed | index(\$n) | not)
     | \"\(\$pkg.name) -> \(.name)\"
    ]
    | .[]
  ")

  if [ -n "$VIOLATIONS" ]; then
    echo "ERROR: Found $SOURCE -> $TARGET dependency violations:"
    echo "$VIOLATIONS" | while read -r violation; do
      echo "  - $violation"
    done
    echo ""
    FOUND_VIOLATIONS=true
  fi
done

if [ "$FOUND_VIOLATIONS" = true ]; then
  echo "Dependency rules are defined in etc/scripts/ci/check-crate-deps.sh"
  exit 1
fi

echo "All crate dependencies are valid"
