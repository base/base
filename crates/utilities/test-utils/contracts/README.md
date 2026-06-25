## Foundry

**Foundry is a blazing fast, portable and modular toolkit for Ethereum application development written in Rust.**

Foundry consists of:

- **Forge**: Ethereum testing framework (like Truffle, Hardhat and DappTools).
- **Cast**: Swiss army knife for interacting with EVM smart contracts, sending transactions and getting chain data.
- **Anvil**: Local Ethereum node, akin to Ganache, Hardhat Network.
- **Chisel**: Fast, utilitarian, and verbose solidity REPL.

## Documentation

https://book.getfoundry.sh/

## Usage

### Build

```shell
$ forge build
```

### Test

```shell
$ forge test
```

### Format

```shell
$ forge fmt
```

### Gas Snapshots

```shell
$ forge snapshot
```

### Anvil

```shell
$ anvil
```

### Deploy

```shell
$ forge script script/Counter.s.sol:CounterScript --rpc-url <your_rpc_url> --private-key <your_private_key>
```

### Real-Token Swap Devnet Harness

The load-test Justfile can deploy a local WETH/USDC swap harness, render the load-test config with the deployed addresses, and run the Docker devnet swap load test:

```shell
just load-test real-token
```

To deploy only the harness for manual validation:

```shell
export FUNDER_KEY=0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d
forge script script/DeployRealTokenSwapDevnet.s.sol:DeployRealTokenSwapDevnet \
  --rpc-url http://localhost:7545 \
  --private-key "$FUNDER_KEY" \
  --broadcast
```

The script verifies the WETH predeploy at `0x4200000000000000000000000000000000000006`, deploys a standard 6-decimal mock USDC token, deploys two router shims, and seeds both shims with WETH and USDC liquidity. Use the printed USDC, Uniswap router shim, and Aerodrome router shim addresses in the devnet load-test config.

Optional liquidity overrides:

```shell
DEVNET_USDC_PER_WETH=1000000000
DEVNET_ROUTER_USDC_LIQUIDITY=100000000000
DEVNET_ROUTER_WETH_LIQUIDITY=100000000000000000000
```

### Cast

```shell
$ cast <subcommand>
```

### Help

```shell
$ forge --help
$ anvil --help
$ cast --help
```
