# Deploying `OPSuccinctL2OutputOracle`

Similar to the `L2OutputOracle` contract, the `OPSuccinctL2OutputOracle` is managed via an upgradeable proxy. Follow the instructions below to deploy the contract.

## 1. Pull the version of `OPSuccinctL2OutputOracle` you want to deploy

Check out the latest release of `op-succinct` from [here](https://github.com/succinctlabs/op-succinct/releases).

## 2. Configure your environment

First, ensure that you have the correct environment variables set in your `.env` file. See the [Configuration](./configuration.md) section for more information.


## 3. Deploy `OPSuccinctL2OutputOracle`

To deploy the `OPSuccinctL2OutputOracle` contract, run the following command in `/contracts`.

```bash
just deploy-oracle
```

Optionally, you can pass the environment file you want to use to the command.

```bash
just deploy-oracle .env.example
```

This will deploy the `OPSuccinctL2OutputOracle` contract using the parameters in the `.env.example` file.

You will see the following output. The contract address that should be used is the proxy address. In the logs below, the proxy address is `0xa8A51b0a66FF2ee852a633cC2D59B6C1b47c7f00`.

```shell
% just deploy-oracle .env.example

warning: op-succinct-scripts@0.1.0: fault-proof built with release-client-lto profile at 2024-12-07 01:24:00
warning: op-succinct-scripts@0.1.0: range built with release-client-lto profile at 2024-12-07 01:24:00
warning: op-succinct-scripts@0.1.0: native_host_runner built with release profile at 2024-12-07 01:24:01
    Finished `release` profile [optimized] target(s) in 0.40s
     Running `target/release/fetch-rollup-config --env-file .env.example`
[⠊] Compiling...
No files changed, compilation skipped
Script ran successfully.

== Return ==
0: address 0xa8A51b0a66FF2ee852a633cC2D59B6C1b47c7f00

## Setting up 1 EVM.

==========================

Chain 11155111

Estimated gas price: 2.524425396 gwei

Estimated total gas used for script: 3225968

Estimated amount required: 0.008143715545883328 ETH

==========================

##### sepolia
✅  [Success]Hash: 0x0bdc8571277951abcc91759086d0b9e092354a14b4180b0182883a0ee2185833
Contract Address: 0xa8A51b0a66FF2ee852a633cC2D59B6C1b47c7f00
Block: 7246470
Paid: 0.0005939866343301 ETH (439035 gas * 1.35293686 gwei)


...
                                                                                                                                                                       

==========================

ONCHAIN EXECUTION COMPLETE & SUCCESSFUL.
##
Start verification for (2) contracts
Start verifying contract `0x476130149cD3828b8d8A9bb57eBf1B8A54592539` deployed on sepolia

...

```
