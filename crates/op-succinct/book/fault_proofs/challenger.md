# Fault Proof Challenger

The fault proof challenger is a component responsible for monitoring and challenging invalid OP-Succinct fault dispute games on the L1 chain. It continuously scans for invalid games and challenges them to maintain L2 state validity.

## Prerequisites

Before running the challenger, ensure you have:

1. Rust toolchain installed (latest stable version)
2. Access to L1 and L2 network nodes
3. The DisputeGameFactory contract deployed (See [Deploy](./deploy.md))
4. Sufficient ETH balance for:
   - Transaction fees
   - Challenger bonds (proof rewards)
5. Required environment variables properly configured (See [Configuration](#configuration))

## Overview

The challenger performs several key functions:

1. **Game Monitoring**: Continuously scans for invalid games that need to be challenged
2. **Game Challenging**: Challenges invalid games by providing counter-proofs
3. **Game Resolution**: Optionally resolves challenged games after their deadline passes
4. **Bond Management**: Handles proof rewards and challenge bonds

## Configuration

The challenger is configured through environment variables. Create a `.env.challenger` file in the project root directory:

### Required Environment Variables

| Variable | Description |
|----------|-------------|
| `L1_RPC` | L1 RPC endpoint URL |
| `L2_RPC` | L2 RPC endpoint URL |
| `FACTORY_ADDRESS` | Address of the DisputeGameFactory contract |
| `GAME_TYPE` | Type identifier for the dispute game |

Either `PRIVATE_KEY` or both `SIGNER_URL` and `SIGNER_ADDRESS` must be set for transaction signing:

| Variable | Description |
|----------|-------------|
| `PRIVATE_KEY` | Private key for transaction signing (if using private key signer) |
| `SIGNER_URL` | URL of the web3 signer service (if using web3 signer) |
| `SIGNER_ADDRESS` | Address of the account managed by the web3 signer (if using web3 signer) |

### Optional Environment Variables

| Variable | Description | Default Value |
|----------|-------------|---------------|
| `FETCH_INTERVAL` | Polling interval in seconds | `30` |
| `ENABLE_GAME_RESOLUTION` | Whether to enable automatic game resolution | `true` |
| `MAX_GAMES_TO_CHECK_FOR_CHALLENGE` | Maximum number of games to scan for challenges | `100` |
| `MAX_GAMES_TO_CHECK_FOR_RESOLUTION` | Maximum number of games to check for resolution | `100` |
| `MAX_GAMES_TO_CHECK_FOR_BOND_CLAIMING` | Maximum number of games to check for bond claiming | `100` |
| `CHALLENGER_METRICS_PORT` | The port to expose metrics on. Update prometheus.yml to use this port, if using docker compose. | `9001` |
| `MALICIOUS_CHALLENGE_PERCENTAGE` | Percentage (0.0-100.0) of valid games to challenge for testing defense mechanisms | `0.0` |

```env
# Required Configuration
L1_RPC=                  # L1 RPC endpoint URL
L2_RPC=                  # L2 RPC endpoint URL
FACTORY_ADDRESS=         # Address of the DisputeGameFactory contract
GAME_TYPE=               # Type identifier for the dispute game
PRIVATE_KEY=             # Private key for transaction signing

# Optional Configuration
FETCH_INTERVAL=30                     # Polling interval in seconds
ENABLE_GAME_RESOLUTION=true           # Whether to enable automatic game resolution
MAX_GAMES_TO_CHECK_FOR_CHALLENGE=100  # Maximum number of games to scan for challenges
MAX_GAMES_TO_CHECK_FOR_RESOLUTION=100 # Maximum number of games to check for resolution
MAX_GAMES_TO_CHECK_FOR_BOND_CLAIMING=100 # Maximum number of games to check for bond claiming
CHALLENGER_METRICS_PORT=9001          # The port to expose metrics on

# Testing Configuration (Optional)
MALICIOUS_CHALLENGE_PERCENTAGE=0.0    # Percentage of valid games to challenge for testing (0.0 = disabled)
```

## Running

To run the challenger:
```bash
cargo run --bin challenger
```

The challenger will run indefinitely, monitoring for invalid games and challenging them as needed.

## Testing Defense Mechanisms

The challenger supports **malicious challenging** of valid games for defense mechanisms testing purposes.

### Configuration

Set `MALICIOUS_CHALLENGE_PERCENTAGE` to enable malicious challenging:

```bash
# Production mode (default) - only challenge invalid games
MALICIOUS_CHALLENGE_PERCENTAGE=0.0

# Testing mode - challenge all valid games
MALICIOUS_CHALLENGE_PERCENTAGE=100.0

# Fine-grained testing - challenge 0.1% of valid games
MALICIOUS_CHALLENGE_PERCENTAGE=0.1

# Mixed testing - challenge 25.5% of valid games
MALICIOUS_CHALLENGE_PERCENTAGE=25.5
```

### Behavior

When malicious challenging is enabled:

1. **Priority 1**: Challenge invalid games (honest challenger behavior)
2. **Priority 2**: Challenge valid games at the configured percentage (defense mechanisms testing behavior)

The challenger will always prioritize challenging invalid games first, then optionally challenge valid games based on the configured percentage.

### Logging

The challenger provides clear logging to distinguish between challenge types:
- `[CHALLENGE]` - Honest challenges of invalid games
- `[MALICIOUS CHALLENGE]` - Testing defense mechanisms of challenged valid games

## Features

### Game Monitoring
- Continuously scans for invalid games
- Checks game validity against L2 state
- Prioritizes oldest challengeable games
- Maintains efficient scanning through configurable limits

### Game Challenging
- Challenges invalid games with counter-proofs
- Handles proof reward bonds
- Ensures proper transaction confirmation
- Provides detailed logging of challenge actions

### Game Resolution
When enabled (`ENABLE_GAME_RESOLUTION=true`), the challenger:
- Monitors challenged games
- Resolves games after their resolution period expires
- Handles resolution of multiple games efficiently
- Respects game resolution requirements

## Architecture

The challenger is built around the `OPSuccinctChallenger` struct which manages:
- Configuration state
- Wallet management for transactions
- Game challenging and resolution logic
- Chain monitoring and interval management

Key components:
- `ChallengerConfig`: Handles environment-based configuration
- `handle_game_challenging`: Main function for challenging invalid games that:
  - Scans for challengeable games
  - Determines game validity
  - Executes challenge transactions
- `handle_game_resolution`: Main function for resolving games that:
  - Checks if resolution is enabled
  - Manages resolution of challenged games
  - Handles resolution confirmations
- `run`: Main loop that:
  - Runs at configurable intervals
  - Handles both challenging and resolution
  - Provides error isolation between tasks

## Error Handling

The challenger includes robust error handling for:
- RPC connection issues
- Transaction failures
- Contract interaction errors
- Invalid configurations

Errors are logged with appropriate context to aid in debugging.

## Development

When developing or modifying the challenger:
1. Ensure all environment variables are properly set
2. Test with a local L1/L2 setup first
3. Monitor logs for proper operation
4. Test challenging and resolution separately
5. Verify proper handling of edge cases
6. Test with various game states and conditions
