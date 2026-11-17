#!/usr/bin/env bash
# RUSTC_WRAPPER that compiles the EVM crates (revm and Base's EVM and precompile crates) with
# SanitizerCoverage inline 8-bit counters, for `base-diff-adapter --features edge-coverage`.
# Every other crate is compiled unchanged.
rustc=$1
shift
for arg in "$@"; do
  case "$arg" in
    revm | revm_* | base_common_evm | base_common_precompiles)
      exec "$rustc" "$@" -Cpasses=sancov-module \
        -Cllvm-args=-sanitizer-coverage-level=3 \
        -Cllvm-args=-sanitizer-coverage-inline-8bit-counters
      ;;
  esac
done
exec "$rustc" "$@"
