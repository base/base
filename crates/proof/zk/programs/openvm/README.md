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

## Showcase

Opens the TUI and proves one live block from the active network (zeronet by
default): `openvm-dump` fetches the witness from RPCs, `cargo openvm run`
executes the guest and reveals the boot digest, then `cargo openvm prove app`
produces the app proof. `cargo-openvm` prints no progress while proving, so the
active card pulses until it exits.

```bash
just openvm demo
# or: cargo run -p basectl -- monitor openvm
```

Press `z` from the home menu to open it on the current network. `p` restarts the
prove and `Esc` aborts it. The network needs `l1_beacon_rpc` and
`consensus_node_rpc` configured.

CLI equivalent:

```bash
just openvm dump-zeronet
just openvm prove-zeronet
```

## Run

The guest still deserializes rkyv `DefaultWitnessData`, same as Succinct. OpenVM
`read_vec` length-prefixes that payload, so the host must frame it as an OpenVM
input (`0x01` + bytes) rather than reusing an SP1 stdin blob as-is. See
https://docs.openvm.dev/book/writing-apps/overview/
