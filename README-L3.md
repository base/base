# Base-on-Base L3

This branch is a temporary fork of Base that lets Base run on top of Base as an L3 chain.

Base as an L3 differs from Base as an L2 in two ways:

* **Base-format L1 decoding** — the derivation pipeline reads Base/OP-format L1 blocks
  (`L1TxFormat::Base`, including deposit `0x7E` and EIP-8130 `0x7D` transactions), not just
  Ethereum blocks.

* **Calldata + alt-DA data-availability** — the batcher submits batches as calldata (no blobs),
  optionally uploading the bytes to an off-chain **DA server** and posting only a generic
  commitment on L1.

It also brings the **multiproof** stack used to prove the L3: a TEE prover (AWS Nitro) and a ZK
prover (SP1), a `challenger`/`proposer`, on-chain `AggregateVerifier` registration, and a
no-dispute challenger mode.

Key L3 code lives in: `crates/infra/alt-da`, `bin/da-server`, `crates/proof/tee` +
`bin/prover/nitro-host`, `crates/proof/{proposer,challenge,prover-service}`, and the Base-format
L1 provider in `crates/common/network` + `crates/consensus/providers`.

## Local devnet

```bash
just devnet up            # 3-node HA conductor cluster, dry-run prover
just devnet up-single     # single sequencer, no conductor
just devnet status        # block numbers + sync status
just devnet smoke         # smoke testing: send L1/L2 transactions
just devnet logs          # tail container logs
just devnet down          # stop and wipe devnet data
```

## Run tests

Full suite (installs nextest, builds contracts + SP1 ELFs, then runs everything):

```bash
just test
```

Docker-backed system tests only (builds the contracts image; same ≥ 8 GB memory note applies):

```bash
just devnet tests
```

**L3 smoke test** — brings up an in-process stack in the L3 profile (Base-format L1 + calldata DA)
and checks block production, L1-derived client sync, and calldata batch submission:

```bash
cargo test -p base-system-tests --test l3_smoke
```

Fast component tests for the L3 mechanisms (no Docker):

```bash
cargo test -p base-batcher-core   alt_da      # batcher alt-DA / commitment submission
cargo test -p base-proof          base_format # Base/OP + EIP-8130 L1 decoding in proof readers
cargo test -p base-challenger     no_dispute  # challenger no-dispute mode
```
