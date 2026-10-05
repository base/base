# Snapshot-backed mainnet-identity devnet

**Experimental: offline checks do not qualify a snapshot.** Run the acceptance commands below
on approved disposable copies before relying on a fork. This is separate from `just devnet up`
(fresh genesis and destructive cleanup) and `base-devnet snapshot` (L1-free benchmark mode).
Neither is used by this launcher. Proof services are out of scope. Setup delegates downloads to
the existing `base snapshot download` command in the pinned Base image.

Snapshot is a native Just submodule with one recipe per task. Run `just devnet snapshot` to list
commands, or `just devnet snapshot setup --help` for a task's options. Put flags after the task:
`just devnet snapshot up --dir /srv/forks/base-test --timeout 7200`.
`start` aliases `up`; `stop` aliases `down`. Neither invokes fresh-genesis cleanup.
The recipes share Python code for RPC calls, manifests and locking.

## Setup once, then use up/down without a directory

```sh
just devnet snapshot setup
just devnet snapshot up
just devnet snapshot status
just devnet snapshot down  # preserves all state
```

Setup reuses the saved working directory and configured endpoints without prompting. It reads
endpoints from the environment, `~/.config/base/l1.env`, or the fork's private `upstreams.json`.
Only missing values are requested: a working directory (default `~/data/snapshot-devnet`) and an
Ethereum L1 RPC (hidden input). Without a configured Beacon URL, it tries the L1 endpoint first,
asking for a separate URL only if necessary. Setup still checks chain ID and Beacon APIs before
downloading; invalid saved endpoints fail validation instead of prompting for replacements.
Initialization checks the history needed by the actual snapshot.

Rerun `just devnet snapshot setup` after an interruption: it remembers the pending directory and
resumes the unfinished phase. `setup.json` records image builds, download completion and copy
completion. Before downloading, `download-manifest.json` pins the selected snapshot and archive
locations; retries never switch to a newer "latest" snapshot. An interrupted download re-verifies
completed files (potentially reading terabytes) and resumes missing work. A completed download is
not invoked again. An interrupted copy skips finished files and recopies incomplete files.
Once initialization starts, neither download nor copy runs again.

Older saved workdir configurations are also reused automatically. To select a download completed
with the older launcher explicitly, use:

```sh
just devnet snapshot setup --workdir ~/data/snapshot-devnet
```

It asks once to confirm that downloading succeeded, both datadirs remain unused, and no manual copy
is running. This confirmation adopts the existing download without invoking the downloader; it
finishes copying and initializes the fork. `reth.toml` alone is not proof of a completed download.
Unknown directories and changed initialization settings are rejected without overwriting data.

Setup defaults to 16 simultaneous HTTP downloads, including parallel chunks within large files.
Use `just devnet snapshot setup --download-concurrency 32` to try a higher limit on a fast connection.
Compare sustained throughput at 16 and 32; more parallelism also increases disk/extraction load and
can cause server throttling, so it does not guarantee more bandwidth. This setting applies to new
downloads, not a download already running.

The working directory holds `builder/`, `validator/`, generated `input.json`, and `fork/`.
Setup downloads once, makes a full independent copy of the stopped data with `rsync` (reporting
progress; no reflinks or hard links), and initializes the fork. The databases are disposable
working copies, not backups.
It saves the selected fork path in `$XDG_CONFIG_HOME/base/snapshot-devnet.json` (normally
`~/.config/base/snapshot-devnet.json`) only after initialization succeeds. Without saved setup,
`up` exits before starting services. `--dir <fork-directory>` remains an explicit override.

Endpoints are stored in the fork's `upstreams.json` with mode 0600; the manifest and selection file
contain no endpoint credentials. Later commands reuse them; environment variables can override
them. Never share `upstreams.json` or `keys.json`. No hand-written input/manifest is needed.

Setup builds the release Base image from the current checkout, including local source changes,
using the existing Docker bake `base` target. There is no `--base-image` selection. It also rebuilds
the snapshot inspector from this checkout unless `BASE_SNAPSHOT_INSPECTOR` explicitly supplies one.
Build caches are reused, then immutable image IDs are recorded. Anvil and batcher use the compatible
source pins below, building them locally if absent; `--anvil-image` and `--batcher-image` select
explicit alternatives. Resuming an existing fork neither rebuilds nor changes its pinned images.

