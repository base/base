# OpenVM guest programs

Local OpenVM analog of `crates/proof/zk/programs/succinct`.

The range binary is a thin I/O wrapper around `base-proof-zk-utils`. Witness
bytes and the STF are shared with the Succinct guest; only the zkVM entrypoint,
stdin read, and public-value commit differ.

Aggregation is not in this workspace. The Succinct aggregator calls
`sp1_lib::verify::verify_sp1_proof`. OpenVM's equivalent is the deferral /
verify-STARK guest library, which is a follow-up.

## Layout

```
openvm/
  range/ethereum   # guest binary (`range`)
  range/utils      # `run_range_program` + OpenVM reveal
  sp1-bls12-shim   # vanilla bls12_381 under the crate name kzg-rs expects
```

## Prerequisites

```bash
rustup install nightly-2026-01-18
rustup component add rust-src --toolchain nightly-2026-01-18
cargo +1.91 install --locked --git https://github.com/openvm-org/openvm.git --tag v2.0.2 cargo-openvm
```

## Build

```bash
just openvm build-elf
```

Artifacts land in `elf/` (gitignored), same pattern as Succinct.

## Run

Dump a live block's witness with [`openvm-dump`](../../../../../etc/tools/openvm-dump/README.md)
into `elf/`, then execute it locally and print the boot digest:

```bash
just openvm run
```

The guest still deserializes rkyv `DefaultWitnessData`, same as Succinct. OpenVM
`read_vec` length-prefixes that payload, so the host must frame it as an OpenVM
input (`0x01` + bytes) rather than reusing an SP1 stdin blob as-is. See
https://docs.openvm.dev/book/writing-apps/overview/
