#!/usr/bin/env bash
# RUSTC_WRAPPER that compiles chosen crates with SanitizerCoverage inline 8-bit counters, for
# `base-diff-adapter --features edge-coverage` and `base-diff-node`. Every other crate is compiled
# unchanged.
#
# SANCOV_CRATES is a space-separated list of crate-name patterns (shell globs over the rustc
# crate name, with `_` for `-`). The default is the EVM crates (revm and Base's EVM and precompile
# crates). Never match a proc-macro crate: it would not link.
rustc=$1
shift
patterns=${SANCOV_CRATES:-"revm revm_* base_common_evm base_common_precompiles"}
crate=
prev=
for arg in "$@"; do
  if [ "$prev" = --crate-name ]; then
    crate=$arg
    break
  fi
  prev=$arg
done
if [ -n "$crate" ]; then
  set -f # split the patterns without matching them against file names
  for pattern in $patterns; do
    # shellcheck disable=SC2254 # the pattern is a glob on purpose
    case "$crate" in
      $pattern)
        exec "$rustc" "$@" -Cpasses=sancov-module \
          -Cllvm-args=-sanitizer-coverage-level=3 \
          -Cllvm-args=-sanitizer-coverage-inline-8bit-counters
        ;;
    esac
  done
fi
exec "$rustc" "$@"