For an already initialized experiment, select it without copying or opening its datadirs:

```sh
just devnet snapshot setup --dir ~/data/snapshot-devnet/fork
just devnet snapshot status
```

Setup preserves unfinished work and the previously selected experiment until initialization
succeeds. Repeating a completed setup selects it without rebuilding, downloading, copying or
reinitializing. Use `--workdir` to choose a different experiment. The fork lock prevents concurrent
launcher operations; never run a manual copy or downloader alongside setup. Builds/downloads/copies
are not limited by the node-readiness timeout. Ctrl+C preserves progress and reports how to resume.
Setup does not start the devnet: run `up` afterward. Repeated `up` keeps fully running services in
place; a partial startup is stopped in dependency order before the existing recovery path resumes.

`up` runs containers through `docker-compose.snapshot.yml`; the Python launcher coordinates
snapshot inspection, RPC readiness, fork recovery, and enabling sequencing and batching. A bare
`docker compose up` does not perform those steps. Startup prints stage messages and reports pending
RPC waits on stderr after the first failed check and every 30 seconds thereafter, including elapsed
time and the current container's latest recognized startup log. Reth may repair
snapshot indexes before exposing its RPC; the report shows repair batches, indexing progress, and
the log timestamp rather than treating an unavailable RPC as proof of a hang. Only known stages
and numeric fields are shown, not raw logs or credentials. Unknown log formats are reported as
unrecognized; use `docker logs --follow <inspection-container>` for full details. L2 execution RPC
readiness has **no deadline**, even if repair progress logs are unchanged. It waits while the
container runs, fails if the container exits or disappears, and can be canceled with Ctrl+C.
Individual RPC and Docker calls remain bounded. L1 RPC and other readiness gates retain
`--timeout`; catch-up waits indefinitely while heads keep advancing.
Startup failure or cancellation stops the inspection nodes and other fork services before stopping
L1; it preserves the downloaded snapshots, copied datadirs, and fork state. Unfinished repair work
can repeat on restart. Rerun `up` after shutdown completes; do not redownload or recopy the data.
Launcher changes and `--timeout` only affect new invocations, not an already running launcher.

## Prerequisites

- Linux, Python 3.11+, Docker Compose v2, Foundry `cast`, `rsync`, and Rust for the inspection binary.
- Two stopped, independently writable Base mainnet Reth datadirs at the **same latest head**
  (created automatically by setup, or supplied to advanced init).
  Make a full copy of the entire datadir, including static files; never use hard links.
  Keep any pristine backups separate. `init --allow-write` explicitly
  permits inspection nodes to open these copies writable; it is not a read-only database open.
  Reth's database lock prevents concurrent opens; do not run these paths in another node.
- Ethereum execution archive and Beacon endpoints retaining all history needed for derivation
  reset/channel lookback and for the batches after the snapshot's safe head. Execution archive
  access does **not** imply historical blob access. No Base rollup endpoint is used.
  Prefer recent snapshots; normal L2 catch-up is required, not hidden by a clock/genesis override.
- A deployed `ProtocolVersions` with `getSchedule`, `minimumProtocolVersion`, `proxyAdminOwner`,
  `registerUpgrade(uint64,uint256)` and `setTimestamp(uint256,uint64)`. Older contract versions,
  unavailable safe/finalized block bodies, conflicting heads and missing history fail closed.
- Linux with the Docker daemon on the same host: L2 RPCs are reached directly at container
  addresses on the project's internal network, which only the Docker host can route to.

The upstream schedule must preserve already-active upgrades in the pinned Base config. This
launcher will not deploy replacement mainnet contracts, fabricate checkpoint labels, patch L2
storage/balances, or bypass derivation to make an incompatible snapshot start.

## Advanced: build and initialize manually

From the repository root:

```sh
cargo build --locked -p base-system-tests --bin base-devnet
docker buildx bake -f etc/docker/docker-bake.hcl base op-batcher --load
just devnet snapshot build-anvil
```

