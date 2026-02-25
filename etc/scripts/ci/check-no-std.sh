#!/usr/bin/env bash
set -eo pipefail

# Crates that must compile in a no_std environment.
# If a crate is added with `#![cfg_attr(not(feature = "std"), no_std)]` or `#![no_std]`,
# add it here to ensure it stays no_std-compatible.
no_std_packages=(
  # alloy crates (ported from op-alloy)
  base-alloy-consensus
  base-alloy-evm
  base-alloy-hardforks
  base-alloy-rpc-types
  base-alloy-rpc-types-engine

  # consensus protocol crates (ported from kona)
  base-macros
  kona-genesis
  kona-hardforks
  kona-registry
  kona-derive
  base-protocol
)

for package in "${no_std_packages[@]}"; do
  cmd="cargo +stable build -p $package --target riscv32imac-unknown-none-elf --no-default-features"
  if [ -n "$CI" ]; then
    echo "::group::$cmd"
  else
    printf "\n%s:\n  %s\n" "$package" "$cmd"
  fi
  $cmd
  if [ -n "$CI" ]; then
    echo "::endgroup::"
  fi
done
