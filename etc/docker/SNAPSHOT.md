# Run a snapshot devnet fork

Run a local Base sequencer and validator from two mainnet snapshot copies, with a batcher and an
Anvil L1 forked at a finalized Ethereum block. **Experimental; use disposable data and test keys only.**

Always use `just devnet snapshot ...`. Plain `just devnet up/down` runs a different,
fresh-genesis devnet with destructive cleanup. Run commands below from the repository root.

## What you need

- Linux with a local Docker daemon, Compose v2 and Buildx; `just`, Python 3.11+, Rust,
  Foundry `cast` and `rsync`.
- An Ethereum mainnet archive RPC and a Beacon API with historical blobs covering the snapshot's
  derivation history. They can share one URL. **No Base RPC is required.**
- Disk space for two independent snapshot databases, build artifacts, download cache and growth.
  Setup downloads the snapshot itself.

To use existing snapshot copies with `init` instead, you also need:

- Two independent, writable copies of the same Base mainnet snapshot (Reth datadirs). Copy only
  stopped databases; hard links, symlinked aliases and nested paths are rejected.
- Local Base, fork-aware Anvil and op-batcher images. Init pins their image IDs.
  `just devnet snapshot build-anvil` builds the pinned Anvil revision as `base-anvil:snapshot-24ec5e47`.
- The snapshot inspector, built with `cargo build --locked -p base-system-tests --bin base-devnet`,
  or `BASE_SNAPSHOT_INSPECTOR` set to an existing build.

## Set up a fork

```sh
just devnet snapshot setup
just devnet snapshot up
```

Setup asks for a working directory (default `~/data/snapshot-devnet`) and L1 endpoints. It builds
Base from this checkout, obtains the pinned Anvil and batcher images and the inspector, downloads
one snapshot, fully copies it for the validator, and initializes the fork as `init` does below.
**No hand-written config, image selection or `--dir` is needed.**

Setup validates the endpoints and saves them privately in the fork's `upstreams.json`. It reads
them from the environment, `~/.config/base/l1.env` or a previous run, prompting only when none is
configured. A configured endpoint that fails validation is an error, not a prompt.

Rerun `setup` after an interruption; it resumes the remembered working directory without repeating
completed builds, downloads or copies, and never downloads or copies again once initialization has
begun. An incomplete download rechecks existing files, which can be slow; an interrupted copy reuses
its partial data. A completed setup only selects its fork and keeps its pinned images; it does not
rebuild from newer source. `--anvil-image` and `--batcher-image` choose other images for a new
setup only; resuming or rerunning an existing setup refuses different ones. For a new experiment, run
`just devnet snapshot setup --workdir /path/to/new-directory`.

## Initialize existing snapshot copies

For existing independently writable copies, use `init` rather than asking setup to overwrite them.
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

To omit `--dir` from later commands, select the fork once; this also saves its endpoints as above
and resumes an interrupted initialization with its recorded config:

```sh
just devnet snapshot setup --dir /data/snapshot/fork
```

## Start, check and stop the fork

Keep the upstream variables exported and pass the fork directory to every command; after setup,
both are optional:

```sh
just devnet snapshot up --dir /data/snapshot/fork      # alias: start
just devnet snapshot status --dir /data/snapshot/fork
just devnet snapshot down --dir /data/snapshot/fork    # alias: stop
```

`up` starts or resumes a prepared or stopped fork in this order:

1. Starts Anvil at F and checks its fork and Beacon identity, the recorded upgrade schedule and
   contract implementations. Inspection then repeats, and both snapshots' L1 origins must exist on
   the local L1. Once Anvil has served the fork, a start without its saved L1 state
   (`l1/anvil.json`) is refused before any container starts.
2. Starts both nodes on the internal network. The validator is a stopped sequencer that derives
   independently. After a restart, each node must re-derive the safe and finalized blocks recorded
   by the last `down` with the same hashes before any L1 block or transaction is created.
3. On first start, mines one L1 block after F if needed and waits until both nodes derive past F
   and agree on a safe head at or above the snapshot head. It then authorizes this fork's batcher
   and signer on the local SystemConfig and restarts the nodes. Each local L1 transaction is
   journaled; one interrupted before its hash was recorded is never resent and must be reconciled
   by hand.
4. Waits until both nodes report upgrade readiness for the recorded schedule, mines on the Beacon
   slot grid, connects the nodes' gossip and starts sequencing and the batcher. Both nodes must
   then derive the same newly batched safe block; any batcher exit, even with code 0, fails.
5. Keeps batching while the sequencer catches up to wall time, then prints the L2 RPC addresses.
6. Schedules Denim (below) and waits until both nodes observe it. A scheduling failure leaves the
   running fork in place; rerun `schedule-denim` to resume.

