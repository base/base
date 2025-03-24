# OptimismPortal2 Support

## Overview

If you want to use `OptimismPortal2` or conform to Optimism's `IDisputeGame`, you can follow this section that describe how to use the `OPSuccinctDisputeGame` on the `DisputeGameFactory` contract.

* `OPSuccinctDisputeGame` a thin wrapper around `OPSuccinctL2OutputOracle` that implements `IDisputeGame`

## Deploying the `OPSuccinctDisputeGame` contract

After having deployed the `OPSuccinctL2OutputOracle` contract, `L2OO_ADDRESS` should now be set with the address of the `OPSuccinctL2OutputOracle` contract in your `.env` file.

Run the following to deploy the `OPSuccinctDisputeGame` contract:

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

## Setting up 1 EVM.

==========================

Chain 3151908

Estimated gas price: 1.000000014 gwei

Estimated total gas used for script: 1614671

Estimated amount required: 0.001614671022605394 ETH

==========================
```

In these deployment logs, `0x6B3342821680031732Bc7d4E88A6528478aF9E38` is the address of the proxy for the `DisputeGameFactory` contract.

## Usage

Once the contract is deployed, you have to add a new variable `DGF_ADDRESS` to your `.env` file with the address above (ex. `DGF_ADDRESS=0x6B3342821680031732Bc7d4E88A6528478aF9E38`).

With this environment variable set, the proposer will create and initialize a new `OPSuccinctDisputeGame` contract on every proposal that wraps the `OPSuccinctL2OutputOracle` contract.
