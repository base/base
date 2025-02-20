# Quick Start Guide: OP Succinct Fault Dispute Game

This guide provides the fastest path to try out OP Succinct fault dispute games by deploying contracts and running a proposer to create games.

## Prerequisites

- [Foundry](https://book.getfoundry.sh/getting-started/installation)
- [Rust](https://www.rust-lang.org/tools/install)
- L1 and L2 RPC endpoints
- ETH on L1 for deployment and bonds

## Step 1: Deploy Contracts

1. Clone and setup the repository:
```bash
git clone https://github.com/succinctlabs/op-succinct.git
cd op-succinct
forge install
```

2. Create a `.env` file in the contracts directory:
```env
# example .env file

# Required - set these values
GAME_TYPE=42
DISPUTE_GAME_FINALITY_DELAY_SECONDS=604800
MAX_CHALLENGE_DURATION=604800
MAX_PROVE_DURATION=86400

# For testing, use mock verifier
USE_SP1_MOCK_VERIFIER=true
```

3. Deploy contracts:
```bash
cd contracts
forge script script/fp/DeployOPSuccinctFDG.s.sol --broadcast --rpc-url <L1_RPC_URL> --private-key <YOUR_PRIVATE_KEY>
```

Save the output addresses, particularly the `FACTORY_ADDRESS` output as "Factory Proxy: 0x..."

## Step 2: Run the Proposer

1. Create a `.env.proposer` file in the fault_proof directory:
```env
# Required Configuration
L1_RPC=<YOUR_L1_RPC_URL>
L2_RPC=<YOUR_L2_RPC_URL>
FACTORY_ADDRESS=<FACTORY_ADDRESS_FROM_DEPLOYMENT>
GAME_TYPE=42
PRIVATE_KEY=<YOUR_PRIVATE_KEY>
```

2. Run the proposer:
```bash
cargo run --bin proposer
```

## Step 3: Monitor Games

1. The proposer will automatically create new games at regular intervals (every 1800 blocks with the default config)
2. You can view created games on a block explorer using the factory address and the game address in the proposer logs
3. The proposer will also attempt to resolve unchallenged games after the challenge period expires

## Troubleshooting

Common issues:
- **Deployment fails**: Check RPC connection and ETH balance
- **Proposer won't start**: Verify environment variables and addresses
- **Games not creating**: Check proposer logs for errors and L1,L2 RPC endpoints

For detailed configuration and advanced features, see:
- [Full Deployment Guide](./deploy.md)
- [Proposer Documentation](./proposer.md)
