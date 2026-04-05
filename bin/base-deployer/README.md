# `base-deployer`

`base-deployer` is a CLI for:

- generating L1/L2 genesis artifacts for local Base devnets
- deploying OP Stack contracts to a live L1
- extracting L2 genesis and rollup configs from a live deployment
- starting and checking a local Docker Compose devnet

## Commands

```bash
base-deployer genesis
base-deployer deploy-l1 --l1-rpc http://127.0.0.1:8545
base-deployer deploy-l2 --l1-rpc http://127.0.0.1:8545
base-deployer devnet
base-deployer status
```

## Local Devnet

Start a fresh local devnet with Docker Compose:

```bash
cargo run -p base-deployer -- devnet
```

Check the running stack:

```bash
cargo run -p base-deployer -- status
cargo run -p base-deployer -- status --json
```

The local `devnet` command resets `./.devnet`, rebuilds the setup/runtime images, starts the compose stack, and waits for the public RPC endpoints to respond before printing connection details.

## Artifact Generation

Generate a standalone artifact bundle:

```bash
cargo run -p base-deployer -- genesis --output-dir ./.artifacts/devnet
```

This writes:

- `l1/el/genesis.json`
- `l1/el/chain-config.json`
- `l1/cl/config.yaml`
- `l1/cl/genesis.ssz`
- `l2/intent.toml`
- `l2/genesis.json`
- `l2/rollup.json`
- `l2/rollup-conductor.json`
- `chain-ids.json`
- `accounts.json`

## Config Files

`base-deployer` accepts JSON or TOML via `--config`.

Example TOML:

```toml
l1_chain_id = 1337
l2_chain_id = 84538453
slot_duration = 4
l2_base_v1_block = 20
```

Example JSON:

```json
{
  "l1_chain_id": 1337,
  "l2_chain_id": 84538453,
  "slot_duration": 4,
  "l2_base_v1_block": 20
}
```

Global flags also accept env vars:

- `--output-dir` / `OUTPUT_DIR`
- `--l1-chain-id` / `L1_CHAIN_ID`
- `--l2-chain-id` / `L2_CHAIN_ID`
- `--slot-duration` / `SLOT_DURATION`
- `--genesis-time` / `GENESIS_TIME`
- `--prefund-balance` / `PREFUND_BALANCE`
- `--l2-base-v1-block` / `L2_BASE_V1_BLOCK`

## Live Deployment

Deploy OP Stack contracts to a live L1 and persist a reusable `op-deployer` workdir:

```bash
cargo run -p base-deployer -- deploy-l1 --l1-rpc https://your-l1.example
```

Then extract the L2 genesis and rollup config:

```bash
cargo run -p base-deployer -- deploy-l2 --l1-rpc https://your-l1.example
```

`deploy-l2` reuses the `chain-ids.json` and live `op-deployer` workdir created by
`deploy-l1`, so repeated commands must point at the same `--output-dir`.

For external-L1 preparation in one step:

```bash
cargo run -p base-deployer -- devnet --l1-rpc https://your-l1.example
```

In that mode, `devnet` prepares the live deployment manifest plus the L2 artifact bundle and prints their paths. The long-lived local runtime flow remains the Docker Compose path described above.

`deploy-l1` writes `l1/deployment-manifest.json`, including the deployed-address manifest for:

- `OptimismPortal`
- `L1CrossDomainMessenger`
- `L1StandardBridge`
- `SystemConfig`
- `AddressManager`

## Migration From Setup Containers

The old setup flow was:

- `setup-l1.sh` for L1 genesis and CL inputs
- `setup-l2.sh` for `op-deployer` execution and L2 config extraction

The new command mapping is:

- `setup-l1.sh` -> `base-deployer genesis`
- live L1 contract deployment -> `base-deployer deploy-l1`
- `setup-l2.sh` -> `base-deployer deploy-l2`
- `docker compose up ...` wrapper -> `base-deployer devnet`

`etc/docker/docker-compose.yml` now invokes `base-deployer` inside the setup image so the compose-based devnet continues to work while moving the behavior into a typed Rust CLI.

## Testing

Unit tests:

```bash
cargo test -p base-deployer
```

Docker-backed integration test:

```bash
cargo test -p base-deployer --test devnet -- --ignored
```

The integration test is ignored by default because it builds images, starts long-lived containers, and needs Docker Compose plus open local ports.
