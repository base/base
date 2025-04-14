# OptimismPortal2 Support

## Overview

This section demonstrates how OP Succinct can be used with `OptimismPortal2` by conforming to Optimism's `IDisputeGame`.

The `OPSuccinctDisputeGame` contract is a thin wrapper around `OPSuccinctL2OutputOracle` that implements `IDisputeGame`.

## Set dispute game to `OPSuccinctDisputeGame`

`OptimismPortal2` requires a `DisputeGameFactory` contract which manages the lifecycle of dispute games, and the current active dispute game.

To use OP Succinct with `OptimismPortal2`, you must set the canonical dispute game implementation to `OPSuccinctDisputeGame`.

### No Existing `DisputeGameFactory` (Testing)

If you don't have a `DisputeGameFactory` contract or `OptimismPortal2` setup and want to test OP Succinct with the `DisputeGameFactory` contract, follow these steps:

After deploying the `OPSuccinctL2OutputOracle` [contract](./deploy.md), set the following environment variables in your `.env` file:

| Variable | Description | Example |
|----------|-------------|---------|
| `L2OO_ADDRESS` | Address of the `OPSuccinctL2OutputOracle` contract | `0x123...` |
| `PROPOSER_ADDRESSES` | Comma-separated list of addresses allowed to propose games. | `0x123...,0x456...` |


Run the following command to deploy the `OPSuccinctDisputeGame` contract:

```shell
just deploy-dispute-game-factory
```

If successful, you should see the following output:

```
[⠊] Compiling...
[⠊] Compiling 1 files with Solc 0.8.15
[⠒] Solc 0.8.15 finished in 1.93s
Compiler run successful!
Script ran successfully.

== Return ==
0: address 0x6B3342821680031732Bc7d4E88A6528478aF9E38
```

In these deployment logs, `0x6B3342821680031732Bc7d4E88A6528478aF9E38` is the address of the proxy for the `DisputeGameFactory` contract.

### Existing `DisputeGameFactory`

If you already have a `DisputeGameFactory` contract, you must call the `setImplementation` function to set the canonical dispute game implementation to `OPSuccinctDisputeGame`.

```solidity
    // The game type for the OP_SUCCINCT proof system.
    GameType gameType = GameType.wrap(uint32(6));

    // Set the canonical dispute game implementation.
    gameFactory.setImplementation(gameType, IDisputeGame(address(game)));
```
## Use OP Succinct with `OptimismPortal2`

Once you have a `DisputeGameFactory` contract, you can use OP Succinct with `OptimismPortal2` by setting the `DGF_ADDRESS` environment variable with the address of the `DisputeGameFactory` contract in your `.env` file.

With this environment variable set, the proposer will create, initialize and finalize a new `OPSuccinctDisputeGame` contract on the `DisputeGameFactory` contract with every aggregation proof.

