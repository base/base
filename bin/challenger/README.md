# `base-challenger-bin`

ZK-proof dispute game challenger binary for Base.

Parses CLI arguments, builds a validated configuration, and delegates to
`base_challenger::ChallengerService::run()` for the full service lifecycle.

## Canonical network env vars

Network endpoints can also be set with the canonical `BASE_*` names shared by the
proof services. The existing name keeps working; if both are set they must match.

| Canonical | Existing |
| --- | --- |
| `BASE_L1_ETH_RPC` | `BASE_CHALLENGER_L1_ETH_RPC` |
| `BASE_L2_ETH_RPC` | `BASE_CHALLENGER_L2_ETH_RPC` |
| `BASE_PROVER_RPC` | `BASE_CHALLENGER_ZK_RPC_URL` |
