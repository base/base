# Prepare a snapshot devnet fork

Prepare a local Base fork of two mainnet snapshot copies, one for a sequencer and one for a
validator, at a finalized Ethereum block. **Experimental; use disposable data and test keys only.**

Always use `just devnet snapshot ...`. Plain `just devnet up/down` runs a different,
fresh-genesis devnet with destructive cleanup. Run commands below from the repository root.

## What you need

- Linux with a local Docker daemon and Compose v2; `just`, Python 3.11+, Rust and Foundry `cast`.
- Two independent, writable copies of the same Base mainnet snapshot (Reth datadirs). Copy only
  stopped databases; hard links, symlinked aliases and nested paths are rejected.
- Local Base, fork-aware Anvil and op-batcher images. Init pins their image IDs.
- An Ethereum mainnet archive RPC and a Beacon API with historical blobs covering the snapshot's
  derivation history. They can share one URL. **No Base RPC is required.**
- The snapshot inspector, built with `cargo build --locked -p base-system-tests --bin base-devnet`,
  or `BASE_SNAPSHOT_INSPECTOR` set to an existing build.

## Initialize a fork

Write a config naming the copies and images:

```json
{
  "sequencer_datadir": "/data/snapshot/sequencer",
  "validator_datadir": "/data/snapshot/validator",
  "base_image": "base:local",
  "anvil_image": "base-anvil:snapshot-24ec5e47",
  "batcher_image": "op-batcher:local"
}
```

Optional fields are `port` (local L1 port, default 19545), `epoch_slots` (default 2),
`protocol_versions` (ProtocolVersions address), and `execution_env`/`beacon_env`: the names of
the variables holding the L1 endpoints, by default `SNAPSHOT_UPSTREAM_EXECUTION` and
`SNAPSHOT_UPSTREAM_BEACON`. Never put URLs or credentials in the config.

```sh
export SNAPSHOT_UPSTREAM_EXECUTION=https://... SNAPSHOT_UPSTREAM_BEACON=https://...
just devnet snapshot init --dir /data/snapshot/fork --config fork.json --allow-write
```

`--allow-write` confirms that both datadirs are disposable: their nodes may repair and later
extend them. Init then:

1. Pins the images and generates a private Compose project and throwaway keys.
2. Starts inspection-only nodes on an internal Docker network. Reth may repair indexes for hours
   before its RPC responds; init waits while the container runs and prints progress.
3. Finds fork block F, the finalized L1 block containing the batches for the sequencer snapshot's
   unsafe tail. F must be canonical and on the Beacon slot grid. Both snapshots must share their
   heads, with canonical L1 origins at or before F. The ProtocolVersions schedule must preserve
   historical activations, and the required contracts must exist at F.
4. Stops the inspection nodes and marks the fork `prepared`. It sends nothing to the upstreams
   and changes no contracts.

Rerunning the same command resumes an interrupted inspection with the same project and keys; a
prepared fork is left unchanged. A changed config, unrecognized files in `--dir`, or a concurrent
command holding the fork lock fails without modifying the fork.

## Where things live

`manifest.json` in the fork directory records the project, pinned images, datadirs, accounts, F,
both inspections and the contract schedule. `keys.json` holds the throwaway keys and
`config/rollup.json` the pinned rollup config. Do not share `keys.json` or raw Docker
logs/configuration, which may contain credentials. Never reconnect modified copies to production.

`just devnet snapshot` lists tasks; `just devnet snapshot test` runs the offline launcher tests.
Implementation: [launcher](../scripts/devnet/snapshot_devnet.py),
[Compose services](docker-compose.snapshot.yml), [Just recipes](../just/snapshot.just).
Do not run Compose directly: it skips the launcher's checks.