The Anvil recipe pins [the fork-aware Beacon and persistence revision](https://github.com/base/base-anvil/commit/24ec5e4732f35b904c43cb05a33fd459e6e92cb8).
Use the Base image built from this checkout, which includes effective `--p2p.no-discovery` handling.
`init` resolves all three images to immutable local image IDs; `start` never pulls or rebuilds.
The inspector defaults to `target/debug/base-devnet`; set `BASE_SNAPSHOT_INSPECTOR` to use another
build of the same source. The canonical Go batcher is pinned by `Dockerfile.op-batcher`.

If you have no downloaded datadirs, build the host downloader and choose a **new** working directory:

```sh
cargo build --locked --release -p base --bin base
WORK="$HOME/data/snapshot-devnet"
mkdir -m 700 "$WORK"  # deliberately fails if this experiment already exists

# Download once, then copy while no node has opened the datadir.
./target/release/base snapshot download --chain mainnet \
  --datadir "$WORK/builder" --non-interactive --download-concurrency 16 \
  --with-txs-distance 1339200 --with-receipts-distance 1339200 \
  --with-state-history-distance 1339200 && \
  rsync -a --info=progress2 --no-inc-recursive "$WORK/builder/" "$WORK/validator/"
```

For the initial copy, use a nonexistent validator directory. The trailing slashes copy the datadir's
contents directly into `validator/`; local `rsync` writes a full independent copy, never reflinks or
hard links, and reports overall progress. If this copy was interrupted, stop any remaining copy
process and rerun just the `rsync` command: it skips completed files and recopies incomplete files.
Both nodes must remain stopped, and the source must be the same untouched snapshot. Guided setup
users can instead rerun setup to finish copying and initialization. Manual users can initialize
with their saved `input.json` as described below. Never invoke an unpinned downloader over this data.
Budget disk space for both writable databases, temporary copy files, download cache and growth.
Setup selects all state/headers and the most recent
1,339,200 blocks of transactions, receipts and state history, matching the published full-snapshot
window (31 days at the legacy two-second cadence, rounded to whole archive chunks). It deliberately
does not use `--full`: that preset follows production Denim pruning defaults and retains 13,392,000
blocks, selecting substantially more snapshot data. Production pruning defaults are unchanged.
Startup still verifies the snapshot's labeled bodies and upstream L1/Beacon history.
The downloader supports resuming; do not use `--force`, which deletes existing database data.
For a disk-space preview before downloading, add `--print-plan-json` to the download command (without
running the copy). Never separately download "latest" twice: the published snapshot can change
between downloads. For these paths, use `$WORK/builder` and `$WORK/validator` in the input JSON,
and `$WORK/fork` as `--dir`; the latter must not already exist.

Create an operator-owned JSON config outside the pristine snapshot directories:

```json
{
  "sequencer_datadir": "/srv/disposable/base-sequencer",
  "validator_datadir": "/srv/disposable/base-validator",
  "base_image": "base:local",
  "anvil_image": "base-anvil:snapshot-24ec5e47",
  "batcher_image": "op-batcher:local",
  "port": 19545,
  "epoch_slots": 2
}
```

The fork directory must be new or belong to the same initialization, and must not overlap either
datadir. `port` is the only published
port: Anvil's L1 RPC/Beacon API on loopback. `epoch_slots` selects Anvil's simulated safe/finalized
**block-depth** policy (one/two epochs), not Ethereum consensus or elapsed-slot finality. There is
no manual fork block: `fork_block` and `rollup_env` from older configs are rejected.

Supply endpoints via your shell/secret manager as `SNAPSHOT_UPSTREAM_EXECUTION` and
`SNAPSHOT_UPSTREAM_BEACON`. The config can override the variable names with `execution_env` and
`beacon_env`; do not put endpoint URLs in the config. The launcher passes the values to the
inspector under the standard names and redacts URLs from its diagnostics. An optional
`protocol_versions` overrides the default mainnet proxy address.

```sh
just devnet snapshot init --dir /srv/forks/base-test --config /srv/snapshot-input.json --allow-write
just devnet snapshot up --dir /srv/forks/base-test
just devnet snapshot status --dir /srv/forks/base-test
```

Init trusts the snapshot's safe and finalized labels. While the sequencer copy's inspection node
runs, `base-devnet inspect-snapshot --find-fork` runs the production derivation pipeline over
upstream L1 and matches canonical batches against the snapshot's (safe, latest] blocks. The L1 block
completing that tail is F (when safe equals latest, the search starts at its parent). Unavailable
history or a tail that no canonical batch produced fails init. The launcher then requires F's
upstream header to match by number, hash, timestamp and parent, F to be finalized and on the Beacon
slot grid, every labeled L1 origin to be canonical and no later than F, and the contracts and
historical schedule to be compatible at F. If F is not finalized yet, retry after finality.

Init records labeled L2 heads/system configs, chain/genesis identity, F, Beacon genesis/slot
duration, historical schedule, image IDs and datadir paths. It stops the inspection nodes before
returning and does not change local L1 contracts. An incomplete init resumes in the same directory,
preserving the Docker project, image IDs and keys. Repeating a completed init with the same settings
is a no-op; changed settings are rejected. Inspection and production nodes never run on the
same datadir at once; the launcher refuses lookups and inspection when both kinds are running.

Start forks L1 at F on the historical production time rules, then runs both normal nodes with
sequencing stopped. The validator uses permanently stopped-sequencer mode: unlike RPC mode, it
probes EL sync immediately without waiting for fresh gossip, then independently derives from L1.
The launcher never enables its sequencing. Reporting `current_l1 == F` means a node entered F,
not that it applied F's batches, so start mines **one** local block F+1 at the current slot and waits until both nodes
independently report `current_l1 > F`. Their pipelines advance only after the engine acknowledged
every attribute from F. Each node must then have safe == unsafe, both must share one safe hash at
or above the snapshot head, and the snapshot head must still be canonical; any mismatch fails
closed. The gate and the F+1 successor are recorded and are not repeated on resume.

Only then does start impersonate local SystemConfig owners to register throwaway batcher/signer
accounts and restart the normal signal readers. The batcher's DA throttling needs the `miner` API,
which only the sequencer's private HTTP RPC exposes; start checks `miner_getMaxDASize` before
enabling sequencing. Sequencing starts on the unchanged protocol time schedule and catches up with
mostly empty L2 blocks to wall time. The batcher starts with sequencing, before catch-up: derivation
authorizes batch senders by the SystemConfig at each batch's L1 inclusion block, so blocks with
pre-bootstrap L1 origins are batchable, and holding them back for hours could expire their
sequencing windows. Start first requires both safe heads to pass the highest pre-batcher safe height
with the same canonical hash at the next height, then waits for wall-time catch-up, and reports
`running` only after both; the batcher exiting at any point fails, even with code 0. Derivation,
catch-up and batching print head/lag progress every 30 seconds and fail only after `--timeout`
seconds (default 7200, or two hours) without head progress. Ordinary readiness gates have the
same per-step budget, except L2 execution RPC readiness, which has no deadline; this is not a
whole-run deadline. A failure preserves data and stops dependents before L1; rerun `start` to resume.
`status` reports `degraded` when a started fork's L1, node or batcher container is not running.

After successful startup, Denim is automatically scheduled at the earliest even timestamp allowed
by the contract's live `MIN_NOTICE()` plus two mining slots for inclusion. Existing Denim timestamps
are retained across restarts, including ones already active. Startup waits until both nodes observe
the schedule, not until activation. Scheduling failure leaves the running fork intact; retry with
`just devnet snapshot schedule-denim` without a timestamp. `status` displays the recorded timestamp
and wall-clock countdown; "activation time reached" is not proof of node activation.

Some snapshots omit transaction-lookup or account/storage-history indexes. Reth rebuilds them
before accepting forkchoice updates; this is local database recovery, not mainnet sync. Use an
optimized Base build for large snapshots. Stage checkpoints may remain unchanged through a large
indexing pass, so consult the node logs before diagnosing a stall. The launcher
does not shorten retention or prune extra history to skip this recovery. `status` remains usable
while `start` is waiting and omits container command arguments containing credentials.

All services except Anvil have only the internal network; Docker ignores published ports there, so
Anvil's loopback port is the only published one. `start` and `status` print the L2 RPCs (EL 8545,
CL 9545) at container addresses on that network. They work **only on the Docker host** and change
whenever containers are recreated; read them from `status` instead of saving them. EL
discovery/persisted peers and CL discovery are disabled; only explicit private CL peers are
connected at their internal addresses. L1/Beacon/upgrade reads point at Anvil. **Any local process
can control this fork.** Do not expose these RPCs or import production keys: the mainnet chain IDs
make local signed transactions replayable. Endpoint credentials and development keys remain visible
to Docker administrators; do not share raw `docker inspect`, `compose config`, container logs,
`upstreams.json` or `keys.json`.

