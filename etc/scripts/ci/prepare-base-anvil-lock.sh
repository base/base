#!/usr/bin/env bash
set -euo pipefail

# Historical base-anvil snapshots carry a lockfile for their original Base dependency.
# CI replaces selected Base crates with the current checkout, whose compatible dependency
# floors can be newer. Updating the committed historical lockfile would break standalone
# builds of that snapshot, so resolve the mixed graph only in CI's temporary clone.

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <base-anvil-dir> <base-workspace-dir>" >&2
  exit 2
fi

base_anvil_dir="$(cd "$1" && pwd)"
base_workspace_dir="$(cd "$2" && pwd)"
lockfile="$base_anvil_dir/Cargo.lock"

if [[ ! -f "$lockfile" ]]; then
  echo "error: missing base-anvil lockfile at $lockfile" >&2
  exit 1
fi

locked_version() {
  local package="$1"

  awk -v package="$package" '
    $0 == "[[package]]" {
      name = ""
      next
    }
    /^name = "/ {
      name = $0
      sub(/^name = "/, "", name)
      sub(/"$/, "", name)
      next
    }
    name == package && /^version = "/ {
      version = $0
      sub(/^version = "/, "", version)
      sub(/"$/, "", version)
      print version
      exit
    }
  ' "$lockfile"
}

update_locked_package() {
  local package="$1"
  local historical_version="$2"
  local required_version="$3"
  local current_version

  current_version="$(locked_version "$package")"
  case "$current_version" in
    "$historical_version")
      (
        cd "$base_anvil_dir"
        cargo update -p "$package@$historical_version" --precise "$required_version"
      )
      ;;
    "$required_version")
      echo "$package is already locked to $required_version"
      ;;
    *)
      echo "error: expected $package $historical_version or $required_version, found ${current_version:-missing}" >&2
      exit 1
      ;;
  esac

  current_version="$(locked_version "$package")"
  if [[ "$current_version" != "$required_version" ]]; then
    echo "error: expected $package $required_version after update, found ${current_version:-missing}" >&2
    exit 1
  fi
}

update_locked_package "c-kzg" "2.1.7" "2.1.8"
update_locked_package "alloy-genesis" "2.0.5" "2.3.0"

patch_args=(
  --config "patch.\"https://github.com/base/base.git\".base-common-precompiles.path=\"$base_workspace_dir/crates/common/precompiles\""
  --config "patch.\"https://github.com/base/base.git\".base-common-chains.path=\"$base_workspace_dir/crates/common/chains\""
)
host_target="$(rustc -vV | awk '$1 == "host:" { print $2 }')"
if [[ -z "$host_target" ]]; then
  echo "error: unable to determine the Rust host target" >&2
  exit 1
fi

# Let Cargo reconcile the temporary lockfile with the current Base path overrides,
# then prove subsequent build commands cannot mutate it.
(
  cd "$base_anvil_dir"
  cargo "${patch_args[@]}" metadata --format-version 1 --filter-platform "$host_target" >/dev/null
)
metadata_file="$(mktemp)"
trap 'rm -f "$metadata_file"' EXIT
(
  cd "$base_anvil_dir"
  cargo "${patch_args[@]}" metadata \
    --format-version 1 \
    --filter-platform "$host_target" \
    --locked >"$metadata_file"
)

verify_manifest_path() {
  local package="$1"
  local expected_path="$2"
  local actual_path

  actual_path="$(jq -r --arg package "$package" \
    '.packages[] | select(.name == $package) | .manifest_path' "$metadata_file")"
  if [[ "$actual_path" != "$expected_path" ]]; then
    echo "error: $package resolved from $actual_path instead of $expected_path" >&2
    exit 1
  fi
}

verify_manifest_path \
  "base-common-precompiles" \
  "$base_workspace_dir/crates/common/precompiles/Cargo.toml"
verify_manifest_path \
  "base-common-chains" \
  "$base_workspace_dir/crates/common/chains/Cargo.toml"

if [[ "$(locked_version "c-kzg")" != "2.1.8" ]]; then
  echo "error: path resolution changed c-kzg away from 2.1.8" >&2
  exit 1
fi
if [[ "$(locked_version "alloy-genesis")" != "2.3.0" ]]; then
  echo "error: path resolution changed alloy-genesis away from 2.3.0" >&2
  exit 1
fi

unexpected_changes="$(
  git -C "$base_anvil_dir" diff --name-only |
    while IFS= read -r path; do
      [[ "$path" == "Cargo.lock" ]] || printf '%s\n' "$path"
    done
)"
if [[ -n "$unexpected_changes" ]]; then
  echo "error: preparing base-anvil changed files other than Cargo.lock:" >&2
  printf '%s\n' "$unexpected_changes" >&2
  exit 1
fi

echo "prepared temporary base-anvil lockfile:"
echo "  c-kzg $(locked_version "c-kzg")"
echo "  alloy-genesis $(locked_version "alloy-genesis")"
git -C "$base_anvil_dir" diff --stat -- Cargo.lock
