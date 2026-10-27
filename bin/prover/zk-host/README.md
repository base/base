# base-prover-zk-host

ZK prover-service worker host binary.

This binary is the CLI entry point for claiming ZK proof jobs from the prover service and running them with the requested ZK backend.

## Canonical network env vars

Network endpoints can also be set with the canonical `BASE_*` names shared by the
proof services. The existing name keeps working; if both are set they must match.

| Canonical | Existing |
| --- | --- |
| `BASE_L1_ETH_RPC` | `L1_NODE_ADDRESS` |
| `BASE_L1_BEACON_RPC` | `L1_BEACON_ADDRESS` |
| `BASE_L2_ETH_RPC` | `L2_NODE_ADDRESS` |
| `BASE_ROLLUP_RPC` | `BASE_CONSENSUS_ADDRESS` |
| `BASE_PROVER_WORKER_RPC` | `PROVER_SERVICE_ENDPOINT` |