Execution RPC startup waits without a deadline while its container runs. Derivation, batching and
catch-up fail after `--timeout` seconds (default 7200) without head progress; other readiness
checks after `--timeout` in total. On any failure or interrupt before scheduling, `up` stops the
batcher and all nodes, then L1, attempting each stop even if an earlier one fails, and keeps all
data; rerun `up` to resume.

`status` reports the phase, container states, the journaled Denim schedule and L2 RPC addresses
without probing RPCs, even while `up` runs; a running fork missing a service is `degraded`. L2 RPC
addresses are internal container IPs, reachable only from the Docker host, and change when
containers are recreated. `down` stops sequencing, the batcher, both nodes and finally L1,
attempting each stop even if an earlier one fails. It records each node's sync status as the
checkpoints the next `up` must restore; a node that is stopped, uninitialized or still re-deriving
its earlier checkpoints keeps them.

To retire a stopped fork, run `just devnet snapshot reset --dir ... --confirm-project <project>`.
It renames the fork directory to `<dir>.retired-<suffix>` and leaves the datadirs, which the fork
has modified, untouched. Supply fresh snapshot copies before another `init`.

## Schedule Denim

Denim honors the ProtocolVersions contract's live minimum notice (currently one hour), measured
from the latest L1, L2 and wall-clock time; there is no activation bypass. An existing or
externally recorded schedule is kept, never moved, and the minimum protocol version is unchanged.
To choose a later activation, pass an L2 timestamp. As in production, any timestamp the notice
allows is valid; each chain activates Denim at its first pre-Denim block slot at or after it:

```sh
just devnet snapshot schedule-denim --dir /data/snapshot/fork 1790000000
```

The status countdown is not proof that a node activated it. A scheduling transaction interrupted
before its hash was saved must have its nonce reconciled manually.

## Send a test transaction

Fund the generated throwaway user through the local L1 portal, then send on L2. Do not use
production keys: this fork retains mainnet chain IDs.

```sh
just devnet snapshot deposit --dir /data/snapshot/fork
SEQUENCER_RPC=$(just devnet snapshot status --dir /data/snapshot/fork | jq -r '.rpc_docker_host_only.sequencer')
VALIDATOR_RPC=$(just devnet snapshot status --dir /data/snapshot/fork | jq -r '.rpc_docker_host_only.validator')
USER_ADDRESS=$(jq -r '.accounts.user' /data/snapshot/fork/manifest.json)
cast balance --rpc-url "$SEQUENCER_RPC" "$USER_ADDRESS"
```

Repeat the balance query until the deposit reaches L2 and the balance is nonzero, then:

```sh
cast send --rpc-url "$SEQUENCER_RPC" --chain-id 8453 \
  --private-key "$(jq -r '.user' /data/snapshot/fork/keys.json)" --gas-limit 21000 --value 1 \
  0x000000000000000000000000000000000000bEEF
```

`deposit` funds 1 ETH once; repeating it does not top up the account. Use the transaction hash
from `cast send` with `cast receipt --rpc-url "$VALIDATOR_RPC" <transaction-hash>`; unsafe gossip
can reach the validator before L1 batching makes the block safe.

## Verify derivation (optional)

```sh
just devnet snapshot verify --dir /data/snapshot/fork
```

It requires a running batcher, deposits through the L1 portal (reconciling an earlier `deposit`
at its recorded amount), sends a transaction, waits until the validator derives its block as safe
from L1 batches, requires that safe block to be the transaction's own block with its receipt on
the validator, compares the block's hash and state root on both nodes, and requires the batcher's
blobs from local L1. Gossip stays connected; only derivation makes a block safe. It writes
`verification.json` in the fork directory after these checks pass. It does not restart services or
check Denim activation.

`just devnet snapshot test` runs offline launcher tests instead; it does not verify a running
devnet unless you explicitly set `BASE_SNAPSHOT_FORK_DIR` to opt into the live verifier.

## Where things live

Under setup's working directory, `builder/` and `validator/` are the writable databases,
`input.json` the generated init config, `setup.json` the setup journal and `fork/` the fork
directory.

`manifest.json` in the fork directory records the project, pinned images, datadirs, accounts, F,
both inspections, the contract schedule, the local transaction journal and the last shutdown
checkpoints. `keys.json` holds the throwaway keys, `config/rollup.json` the pinned rollup config
and `l1/` Anvil's saved state, and `upstreams.json` the endpoint credentials saved by setup. The
selected fork is saved in `~/.config/base/snapshot-devnet.json` (or `$XDG_CONFIG_HOME/base/`).
Do not share `keys.json`, `upstreams.json` or raw Docker logs/configuration, which may contain
credentials. Never reconnect modified copies to production.

`just devnet snapshot` lists tasks; `just devnet snapshot test` runs the offline launcher tests.
Implementation: [launcher](../scripts/devnet/snapshot_devnet.py),
[Compose services](docker-compose.snapshot.yml), [Just recipes](../just/snapshot.just).
Do not run Compose directly: it skips the launcher's initialization and recovery checks.
