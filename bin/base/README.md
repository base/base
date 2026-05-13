# `base`

Unified Base node binary.

The `base node rpc` command starts a Base execution client and an embedded
Base consensus client in one process. Execution and consensus share one
top-level chain selector. The execution client is launched first with the
authenticated Engine API enabled over IPC, and the consensus client is then
started with its L2 engine endpoint overridden to that local IPC endpoint.

Supported CLI forms:

```text
base node rpc
base --chain base-sepolia node rpc
base -c base-sepolia node rpc
base --chain base-zeronet node rpc
base node rpc --chain dev
base --chain ./chain.toml node rpc
base -c ./chain.toml node rpc
```

Chain selection currently supports:

- built-in names from `base-common-chains`: `base`, `base-sepolia`,
  `base_sepolia`, `base-zeronet`, `dev`
- short aliases: `mainnet`, `sepolia`, `zeronet`, `devnet`
- TOML files for custom chains:

```toml
name = "custom-chain"
l2_chain_id = 84532
l1_chain_id = 11155111
```

For embedded consensus, the L2 engine endpoint is supplied by `base node rpc`.
Do not pass `--l2-engine-rpc` in unified mode. Execution networking keeps the
reth-compatible bare `--port` flag, while embedded consensus RPC uses
`--rpc.port`.
