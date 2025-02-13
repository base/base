# OptimismPortalV2

If you want to use `OptimismPortalV2` or conform to Optimism's `IDisputeGame`, you can follow this section that describe how to deploy the 2 contracts:

* `OPSuccinctDisputeGame` a thin wrapper around `OPSuccinctL2OutputOracle` that implements `IDisputeGame`.
* `DisputeGameFactory` the proposer entry point when creating new dispute game.

And instructions about how to configure the proposer to use them.

After having done the step 2) either in mock or full mode, with `L2OO_ADDRESS` set with the address of the `OPSuccinctL2OutputOracle` contract in your `.env` file,
run the following to deploy the contracts:

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

In order to have the proposer to use it, you have to add a new variable `DGF_ADDRESS` to your `.env` file with the value above.

If `DGF_ADDRESS` is not set, the proposer will submit the output root directly to the `OPSuccinctL2OutputOracle` contract.
