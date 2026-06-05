# Base Activator

`base-activator` inspects the Beryl native precompile surface and generates activation-registry calldata.

## Commands

List Beryl precompiles and the upgrade that installs them:

```sh
cargo run -p base-activator -- list
```

Generate raw transaction data for the activation registry:

```sh
cargo run -p base-activator -- calldata activate b20-asset
cargo run -p base-activator -- calldata deactivate policy-registry
```

Check activation registry state on Base Mainnet, Base Sepolia, and Base Zeronet:

```sh
cargo run -p base-activator -- status
```

Mainnet and Sepolia use the baked-in public Base RPC URLs when no flag or environment variable is
provided. Zeronet has no baked-in public URL. If a configured or default RPC does not reach the
expected chain ID, the command falls back to a `rpc:` field in
`~/.config/base/networks/{mainnet,sepolia,zeronet}.yaml` or `.yml`, when present.

RPC URL priority is:

1. `--mainnet-rpc-url`, `--sepolia-rpc-url`, or `--zeronet-rpc-url`.
2. `BASE_MAINNET_RPC_URL`, `BASE_SEPOLIA_RPC_URL`, or `BASE_ZERONET_RPC_URL`.
3. Baked-in public URL for Mainnet and Sepolia.
4. Basectl user config in `~/.config/base/networks/`.
