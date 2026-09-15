#!/usr/bin/env bash
# Build the pinned contracts and package everything Forge needs offline.
set -euo pipefail
cd "$(dirname "$0")/../.."
revision=6a6add446c30639e41e39ed5d65797f438b55595
version="$(forge --version)"
[[ "$version" == $'forge Version: 1.8.1\n'* ]] || { echo 'Install Forge 1.8.1' >&2; exit 1; }
mkdir -p build/genesis
output="$(cd build/genesis && pwd -P)"
project="$output/contracts"
source="${1:-}"
if [[ -n "$source" ]]; then
  source="$(cd "$source" && pwd -P)"
  [[ "$source" != "$project" && "$source" != "$project/"* && "$project" != "$source/"* ]] || {
    echo 'Local source and build output must be separate directories' >&2; exit 1;
  }
  revision="$(git -C "$source" rev-parse HEAD)"
fi
# A failed rebuild must not leave a valid-looking manifest or stale artifacts.
rm -f "$output/manifest.json"
rm -rf "$project"
mkdir -p "$project"
if [[ -n "$source" ]]; then
  tar -C "$source" --exclude=.git --exclude=.gitmodules --exclude=lib \
    --exclude=forge-artifacts --exclude=cache --exclude=deployments --exclude=.tools -cf - . |
    tar -C "$project" -xf -
fi
cd "$project"
git init -q
if [[ -z "$source" ]]; then
  git fetch --depth 1 https://github.com/base/contracts.git "$revision"
  git checkout --detach FETCH_HEAD
fi
# Keep dependency Git metadata: Forge 1.8.1's --no-git breaks nested submodules.
read -r -a dependencies <<< "$(sed -n '/^deps: clean-lib$/,/^$/p' justfile | awk '/github.com\// {printf "%s ", $1}')"
((${#dependencies[@]})) || { echo 'Missing pinned contracts dependencies' >&2; exit 1; }
forge install "${dependencies[@]}"
export FOUNDRY_PROFILE=default
export XDG_DATA_HOME="$project/.tools"
forge build --force --skip '/**/test/**'
# SVM prefers an existing legacy cache over XDG_DATA_HOME.
if [[ ! -d .tools/svm ]]; then
  cache="$HOME/.svm"
  [[ -d "$cache" ]] || cache="$HOME/Library/Application Support/svm"
  mkdir -p .tools
  cp -R "$cache" .tools/svm
fi
mkdir -p deployments
hash=(sha256sum)
command -v sha256sum >/dev/null || hash=(shasum -a 256)
find src interfaces scripts lib test forge-artifacts .tools foundry.toml \
  -name .git -prune -o -type f -exec "${hash[@]}" {} + |
  jq -Rn --arg revision "$revision" --arg forge_version "$version" '
    [inputs | capture("^(?<hash>[a-f0-9]{64})  (?<path>.*)$") | {key: .path, value: .hash}]
    | {revision: $revision, forge_version: $forge_version, files: from_entries}
  ' > "$output/manifest.json.tmp"
mv "$output/manifest.json.tmp" "$output/manifest.json"
echo "Prepared genesis contracts: $output"
