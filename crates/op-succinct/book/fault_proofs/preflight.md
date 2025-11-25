# Pre-Flight Validation

**Proving will break if parameters are incorrect.** After deploying contracts, run this script to verify the contract configuration before the mainnet upgrade.

## What Does Pre-Flight Validate?

Catches configuration issues before production by testing the complete proof generation and submission pipeline end-to-end. The game creation and proof submission is simulated on a forked L1 network.

## Prerequisites

- Deployed `DisputeGameFactory` contract
- L1/L2 RPC access
- SP1 network prover access

## Required Environment Variables

Create a `.env.preflight` file in the **repository root** with the following variables.

> **Note:** Using `.env.preflight` avoids conflicts with existing `.env` files from deployments or other operations. You can specify a different file with `--env-file` if needed.

### Contract Configuration
```bash
# Address of the DisputeGameFactory contract on L1
FACTORY_ADDRESS=

# Game type identifier for OPSuccinctFaultDisputeGames
# This must match the game type registered in the factory
GAME_TYPE=42

# Optional: block number where DisputeGameFactory.setImplementation was called.
# Set when you want to bypass on-chain log discovery (e.g., restrictive RPC rate limits).
# SET_IMPL_BLOCK=<L1_BLOCK_NUMBER>
```

### Signing Configuration
```bash
# L1 private key used to create and prove the dispute game on the forked Anvil chain
# This must be a whitelisted address in the OPSuccinctFaultDisputeGame contract
PRIVATE_KEY=0x...
```

### Network Configuration
```bash
# L1 RPC endpoint (used for Anvil fork during validation)
L1_RPC=<YOUR_L1_RPC>

# L1 beacon chain RPC endpoint (needed when validating blocks with blob/EIP-4844 data)
L1_BEACON_RPC=<YOUR_L1_BEACON_RPC>

# L2 RPC endpoint
L2_RPC=<YOUR_L2_RPC>

# L2 node RPC endpoint (often same as L2_RPC)
L2_NODE_RPC=<YOUR_L2_NODE_RPC>
```

### Prover Configuration
```bash
# Range proof fulfillment strategy
# Options: "reserved", "hosted", "auction"
# - reserved: Use SP1 Network Reserved mode (requires reserved capacity)
# - hosted: Use SP1 Network Hosted mode (managed by SP1)
# - auction: Use SP1 Network Auction mode (open competitive bidding)
RANGE_PROOF_STRATEGY=auction

# Aggregation proof fulfillment strategy
# Options: "reserved", "hosted", "auction"
# Compatibility rules:
# - If RANGE_PROOF_STRATEGY is "auction", this must also be "auction"
# - If RANGE_PROOF_STRATEGY is "reserved" or "hosted", this can be either "reserved" or "hosted"
AGG_PROOF_STRATEGY=auction

# Optional: aggregation proof mode ("plonk" default or "groth16")
# Note: Changing the proof mode requires the corresponding SP1 verifier contract
# address to be set in your deployment.
# AGG_PROOF_MODE=plonk

# Set to 'true' to use AWS KMS for key management (requires KMS configuration).
# Set to 'false' to use a local private key (requires NETWORK_PRIVATE_KEY below).
# Optional; defaults to 'false' if omitted.
# USE_KMS_REQUESTER=false

# SP1 network prover private key (required when USE_KMS_REQUESTER=false)
# When USE_KMS_REQUESTER=true, this should be an AWS KMS key ARN instead
NETWORK_PRIVATE_KEY=0x...
```

## Running Pre-Flight Validation

Run the preflight script from the **repository root**:

```bash
RUST_LOG=info cargo run --release --bin preflight
```

```admonish note
For rollups using alternative DA layers, compile with the matching feature flag:
`--features celestia` or `--features eigenda` (for example,
`RUST_LOG=info cargo run --release --features eigenda --bin preflight`).
```

By default, this uses `.env.preflight`. To use a different environment file:

```bash
RUST_LOG=info cargo run --release --bin preflight -- --env-file .env.custom
```

The script will:
1. Fetch the anchor L2 block number from the factory
2. Generate a range proof for the game
3. Generate an aggregation proof for the game
4. Fork L1 at a finalized block using Anvil
5. Create a game at 10 blocks after the anchor
6. Prove the game with the aggregation proof
7. Verify the game has been validated with the aggregation proof

Generated proofs are saved to:
- `data/{CHAIN_ID}/proofs/range/{L2_START_BLOCK}-{L2_END_BLOCK}.bin`
- `data/{CHAIN_ID}/proofs/agg/agg.bin`
