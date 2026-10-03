# `base-snark-e2e`

SNARK PLONK end-to-end prover verification library.

Submits a one-block SNARK prove request to the JSON-RPC prover-service requester
API, polls until completion, and cryptographically verifies the receipt. Used by
the `base-snark-e2e` binary (K8s `CronJob`) and an ignored integration test.

Requires a running `base-prover-service` requester plus a zk-host worker that
claims SNARK jobs.

Before requesting the proof it computes the range and aggregation verifying keys
and compares them with `ZK_RANGE_HASH` / `ZK_AGGREGATE_HASH` on the
`AggregateVerifier` the factory registers for `GAME_TYPE`. The verify step
checks the SNARK against those locally computed keys, so without this
comparison the test passes even when the chain would reject every proof with
`InvalidProof()`. The comparison is only meaningful when this binary and zk-host
are built from the same base commit: the keys come from this binary's ELFs, the
proofs from zk-host's.

## Required environment

| Variable | Required | Purpose |
|----------|----------|---------|
| `L2_NODE_ADDRESS` | Yes | L2 execution RPC |
| `L1_NODE_ADDRESS` | Yes | L1 execution RPC (finalized check) |
| `BASE_CONSENSUS_ADDRESS` | Yes | Op-node / consensus RPC (L1 origin) |
| `PROVER_RPC_ADDR` | No (default `http://localhost:9000`) | Prover-service requester JSON-RPC |
| `DISPUTE_GAME_FACTORY_ADDRESS` | Yes, unless skipped | `DisputeGameFactory` on L1 whose `AggregateVerifier` the computed keys are compared with |
| `GAME_TYPE` | No (default `621`) | Game type of that `AggregateVerifier` |
| `SKIP_ONCHAIN_VKEY_CHECK` | No | `true` skips the comparison; anything else runs it |

## Usage

```toml
[dependencies]
base-snark-e2e = { workspace = true }
```

```rust,ignore
use base_snark_e2e::SnarkE2e;

SnarkE2e::run().await?;
```

## Integration test

```bash
cargo nextest run --run-ignored all -p base-snark-e2e --test snark_plonk_e2e
```
