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
base --chain sepolia node rpc
base -c sepolia node rpc
base --chain zeronet node rpc
base node rpc --chain sepolia
base node rpc --chain /path/to/genesis.json
base --chain ./chain.toml node rpc
base -c ./chain.toml node rpc
```

Chain selection currently supports:

- built-in names: `mainnet`, `sepolia`, `zeronet`
- TOML files for custom chains:

```toml
name = "custom-chain"
l2_chain_id = 84532
l1_chain_id = 11155111
```

- execution genesis JSON files, matching the `base-reth-node --chain
  /path/to/genesis.json` behavior

For embedded consensus, the L2 engine endpoint is supplied by `base node rpc`.
Do not pass `--l2-engine-rpc` in unified mode. Execution networking keeps the
reth-compatible bare `--port` flag, while embedded consensus RPC uses
`--rpc.port`.