## Baseline, Denim and restart qualification

```sh
just devnet snapshot verify --restart
# Run before the timestamp displayed by status; up has already scheduled Denim.
just devnet snapshot verify --denim --restart --interrupt
```

The verifier funds a throwaway user through the real L1 portal, disconnects CL gossip, sends L2
transactions, requires independently derived safe hashes/state roots, checks local blob responses
and origin advancement, and writes `verification.json`. Startup's matching newly batched safe-head
gate is sufficient for scheduling; the full verifier is no longer a separate scheduling prerequisite.
Scheduling uses the real contract's one-hour notice/freeze rules; it retains the global minimum
version and all historical entries, and waits for the integrated nodes' readers to observe Denim.
No time warp or activation bypass is used. The activation-window wait uses the two-hour timeout
as a stall budget while L2 heads advance, so a healthy chain can wait through the one-hour notice.
Start qualification before activation; it fails if the transaction generator
misses any required pre-activation/activation/sibling/whole-second block. It checks 200ms cadence,
BaseTime at tx[1], gas division and fee-denominator multiplication exactly once.

`--restart` checks retained transactions, safe block hashes and blobs; with `--denim`, it restarts
before and after activation. It also stops batching with an unsafe transaction pending safe
advancement. `--interrupt` kills only this project's L2 containers in that state, then cleanly stops
L1 and resumes through normal replay. This is **not** an all-process crash/atomic-checkpoint test.
The full fixture-backed production run and a real mainnet-snapshot session remain required
qualification; mocked lifecycle tests alone do not establish those acceptance criteria.

