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

  # consensus protocol crates
  base-macros
  base-consensus-genesis
  base-consensus-upgrades
  base-consensus-registry
  base-consensus-derive
  base-protocol

  # proof crates: only those whose dep trees are clean enough for --no-default-features
  # on a bare-metal target. Heavier proof crates (base-proof, base-proof-client, etc.)
  # depend on alloy crates that activate std via feature propagation; those are checked
  # separately via check-no-std-proof.sh using -Zbuild-std=core,alloc.
  base-proof-preimage
)

for package in "${no_std_packages[@]}"; do
  cmd="cargo build -p $package --target riscv32imac-unknown-none-elf --no-default-features"
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
