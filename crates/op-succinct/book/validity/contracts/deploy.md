# Deploying `OPSuccinctL2OutputOracle`

Similar to the `L2OutputOracle` contract, the `OPSuccinctL2OutputOracle` is managed via an upgradeable proxy. Follow the instructions below to deploy the contract.

## Prerequisites

- [Foundry](https://book.getfoundry.sh/getting-started/installation)
- Configured RPCs. If you don't have these already, see [Node Setup](../../advanced/node-setup.md) for more information.

## 1. Pull the version of `OPSuccinctL2OutputOracle` you want to deploy

Check out the latest release of `op-succinct` from [here](https://github.com/succinctlabs/op-succinct/releases).

## 2. Configure your environment

First, ensure that you have the correct environment variables set in your `.env` file. See the [Environment Variables](./environment.md) section for more information.

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

You will see the following output. The contract address that should be used is the proxy address.

```shell
% just deploy-oracle .env.example
    Finished `release` profile [optimized] target(s) in 0.40s
     Running `target/release/fetch-rollup-config --env-file .env.example`
[⠊] Compiling...
No files changed, compilation skipped
Script ran successfully.

== Return ==
0: address 0xa8A51b0a66FF2ee852a633cC2D59B6C1b47c7f00

...
```

In the logs above, the proxy address is `0xa8A51b0a66FF2ee852a633cC2D59B6C1b47c7f00`.