## Non-destructive lifecycle

```sh
just devnet snapshot down --dir /srv/forks/base-test
just devnet snapshot up --dir /srv/forks/base-test  # same identities, state and schedule
just devnet snapshot deposit --dir /srv/forks/base-test --wei 1000000000000000000
just devnet snapshot reset --dir /srv/forks/base-test --confirm-project snapshot-PROJECT_FROM_STATUS
```

`stop` stops sequencing, batcher, L2 and finally L1; it never removes containers, volumes or data.
Reth may leave a short in-memory tail unpersisted even on clean shutdown. On `start`, missing
recorded safe/finalized blocks above the durable head are replayed from retained local L1 with
sequencing stopped. Both nodes must reach their saved safe heights and match every recorded hash
before mining or sequencing resumes. A missing checkpoint at/below the durable head, a conflicting
hash, or stalled replay still fails closed. Unbatched unsafe blocks are not recoverable from L1.
Then `start` re-aligns local L1 to the next current Beacon slot, skipping downtime, and reapplies
interval mining. Normal L2 catch-up remains mandatory. Setup saves execution/Beacon endpoints for
restarts; manual init users must supply the variables again. Stop/status/reset do not require them.
Deposit is an idempotent one-time funding
operation; repeating it with a different amount fails instead of spending again.

`reset` only renames the stopped fork directory to a `.retired-*` sibling. Both L2 datadirs and all
L1/config evidence remain intact. Restore **fresh working copies** before initializing another fork;
never reconnect locally mutated copies to production. Reclaim retired disk space manually only
after deciding which disposable data to delete. Reserve space for Reth growth, Anvil historical
states/blobs and periodic full dumps; there is no automatic pruning or disk quota.

Bootstrap/deposit transactions journal their nonce and hash. Known successful operations are not
resent on resume. A send interrupted before its hash was recorded requires manual nonce/receipt
reconciliation; do not delete its journal and retry. Missing L1 state, lost safe checkpoints or
contract/schedule discrepancies stop startup rather than silently resetting either chain.

Offline checks: `just devnet snapshot test`. Opt-in live test entrypoint:
`BASE_SNAPSHOT_FORK_DIR=/srv/forks/base-test just devnet snapshot test`; optionally add
`BASE_SNAPSHOT_TEST_DENIM=1` after scheduling Denim. These commands mutate only the approved fork.
