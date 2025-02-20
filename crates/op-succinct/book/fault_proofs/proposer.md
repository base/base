# Fault Proof Proposer

The fault proof proposer is a component responsible for creating and managing OP-Succinct fault dispute games on the L1 chain. It continuously monitors the L2 chain and creates new dispute games at regular intervals to ensure the validity of L2 state transitions.

## Prerequisites

Before running the proposer, ensure you have:

1. Rust toolchain installed (latest stable version)
2. Access to L1 and L2 network nodes
3. The DisputeGameFactory contract deployed (See [Deploy](./deploy.md))
4. Sufficient ETH balance for:
   - Transaction fees
   - Game bonds (configurable in the factory)
5. Required environment variables properly configured (see [Configuration](#configuration))

## Overview

The proposer performs several key functions:

1. **Game Creation**: Creates new dispute games for L2 blocks at configurable intervals
2. **Game Resolution**: Optionally resolves unchallenged games after their deadline passes
3. **Chain Monitoring**: Continuously monitors the L2 chain's safe head and creates proposals accordingly
4. **Fast Finality Mode**: Optionally enables fast finality by including proofs with proposals

## Configuration

The proposer is configured through various environment variables. Create a `.env.proposer` file in the fault_proof directory:

### Required Environment Variables

| Variable | Description |
|----------|-------------|
| `L1_RPC` | L1 RPC endpoint URL |
| `L2_RPC` | L2 RPC endpoint URL |
| `FACTORY_ADDRESS` | Address of the DisputeGameFactory contract |
| `GAME_TYPE` | Type identifier for the dispute game |
| `PRIVATE_KEY` | Private key for transaction signing |

### Optional Environment Variables

| Variable | Description | Default Value |
|----------|-------------|---------------|
| `PROPOSAL_INTERVAL_IN_BLOCKS` | Number of L2 blocks between proposals | `1800` |
| `FETCH_INTERVAL` | Polling interval in seconds | `30` |
| `ENABLE_GAME_RESOLUTION` | Whether to enable automatic game resolution | `true` |
| `MAX_GAMES_TO_CHECK_FOR_RESOLUTION` | Maximum number of games to check for resolution | `100` |

```env
# Required Configuration
L1_RPC=                  # L1 RPC endpoint URL
L2_RPC=                  # L2 RPC endpoint URL
FACTORY_ADDRESS=         # Address of the DisputeGameFactory contract (obtained from deployment)
GAME_TYPE=               # Type identifier for the dispute game (must match factory configuration)
PRIVATE_KEY=             # Private key for transaction signing

# Optional Configuration
PROPOSAL_INTERVAL_IN_BLOCKS=1800    # Number of L2 blocks between proposals
FETCH_INTERVAL=30                   # Polling interval in seconds
ENABLE_GAME_RESOLUTION=false        # Whether to enable automatic game resolution
MAX_GAMES_TO_CHECK_FOR_RESOLUTION=100  # Maximum number of games to check for resolution
```

### Configuration Steps

1. Deploy the DisputeGameFactory contract following the [deployment guide](./deploy.md)
2. Copy the factory address from the deployment output
3. Create `.env` file with the above configuration
4. Ensure your account has sufficient ETH for bonds and gas

## Running

To run the proposer:
   ```bash
   cargo run --bin proposer
   ```

The proposer will run indefinitely, creating new games and optionally resolving them based on the configuration.

## Features

### Game Creation
- Creates new dispute games at configurable block intervals.
- Computes L2 output roots for game proposals.
- Ensures proper game sequencing with parent-child relationships.
- Handles bond requirements for game creation.
- Supports fast finality mode with proofs.

### Game Resolution
When enabled (`ENABLE_GAME_RESOLUTION=true`), the proposer:
- Monitors unchallenged games
- Resolves games after their challenge period expires
- Respects parent-child game relationships in resolution
- Only resolves games whose parent games are already resolved

### Chain Monitoring
- Monitors the L2 chain's finalized (safe) head
- Creates proposals for new blocks as they become available
- Maintains proper spacing between proposals based on configuration
- Tracks the latest valid proposal for proper sequencing

## Logging

The proposer uses the `tracing` crate for logging with a default level of INFO. You can adjust the log level by setting the `RUST_LOG` environment variable:

```bash
RUST_LOG=debug cargo run --bin proposer
```

## Error Handling

The proposer includes robust error handling for:
- RPC connection issues
- Transaction failures
- Contract interaction errors
- Invalid configurations

Errors are logged with appropriate context to aid in debugging.

## Architecture

The proposer is built around the `OPSuccinctProposer` struct which manages:
- Configuration state.
- Wallet management for transactions.
- Game creation and resolution logic.
- Chain monitoring and interval management.

Key components:
- `ProposerConfig`: Handles environment-based configuration.
- `handle_game_creation`: Main function for proposing new games that:
  - Monitors the L2 chain's safe head.
  - Determines appropriate block numbers for proposals.
  - Creates new games with proper parent-child relationships.
- `handle_game_resolution`: Main function for resolving games that:
  - Checks if resolution is enabled.
  - Manages resolution of unchallenged games.
  - Respects parent-child relationships.
- `run`: Main loop that:
  - Runs at configurable intervals.
  - Handles both game creation and resolution.
  - Provides error isolation between creation and resolution tasks.

### Helper Functions
- `create_game`: Creates individual games with proper bonding.
- `try_resolve_unchallenged_game`: Attempts to resolve a single game.
- `should_attempt_resolution`: Determines if games can be resolved based on parent status.
- `resolve_unchallenged_games`: Manages batch resolution of games.

## Development

When developing or modifying the proposer:
1. Ensure all environment variables are properly set.
2. Test with a local L1/L2 setup first.
3. Monitor logs for proper operation.
4. Test game creation and resolution separately.
5. Verify proper handling of edge cases (network issues, invalid responses, etc.).
6. Test both normal and fast finality modes.
