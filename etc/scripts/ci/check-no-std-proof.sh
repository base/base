#!/usr/bin/env bash
set -eo pipefail

# Checks that proof crates compile for a bare-metal FPVM target using
# -Zbuild-std=core,alloc (nightly). This mirrors how kona builds the fault proof
# client binary: std is simply absent from the target, so any crate that
# unconditionally links it will fail.
#
# Requires:
#   rustup toolchain install nightly
#   rustup component add rust-src --toolchain nightly
#   rustup target add riscv32imac-unknown-none-elf
#
# getrandom_backend=custom silences the "target not supported" error from
# getrandom 0.3+. getrandom 0.2 (pulled in by k256 via alloy-consensus/k256)
# is handled by enabling features = ["custom"] in base-proof-executor's
# Cargo.toml, which propagates via Cargo feature unification.

RUSTFLAGS="${RUSTFLAGS} --cfg getrandom_backend=\"custom\""
export RUSTFLAGS

proof_packages=(
  base-proof-preimage
  base-proof-mpt
  base-proof-executor
  base-proof-driver
  base-proof
  base-proof-client
  base-proof-fpvm-precompiles
)

for package in "${proof_packages[@]}"; do
  cmd="cargo +nightly build -p $package -Zbuild-std=core,alloc --target riscv32imac-unknown-none-elf --no-default-features"
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
