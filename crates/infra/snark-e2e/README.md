# `base-snark-e2e`

SNARK PLONK end-to-end prover verification library.

Submits a one-block SNARK prove request to the JSON-RPC prover-service requester
API, polls until completion, and cryptographically verifies the receipt. Used by
the `base-snark-e2e` binary (K8s `CronJob`) and an ignored integration test.

Requires a running `base-prover-service` requester plus a zk-host worker that
claims SNARK jobs.

## Required environment

| Variable | Required | Purpose |
|----------|----------|---------|
| `L2_NODE_ADDRESS` | Yes | L2 execution RPC |
| `L1_NODE_ADDRESS` | Yes | L1 execution RPC (finalized check) |
| `BASE_CONSENSUS_ADDRESS` | Yes | Op-node / consensus RPC (L1 origin) |
| `PROVER_RPC_ADDR` | No (default `http://localhost:9000`) | Prover-service requester JSON-RPC |

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
