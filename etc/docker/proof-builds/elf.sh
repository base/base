#!/bin/sh
set -eu

cd /root/program
separator=$(printf '\037')
export CARGO_ENCODED_RUSTFLAGS="-C${separator}passes=lower-atomic${separator}-C${separator}link-arg=--image-base=2013265920${separator}-C${separator}panic=abort${separator}--cfg${separator}getrandom_backend=\"custom\"${separator}-C${separator}llvm-args=-misched-prera-direction=bottomup${separator}-C${separator}llvm-args=-misched-postra-direction=bottomup"
sysroot=$(RUSTUP_TOOLCHAIN=succinct rustc --print sysroot)
export RUSTC="$sysroot/bin/rustc"
export CARGO_TARGET_DIR=/root/program/target/elf-compilation/docker
export RUSTUP_TOOLCHAIN=succinct
export RUSTC_BOOTSTRAP=1
export CFLAGS_riscv32im_succinct_zkvm_elf=-D__ILP32__

cd crates/proof/zk/programs/succinct/range/ethereum
cargo build --release --ignore-rust-version --target riscv64im-succinct-zkvm-elf
mkdir -p /root/program/crates/proof/zk/programs/succinct/elf
cp "$CARGO_TARGET_DIR/riscv64im-succinct-zkvm-elf/release/range" \
    /root/program/crates/proof/zk/programs/succinct/elf/range-elf-embedded
cd /root/program/crates/proof/zk/programs/succinct/aggregation
cargo build --release --ignore-rust-version --target riscv64im-succinct-zkvm-elf
cp "$CARGO_TARGET_DIR/riscv64im-succinct-zkvm-elf/release/aggregation" \
    /root/program/crates/proof/zk/programs/succinct/elf/aggregation-elf
