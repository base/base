# `base`

Minimal scaffolding for the unified Base node binary.

The current implementation only does four things:

- parses the public `base` CLI surface for `--chain` and `node rpc`
- initializes workspace-standard logging
- initializes the Prometheus recorder when metrics are enabled
- logs `Hello, I'm running this chain` with the resolved chain config

Supported CLI forms:

```text
base node rpc
base --chain sepolia node rpc
base -c sepolia node rpc
base --chain zeronet node rpc
base --chain ./chain.toml node rpc
base -c ./chain.toml node rpc
```

Chain selection currently supports:

- built-in names: `mainnet`, `sepolia`, `zeronet`
- TOML files with optional fields:
- TOML files for custom chains:

```toml
name = "custom-chain"
l2_chain_id = 84532
l1_chain_id = 11155111
```
