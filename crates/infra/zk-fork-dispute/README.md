# `base-zk-fork-dispute`

Anvil fork workflow that patches an invalid dispute-game intermediate root,
requests a SNARK PLONK proof from prover-service, and submits `challenge()` /
`nullify()` through the same path as `base-challenger`.

Used by the `base-zk-fork-dispute` binary. Requires a running prover-service
JSON-RPC endpoint with a zk-host worker that can prove `SnarkPlonk`.
