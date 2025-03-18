# Deploying OP Succinct Fault Dispute Game

This guide explains how to deploy the OP Succinct Fault Dispute Game contracts using the `DeployOPSuccinctFDG.s.sol` script.

## Overview

The deployment script performs the following actions:
1. Deploys the `DisputeGameFactory` implementation and proxy.
2. Deploys the `AnchorStateRegistry` implementation and proxy.
3. Deploys a mock `OptimismPortal2` for testing.
4. Deploys the `AccessManager` and configures it for permissionless games.
5. Deploys either a mock SP1 verifier for testing or uses a provided verifier address.
6. Deploys the `OPSuccinctFaultDisputeGame` implementation.
7. Configures the factory with initial bond and game implementation.

## Prerequisites

- [Foundry](https://book.getfoundry.sh/getting-started/installation) installed.
- Access to an Ethereum node (local or network).
- Environment variables properly configured.

## Configuration

Create a `.env` file in the contracts directory with the following variables:

### Required Environment Variables

| Variable | Description | Example |
|----------|-------------|---------|
| `GAME_TYPE` | Unique identifier for the game type (uint32). | `42` |
| `DISPUTE_GAME_FINALITY_DELAY_SECONDS` | Delay before finalizing dispute games. | `604800` for 7 days |
| `MAX_CHALLENGE_DURATION` | Maximum duration for challenges in seconds. | `604800` for 7 days |
| `MAX_PROVE_DURATION` | Maximum duration for proving in seconds. | `86400` for 1 day |
| `STARTING_L2_BLOCK_NUMBER` | Starting L2 block number in decimal. | `786000` |
| `STARTING_ROOT` | Starting anchor root in hex. | `0x...` |

#### Getting the Starting Root

You can get the starting root for the `STARTING_L2_BLOCK_NUMBER` from the L2 node RPC using this command:

```bash
# Convert block number to hex and remove '0x' prefix
BLOCK_HEX=$(cast --to-hex <STARTING_L2_BLOCK_NUMBER> | sed 's/0x//')

# Construct the JSON RPC request
JSON_DATA='{
    "jsonrpc": "2.0",
    "method": "optimism_outputAtBlock",
    "params": ["0x'$BLOCK_HEX'"],
    "id": 1
}'

# Make the RPC call and extract the output root
starting_root=$(curl -s -X POST \
    -H "Content-Type: application/json" \
    <L2_NODE_RPC> \
    --data "$JSON_DATA" \
    | jq -r '.result.outputRoot')

# Display the result
printf "\nStarting root: %s\n" "$starting_root"
```

### SP1 Verifier Configuration
For testing, set:
```bash
USE_SP1_MOCK_VERIFIER=true
```

For production, remove the `USE_SP1_MOCK_VERIFIER` environment variable and set all of these:

| Variable | Description | Example |
|----------|-------------|---------|
| `VERIFIER_ADDRESS` | Address of the SP1 verifier ([see contract addresses](https://docs.succinct.xyz/docs/sp1/verification/onchain/contract-addresses)) | `0x...` |
| `ROLLUP_CONFIG_HASH` | Hash of the rollup configuration | `0x...` |
| `AGGREGATION_VKEY` | Verification key for aggregation | `0x...` |
| `RANGE_VKEY_COMMITMENT` | Commitment to range verification key | `0x...` |

#### Getting the Rollup Config Hash, Aggregation Verification Key, and Range Verification Key Commitment

First, create a `.env` file in the root directory with the following variables:
```bash
L1_RPC=<L1_RPC_URL>
L1_BEACON_RPC=<L1_BEACON_RPC_URL>
L2_RPC=<L2_RPC_URL>
L2_NODE_RPC=<L2_NODE_RPC_URL>
```

You can get the aggregation program verification key, range program verification key commitment, and rollup config hash by running the following command:

```bash
cargo run --bin config --release -- --env-file <PATH_TO_ENV_FILE>
```

## Deployment

1. Change directory to contracts:
   ```bash
   cd contracts
   ```

2. Install dependencies:
   ```bash
   forge install
   ```

3. Build the contracts:
   ```bash
   forge build
   ```

4. Run the deployment script:
   ```bash
   forge script script/fp/DeployOPSuccinctFDG.s.sol --broadcast --rpc-url <RPC_URL> --private-key <PRIVATE_KEY>
   ```

## Optional Environment Variables

The deployment script deploys the contract with the following parameters:

| Variable | Description | Example |
|----------|-------------|---------|
| `INITIAL_BOND_WEI` | Initial bond for the game. | 1_000_000_000_000_000 (for 0.001 ETH) |
| `CHALLENGER_BOND_WEI` | Challenger bond for the game. | 1_000_000_000_000_000 (for 0.001 ETH) |

Use `cast --to-wei <value> eth` to convert the value to wei to avoid mistakes.

These values depend on the L2 chain, and the total value secured. Generally, to prevent frivolous challenges, `CHALLENGER_BOND` should be set to at least 10x of the proving cost needed to prove a game.

## Post-Deployment

After deployment, the script will output the addresses of:
- Factory Proxy.
- Game Implementation.
- SP1 Verifier.
- OptimismPortal2.
- Anchor State Registry.
- Access Manager.

Save these addresses for future reference and configuration of other components.

## Security Considerations

- The deployer address will be set as the factory owner.
- Initial parameters are set for testing - adjust for production.
- The mock SP1 verifier (`USE_SP1_MOCK_VERIFIER=true`) should ONLY be used for testing.
- For production deployments:
  - Provide a valid `VERIFIER_ADDRESS`.
  - Configure proper `ROLLUP_CONFIG_HASH`, `AGGREGATION_VKEY`, and `RANGE_VKEY_COMMITMENT`.
  - Review and adjust finality delay and duration parameters.
  - Consider access control settings.

## Troubleshooting

Common issues and solutions:

1. **Compilation Errors**:
   - Ensure Foundry is up to date (run `foundryup`).
   - Run `forge clean && forge build`.

2. **Deployment Failures**:
   - Check RPC connection.
   - Verify sufficient ETH balance.
   - Confirm environment variables are set correctly.

## Next Steps

After deployment:

1. Update the proposer configuration with the factory address.
2. Configure the challenger with the game parameters.
3. Test the deployment with a sample game.
4. Monitor initial games for correct behavior.
5. For production: Replace mock OptimismPortal2 with the real implementation.
