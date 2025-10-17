# Upgrading the OPSuccinct Fault Dispute Game

This guide explains how to upgrade the OPSuccinct Fault Dispute Game contract.

## Overview

The upgrade script performs the following actions:
1. Deploys a new implementation of the `OPSuccinctFaultDisputeGame` contract
2. Sets the new implementation in the `DisputeGameFactory` for the specified game type

### Required Environment Variables

Create a `.env` file in the contracts directory with the following variables:

| Variable | Description | Example |
|----------|-------------|---------|
| `FACTORY_ADDRESS` | Address of the existing DisputeGameFactory | `0x...` |
| `GAME_TYPE` | Unique identifier for the game type (uint32) | `42` |
| `MAX_CHALLENGE_DURATION` | Maximum duration for challenges in seconds | `604800` for 7 days |
| `MAX_PROVE_DURATION` | Maximum duration for proving in seconds | `86400` for 1 day |
| `VERIFIER_ADDRESS` | Address of the SP1 verifier | `0x...` |
| `ROLLUP_CONFIG_HASH` | Hash of the rollup configuration | `0x...` |
| `AGGREGATION_VKEY` | Verification key for aggregation | `0x...` |
| `RANGE_VKEY_COMMITMENT` | Commitment to range verification key | `0x...` |
| `ANCHOR_STATE_REGISTRY` | Address of the AnchorStateRegistry | `0x...` |
| `ACCESS_MANAGER` | Address of the AccessManager | `0x...` |

### Getting the Rollup Config Hash, Aggregation Verification Key, and Range Verification Key Commitment

First, create a `.env` file in the root directory with the following variables:
```bash
L1_RPC=<L1_RPC_URL>
L2_RPC=<L2_RPC_URL>
L2_NODE_RPC=<L2_NODE_RPC_URL>
```

You can get the aggregation program verification key, range program verification key commitment, and rollup config hash by running the following command:

```bash
cargo run --bin config --release -- --env-file <PATH_TO_ENV_FILE>
```

### Optional Environment Variables

| Variable | Description | Default | Example |
|----------|-------------|---------|---------|
| `CHALLENGER_BOND_WEI` | Challenger bond for the game | `0.001 ether` | `1000000000000000` |

Use `cast --to-wei <value> eth` to convert the value to wei to avoid mistakes.

## Upgrade Command

Dry run the upgrade command in the root directory of the project:
```bash
DRY_RUN=true just -f fault-proof/justfile --dotenv-filename contracts/.env upgrade-fault-dispute-game
```

Run the upgrade command in the root directory of the project:
```bash
DRY_RUN=false just -f fault-proof/justfile --dotenv-filename contracts/.env upgrade-fault-dispute-game
```

## Verification

You can verify the upgrade by running the following command:
```bash
cast call <FACTORY_ADDRESS> "gameImpls(uint32)" <GAME_TYPE> --rpc-url <L1_RPC_URL>
```

## Troubleshooting

Common issues and solutions:

1. **Compilation Errors**:
   - Run `cd contracts && forge clean`

2. **Deployment Failures**:
   - Check RPC connection
   - Verify sufficient ETH balance
   - Confirm environment variables are set correctly
