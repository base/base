#!/usr/bin/env bash
set -euo pipefail

# Disallowed crate dependency rules: "source:target"
# Crates in crates/<source>/ must not depend on crates in crates/<target>/
DISALLOWED_DEPS=(
  "shared:client"
  "shared:builder"
  "shared:consensus"
  "client:infra"
  "shared:infra"
  "builder:infra"
  "consensus:infra"
)

# Fetch cargo metadata once
METADATA=$(cargo metadata --format-version 1 --no-deps)

FOUND_VIOLATIONS=false

for rule in "${DISALLOWED_DEPS[@]}"; do
  SOURCE="${rule%%:*}"
  TARGET="${rule##*:}"

  VIOLATIONS=$(echo "$METADATA" | jq -r "
    [.packages[]
     | select(.manifest_path | contains(\"/crates/$SOURCE/\"))
     | . as \$pkg
     | .dependencies[]
     | select(.path)
     | select(.path | contains(\"/crates/$TARGET/\"))
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
