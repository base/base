# Runbook: ZK Fork Dispute Test

## Goal

Run the ignored ZK fork dispute test against a local Anvil fork and verify that a ZK proof can successfully call `challenge()` on an invalid dispute game.

The default run does not require a naturally invalid live game. It auto-selects the newest game, mutates one intermediate root on the fork, patches the factory registration for the mutated game data, requests a ZK proof, and submits `challenge()`.

## When To Use

Use this runbook when you need to verify the end-to-end ZK challenge path without waiting for a real invalid dispute game.

Do not use this as evidence about live L1 state. The test mutates only the local Anvil fork.

## Prerequisites

- Run from the repository root.
- Docker Desktop is running.
- You can access the RPC endpoints configured in the selected chain YAML.
- Replace the placeholder URLs in `mainnet.yaml`, `sepolia.yaml`, or `zeronet.yaml`, or override them with `BASE_ZK_FORK_*` environment variables.
- You have a funded test private key. The recipe funds this signer on the local Anvil fork.
- You are in the required role shell:

```bash
with-role sudo-dev@web3-shared-dev
```

## Procedure

1. Start the role shell if you are not already in it:

```bash
with-role sudo-dev@web3-shared-dev
```

2. Run the default challenge-path test:

```bash
BASE_ZK_FORK_PRIVATE_KEY='<funded-test-private-key>' \
  just zk-fork-dispute sepolia challenge
```

Do not pass a `game_address` or `game_index` for the default challenge-path test.

## What The Test Does

The default challenge-path run:

1. Starts or uses a local Anvil L1 fork.
2. Auto-selects the newest dispute game.
3. Patches one intermediate root to make the game invalid on the fork.
4. Patches the factory registration so the mutated game remains registered.
5. Requests a ZK proof from the local prover.
6. Submits `challenge()`.
7. Asserts the game records the challenger and countered index.

## Expected Runtime

The run takes roughly 15 minutes with the real prover.

## Success Criteria

A successful run includes output like:

```text
auto-selected game ...
patched factory registration ...
patched game ... intermediate index 0 ...
requesting ZK proof ...
submitted Challenge ... in tx ...
test zk_proof_disputes_invalid_intermediate_root_on_fork ... ok
```

The final test summary should be:

```text
test result: ok. 1 passed; 0 failed
```

## Command Variants

Run against Base mainnet config:

```bash
BASE_ZK_FORK_PRIVATE_KEY='<funded-test-private-key>' \
  just zk-fork-dispute mainnet challenge
```

Run against Base Zeronet config:

```bash
BASE_ZK_FORK_PRIVATE_KEY='<funded-test-private-key>' \
  just zk-fork-dispute zeronet challenge
```

Use a specific invalid intermediate index while still auto-selecting and fork-patching the game:

```bash
BASE_ZK_FORK_PRIVATE_KEY='<funded-test-private-key>' \
  just zk-fork-dispute sepolia challenge '' '' 0
```

Use an already-invalid real game by address:

```bash
BASE_ZK_FORK_PRIVATE_KEY='<funded-test-private-key>' \
  just zk-fork-dispute sepolia challenge '<game_address>'
```

Use an already-invalid real game by factory index:

```bash
BASE_ZK_FORK_PRIVATE_KEY='<funded-test-private-key>' \
  just zk-fork-dispute sepolia challenge '' '<game_index>'
```

Explicit `game_address` or `game_index` mode does not auto-patch the game. The selected game must already contain an invalid intermediate root.

## Troubleshooting

### `InvalidGame()`

The game is not considered registered after mutation. The default auto-selected path should patch the factory registration before patching bytecode. Confirm the output includes:

```text
patched factory registration for game ...
```

### `MissingProof(uint8)`

The wrong intent was used for the selected game state. For the fork-patched invalid TEE proposal path, use:

```bash
just zk-fork-dispute sepolia challenge
```

### `anvil_setCode` Or `anvil_setStorageAt` Failed

`BASE_ZK_FORK_L1_RPC_URL` must point to an Anvil fork, not live L1. The default recipe uses `http://127.0.0.1:18545`.

### Prover Connection Errors

Confirm the local prover endpoint from the selected chain YAML is reachable:

```bash
just zk-prover up
```

The default chain YAMLs use:

```text
prover_grpc_url: "http://localhost:9000"
```
