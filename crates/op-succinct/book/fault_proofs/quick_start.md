# Quick Start Guide: OP Succinct Fault Dispute Game

This guide provides the fastest path to try out OP Succinct fault dispute games by deploying contracts and running a proposer to create games.

```admonish note
If your integration involves alternative data availability solutions like
Celestia or EigenDA, you may need to configure additional environment variables.
Refer to the [`Experimental Features`](./experimental/experimental.md) section
for the required setup steps.
```

## Prerequisites

- [Foundry](https://book.getfoundry.sh/getting-started/installation)
- [Rust](https://www.rust-lang.org/tools/install) (latest stable version)
- [just](https://github.com/casey/just)
- L1 and L2 archive node RPC endpoints
  - L2 node should be configured with SafeDB enabled. 
  - See [SafeDB Configuration](./best_practices.md#safe-db-configuration) for more details.
- ETH on L1 for:
  - Contract deployment
  - Game bonds (configurable in factory)
  - Challenge bonds (proof rewards)
  - Transaction fees

## Step 1: Deploy Contracts

1. Clone and setup the repository:

    ```bash
    git clone https://github.com/succinctlabs/op-succinct.git
    cd op-succinct/contracts
    forge install
    ```

2. Create a `.env` file in the project root. See [this guide](deploy.md#prerequisites) for more details on these environment variables:

    ```env
    # example .env file
    L1_RPC=<YOUR_L1_RPC_URL>
    L2_RPC=<YOUR_L2_RPC_URL>
    L2_NODE_RPC=<YOUR_L2_NODE_RPC_URL>
    PRIVATE_KEY=<YOUR_PRIVATE_KEY>

    # Required - set these values
    GAME_TYPE=42
    DISPUTE_GAME_FINALITY_DELAY_SECONDS=604800
    MAX_CHALLENGE_DURATION=604800
    MAX_PROVE_DURATION=86400

    # Optional

    # Not needed by default, but could be required for integrations that access consensus-layer data.
    L1_BEACON_RPC=<L1_BEACON_RPC_URL>

    # Warning: Setting PERMISSIONLESS_MODE=true allows anyone to propose and challenge games. Ensure this behavior is intended for your deployment.
    # For a permissioned setup, set this to false and configure PROPOSER_ADDRESSES and CHALLENGER_ADDRESSES.
    PERMISSIONLESS_MODE=true

    # For testing, use mock verifier
    OP_SUCCINCT_MOCK=true
    ```

    ```admonish info
    Obtaining a Test Private Key

    - Anvil (local devnet): Run `anvil` and use one of the pre-funded accounts printed on startup. Copy the Private Key value for any account. Only use these on your local Anvil network.

    - Foundry (generate a fresh key): Run `cast wallet new` to generate a human-readable output. Save the private key and fund it on your test network.

    ⚠️ **Caution:** Never use test keys on mainnet or with real assets.
    ```

3. Deploy an SP1 mock verifier:

    ```bash
    just deploy-mock-verifier
    ```  

4. Deploy the core fault dispute game contracts:

    ```bash
    just deploy-fdg-contracts
    ```

Save the output addresses, particularly the `FACTORY_ADDRESS` output as "Factory Proxy: 0x..."

## Step 2: Run the Proposer

1. Create a `.env.proposer` file in the project root directory:

    ```env
    # Required Configuration
    L1_RPC=<YOUR_L1_RPC_URL>
    L2_RPC=<YOUR_L2_RPC_URL>
    L2_NODE_RPC=<YOUR_L2_NODE_RPC_URL>
    FACTORY_ADDRESS=<FACTORY_ADDRESS_FROM_DEPLOYMENT>
    GAME_TYPE=42
    PRIVATE_KEY=<YOUR_PRIVATE_KEY>
    MOCK_MODE=true # Set to true for mock mode
    ```

    ```admonish note
    If your integration requires access to consensus-layer data, set the
    `L1_BEACON_RPC` (L1 Beacon Node). This is optional and not required by
    default.
    ```

2. Run the proposer:

    ```bash
    cargo run --bin proposer
    ```

## (Optional) Run with fast finality mode

1. Configure more environment variables to the `.env.proposer` file in the project root directory:

    ```env
    FAST_FINALITY_MODE=true
    NETWORK_PRIVATE_KEY=0x...
    ```

    For the Succinct Prover Network setup, see the [quickstart guide](https://docs.succinct.xyz/docs/sp1/prover-network/quickstart).

2. Run the proposer:

    ```bash
    cargo run --bin proposer
    ```

## Step 3: Run the Challenger

1. Create a `.env.challenger` file in the project root directory:

    ```env
    # Required Configuration
    L1_RPC=<YOUR_L1_RPC_URL>
    L2_RPC=<YOUR_L2_RPC_URL>
    FACTORY_ADDRESS=<FACTORY_ADDRESS_FROM_DEPLOYMENT>
    GAME_TYPE=42
    PRIVATE_KEY=<YOUR_PRIVATE_KEY>
    ```

2. Run the challenger:

    ```bash
    cargo run --bin challenger
    ```

## Monitoring

- Games are created every 1800 blocks by default (via `PROPOSAL_INTERVAL_IN_BLOCKS` in `.env.proposer`. See [Optional Environment Variables for Proposer](./proposer.md#optional-environment-variables))
- Track games via block explorer using factory/game addresses and tx hashes from logs
- Both proposer and challenger attempt to resolve eligible games after challenge period

## Troubleshooting

Common issues:

- **Deployment fails**: Check RPC connection and ETH balance
- **Proposer won't start**: Verify environment variables and addresses
- **Games not creating**: Check proposer logs for errors and L1,L2 RPC endpoints

For detailed configuration and advanced features, see:

- [Full Deployment Guide](./deploy.md)
- [Proposer Documentation](./proposer.md)
- [Challenger Documentation](./challenger.md)
