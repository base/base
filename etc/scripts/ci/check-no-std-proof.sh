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
# getrandom 0.3+. getrandom 0.2 (pulled in transitively by k256/revm) is not
# affected by this flag — fixing it requires upgrading those deps to 0.3+.
#
# Crates that are currently blocked (alloy-* std feature propagation through
# the dep tree prevents them from passing): base-proof, base-proof-mpt,
# base-proof-driver, base-proof-executor, base-proof-client,
# base-proof-fpvm-precompiles. Fixing these requires either upgrading to
# no_std-compatible alloy releases or cross-compiling the full client binary.

RUSTFLAGS="${RUSTFLAGS} --cfg getrandom_backend=\"custom\""
export RUSTFLAGS

proof_packages=(
  # Passes cleanly — simple dep tree, no alloy std propagation.
  base-proof-preimage
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
