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

| Parameter | Description |
|-----------|-------------|
| `L1_RPC` | L1 Archive Node. |
| `L2_RPC` | L2 Execution Node (`op-geth`). |
| `L2_NODE_RPC` | L2 Rollup Node (`op-node`). |
| `PRIVATE_KEY` | Private key for the account that will be deploying the contract. |
| `ETHERSCAN_API_KEY` | Etherscan API key used for verifying the contract (optional). |

## Contract Configuration

Create a `.env` file in the project root directory with the following variables:

### Required Environment Variables

| Variable | Description | Example |
|----------|-------------|---------|
| `L1_BEACON_RPC` | L1 Consensus (Beacon) Node. Could be required for integrations that access consensus-layer data. |
| `GAME_TYPE` | Unique identifier for the game type (uint32). In almost all cases, to use the OP Succinct Fault Dispute Game, this should be set to 42. | `42` |
| `DISPUTE_GAME_FINALITY_DELAY_SECONDS` | Delay before finalizing dispute games. | `604800` for 7 days |
| `MAX_CHALLENGE_DURATION` | Maximum duration for challenges in seconds. | `604800` for 7 days |
| `MAX_PROVE_DURATION` | Maximum duration for proving in seconds. | `86400` for 1 day |
| `PROPOSER_ADDRESSES` | Comma-separated list of addresses allowed to propose games. Ignored if `PERMISSIONLESS_MODE` is true. | `0x123...,0x456...` |
| `CHALLENGER_ADDRESSES` | Comma-separated list of addresses allowed to challenge games. Ignored if `PERMISSIONLESS_MODE` is true. | `0x123...,0x456...` |

### SP1 Verifier Configuration

**For testing**: Set `OP_SUCCINCT_MOCK=true`. The deployment script will automatically deploy a new `SP1MockVerifier` contract — no need to set `VERIFIER_ADDRESS`.

**For production**: Leave `OP_SUCCINCT_MOCK` unset (defaults to `false`) and optionally set `VERIFIER_ADDRESS` to a custom verifier. If `VERIFIER_ADDRESS` is not set, it defaults to Succinct's official Groth16 VerifierGateway.

## Deployment

Run the following command. This automatically detects configurations based on the contents of the `elf` directory, environment variables, and the L2.  

```bash
just deploy-fdg-contracts
```

## Optional Environment Variables

The deployment script deploys the contracts with the following parameters:

| Variable | Description | Example |
|----------|-------------|---------|
| `INITIAL_BOND_WEI` | Initial bond for the game. | 1_000_000_000_000_000 (for 0.001 ETH) |
| `CHALLENGER_BOND_WEI` | Challenger bond for the game. | 1_000_000_000_000_000 (for 0.001 ETH) |
| `OPTIMISM_PORTAL2_ADDRESS` | Address of an existing OptimismPortal2 contract. Default: if unset, a fresh `MockOptimismPortal2` is deployed. | `0x...` |
| `PERMISSIONLESS_MODE` | If set to true, anyone can propose or challenge games. Default: `false` | `true` or `false` |
| `FALLBACK_TIMEOUT_FP_SECS` | Timeout in seconds after which permissionless proposing is allowed if no proposal has been made. | `1209600` (for 2 weeks) |
| `STARTING_L2_BLOCK_NUMBER` | Starting L2 block number in decimal. Default: \<Latest L2 Finalized block\> - \<Number of blocks since the `DISPUTE_GAME_FINALITY_SECONDS`>  | `786000` |
| `VERIFIER_ADDRESS` | Default: Succinct's official Groth16 VerifierGateway. Address of the `ISP1Verifier` contract used to verify proofs. **Ignored when `OP_SUCCINCT_MOCK=true`**. | `0x...` |
| `OP_SUCCINCT_MOCK` | Default: `false`. If `true`, the deployment script automatically deploys a new `SP1MockVerifier` for testing (faster and cheaper than real proofs). | `true` or `false` |

Use `cast --to-wei <value> eth` to convert the value to wei to avoid mistakes.

These values depend on the L2 chain, and the total value secured. Generally, to prevent frivolous challenges, `CHALLENGER_BOND` should be set to at least 10x of the proving cost needed to prove a game.

### Fallback Timeout Mechanism

The `FALLBACK_TIMEOUT_FP_SECS` parameter configures a permissionless fallback timeout mechanism for proposal creation:

- **Default**: If not set, defaults to 2 weeks (1209600 seconds)
- **Behavior**: After the specified timeout has elapsed since the last proposal, anyone can create a new proposal regardless of the `PROPOSER_ADDRESSES` configuration
- **Reset**: The timeout is reset every time a valid proposal is created
- **Immutable**: The timeout value is set during deployment and cannot be changed later
- **Scope**: Only affects proposer permissions; challenger permissions are unaffected

This mechanism ensures that if approved proposers become inactive, the system can still progress through permissionless participation after a reasonable delay.

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
- The mock SP1 verifier (`OP_SUCCINCT_MOCK=true`) should ONLY be used for testing.
- For production deployments:
  - Provide a valid `VERIFIER_ADDRESS`.
  - Configure proper `ROLLUP_CONFIG_HASH`, `AGGREGATION_VKEY`, and `RANGE_VKEY_COMMITMENT`. If you used the `just deploy-fdg-contracts` script, these parameters should have been automatically set correctly using `fetch_fault_dispute_game_config.rs`.
  - Set the `OPTIMISM_PORTAL2_ADDRESS` environment variable, instead of using the default mock portal.
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
