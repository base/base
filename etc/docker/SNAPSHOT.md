# Run a snapshot devnet

Run a local Base sequencer, optionally with a validator, from mainnet snapshot data, with a forked
Anvil L1 and a batcher. **Experimental; use disposable data and test keys only.**

Always use `just devnet snapshot ...`. Plain `just devnet up/down` runs a different,
fresh-genesis devnet with destructive cleanup. Run commands below from the repository root.

## What you need

- Linux with a local Docker daemon, Compose v2 and Buildx; `just`, Python 3.11+, Rust,
  Foundry `cast` and `forge`. Only the all-in-one `setup` path requires `rsync`.
  The RPC examples below also use `jq`.
- An Ethereum mainnet archive RPC and a Beacon API with historical blobs covering the
  snapshot's derivation history. They can share one URL. **No Base RPC is required.**
- A snapshot from `download`, or an existing Base mainnet Reth datadir.
- Disk space for the databases you use, build artifacts and growth. Setup also needs space for
  two independent snapshot copies and its download cache.

## 1. Download, initialize, then start (recommended)

**Build the local Base image** from this checkout. You can do this before downloading any data:

```sh
docker buildx bake -f etc/docker/docker-bake.hcl base \
  --set base.args.PROFILE=release --load
```

**Download one snapshot** into an empty datadir (skip this if you already have one):

```sh
just devnet snapshot download --dir ~/data/snapshot-devnet/sequencer
```

This uses the locally built `base:local` image, downloads one Base mainnet datadir, and stops.
It does not copy a validator, ask for L1 credentials, generate fork configuration, or change the
selected devnet. It pins the image and snapshot manifest in the destination; rerun the same command
to resume an interrupted download. A completed download is a no-op on retry. Keep
`snapshot-download.json` and `download-manifest.json` with the data. Do not download into a datadir
owned by another downloader or a running node.

The default download concurrency is 16; add `--download-concurrency 32` to increase it. The command
keeps 1,339,200 blocks of transaction, receipt and state history, matching the published pre-Denim
full snapshot. It does **not** use `--archive` or `--full` (the latter currently selects 10× more
history). Base's downloader writes the matching pruning configuration to `reth.toml`.

**Initialize that datadir, then start the devnet** (sequencer only by default):

```sh
just devnet snapshot init --dir ~/data/snapshot-devnet/sequencer
just devnet snapshot up
```

For an existing mainnet datadir, use its path in `init --dir` directly. **`--dir` here is the database
directory, not the configuration directory.** Init builds Base from this checkout (reusing Docker's
build cache), obtains and pins the Anvil/batcher images and snapshot inspector, and generates fork
configuration. It prompts only for missing L1 execution and Beacon endpoints, reusing saved settings
and `~/.config/base/l1.env`. No `setup`, hand-written manifest, `--config` or `--allow-write` is needed.

`up` runs Anvil, the sequencer and the batcher, using that datadir for L2 without copying it. To add a
validator, pass `--validator-dir /path/to/other-datadir`: a separate existing datadir from the
same snapshot with the same head, also used in place. Fork state goes to the sibling
`<datadir-name>-devnet/fork`, here `~/data/snapshot-devnet/sequencer-devnet/fork`.

- **Cleanly stop the mainnet node first.** Init turns each datadir into a writable local fork;
  never run it against mainnet again.
- Storage v2 is detected automatically. Pruning settings in `reth.toml` are kept and `--full` is
  not added. Pruning distances count blocks: 1,339,200 blocks is about 31 days of history before
  Denim but only about 3.1 days after it, at 200ms blocks.
- A datadir stopped at the tip may hold unsafe blocks whose batches are not yet in finalized L1;
  init then refuses it. Wait for L1 finality and rerun the same `init`; no download or copy repeats.
  L2 catches up the time since the node stopped after `up` starts sequencing.

**Optional all-in-one path (sequencer + validator):** `just devnet snapshot setup` builds images,
downloads one snapshot and fully copies it into independent builder and validator databases before
initializing both. Its default working directory is `~/data/snapshot-devnet`. Do not run it over the
separate download above; use `init` for that datadir instead.

The selected fork is remembered after initialization, so `up`, `status` and `down` take no arguments.
`init` (also run by `setup`) does the one-time work: repairs/inspects the snapshot, discovers the
L1 fork boundary, temporarily starts nodes to derive through it, configures the local signer and
batcher, and schedules Denim. It saves L1 state and stops the services before returning. Snapshot
index repair and initial derivation can take hours; progress is printed and interrupted work resumes.

`up` restores L1, enables mining, starts L2 and the batcher, and returns once their startup RPCs
respond and sequencing is enabled. It does **not** inspect snapshots, wait for recorded checkpoints,
safe-head advancement or wall-time catch-up, or reschedule Denim. Containers keep running and
catch up in the background; returning from `up` does not mean L2 is at wall time.

In another terminal, check services, RPC addresses and the Denim schedule:

```sh
just devnet snapshot status
```

New devnets schedule Denim for **wall clock + about 60 seconds during init**, without the production
contract's one-hour notice. Activation follows **L2 timestamps**, not the launcher's countdown:
an old snapshot must build up to that timestamp, and starting later does not move the schedule.
They use a local MockProtocolVersions contract seeded with the historical schedule and minimum
version. Node activation behavior is unchanged; the production contract's scheduling restrictions
are not tested. Existing experiments retain their saved contract and schedule on resume.
The status countdown is not proof that a node activated Denim.

## 2. Query an L2 node

Run these on the Docker host. L2 RPCs are private container addresses, not localhost ports;
refresh them after restarting containers. This selects the validator when present, otherwise the sequencer.

```sh
L2_RPC=$(just devnet snapshot status | jq -r '.rpc_docker_host_only | .validator // .sequencer')
cast block-number --rpc-url "$L2_RPC"
cast rpc --rpc-url "$L2_RPC" eth_getBlockByNumber latest false
cast rpc --rpc-url "$L2_RPC" eth_getBlockByNumber safe false
```

Use this URL in your integration tests. Submit transactions to the sequencer. If a validator is
configured, query it too; unsafe gossip can arrive before the block becomes safe through L1 batching.

The sequencer enables HTTP `eth,net,web3,debug,trace,miner` and WebSocket
`eth,net,web3,debug,trace`. Use its private WebSocket address for RPC calls and subscriptions:

```sh
SEQUENCER_WS=$(just devnet snapshot status | jq -r '.rpc_docker_host_only["sequencer-ws"]')
cast rpc --rpc-url "$SEQUENCER_WS" eth_blockNumber
```

## 3. Send a test transaction

Fund the generated throwaway user through the local L1 portal, then send on L2:

```sh
just devnet snapshot deposit
FORK=$(jq -r '.directory' "${XDG_CONFIG_HOME:-$HOME/.config}/base/snapshot-devnet.json")
SEQUENCER_RPC=$(just devnet snapshot status | jq -r '.rpc_docker_host_only.sequencer')
USER_ADDRESS=$(jq -r '.accounts.user' "$FORK/manifest.json")
cast balance --rpc-url "$SEQUENCER_RPC" "$USER_ADDRESS"
```

Repeat the balance query until the deposit reaches L2 and the balance is nonzero, then:

```sh
cast send --rpc-url "$SEQUENCER_RPC" --chain-id 8453 \
  --private-key "$(jq -r '.user' "$FORK/keys.json")" --gas-limit 21000 --value 1 \
  0x000000000000000000000000000000000000bEEF
```

`deposit` funds 1 ETH once; repeating it does not top up the account. Use the transaction hash
from `cast send` with `cast receipt --rpc-url "$L2_RPC" <transaction-hash>`.
While L2 is catching up, deposits wait for L2 to reach their L1 origin; `up` returning does not
guarantee low deposit latency.
Do not use production keys: this fork retains mainnet chain IDs.

## 4. Run the full verification (optional, disruptive)

```sh
just devnet snapshot verify
```

Start promptly after `up`, **before Denim activation**, and do not run other tests concurrently.
It checks the roles present: deposits, transactions, validator safe hashes/state roots, batch
blobs, the Denim transition and 200ms cadence, clean restarts, and forced L2
interruption/recovery. On a sequencer-only fork it reports that no independent validator checks
ran. It writes `verification.json` in the fork directory only after all checks pass. A missed
transition requires a fresh fork to test fully; the verifier will not reset your data or silently
skip it. To avoid a manual delay on a fresh fork, run `just devnet snapshot up && just devnet snapshot verify`.
Deposit derivation can still consume the short lead time; verification fails if it misses the transition.

`just devnet snapshot test` runs offline launcher tests instead; it does not verify a running
devnet unless you explicitly set `BASE_SNAPSHOT_FORK_DIR` to opt into the live verifier.
With Foundry `anvil` and `forge` installed, `BASE_SNAPSHOT_TEST_ANVIL=1 just devnet snapshot test`
also tests fast-Denim contract seeding, scheduling and state restoration on a disposable local
Anvil. This does not start L2 nodes or prove Denim activation.

## Stop, resume, or recover an interrupted run

```sh
just devnet snapshot down  # stops services; keeps all data
just devnet snapshot up    # resumes the same fork
```

**Stop and forget the selected devnet**, so a later bare `up` refuses to start anything:

```sh
just devnet snapshot clear
```

This stops the selected fork (and any saved pending initialization), then removes only the saved
selection, not databases, fork state, L1 credentials or stopped containers. It is safe to repeat;
if shutdown fails or the fork is busy, the selection stays available for retry. Services have no
automatic restart policy. To deliberately select the fork again,
use `just devnet snapshot setup --dir /path/to/fork`; `up --dir /path/to/fork` also bypasses the
selection. `clear` prevents accidental bare `up`; it does not delete the experiment.

- Interrupted during setup or init? Rerun the same command; it resumes unfinished work without
  repeating completed builds, downloads or copies. An incomplete download rechecks existing
  files, which can be slow.
- Interrupted during `up`? Let shutdown finish, then rerun `up`. Data is preserved, but unfinished
  index repair may repeat. Do not redownload or recopy.
- Waiting for execution RPC? Reth may still be repairing indexes. This wait has no deadline while
  the container runs. Init's boundary-derivation wait has a two-hour stall budget, reset by progress.
- Denim scheduling failed during init? Rerun the same `init` or `setup`; committed operations are reused.
- Existing fork? Select it with `just devnet snapshot setup --dir /path/to/fork`. This also completes
  initialization for older forks that had not finished bootstrap and scheduling. Completed forks
  keep their state and schedule without repeating initialization.
- New experiment? Use `just devnet snapshot setup --workdir /path/to/new-directory`.
  Resuming an existing fork keeps its pinned images; it does not rebuild from newer source.

Never copy databases while nodes are using them, share a datadir between nodes, or reconnect
locally modified copies to production. Do not expose these RPCs publicly or share `keys.json`,
`upstreams.json`, or raw Docker logs/configuration that may contain credentials.

## Where things live

With `download`, `--dir` contains the database and download journal only; no fork is selected.
With `setup`, `builder/` and `validator/` under the working directory are writable databases and
`input.json` is generated setup input. With `init`, your datadirs are the databases and state
lives in the sibling `<datadir-name>-devnet/`. Either way, `fork/manifest.json` records the fork,
images, accounts and schedule; `fork/` also holds keys, endpoint credentials and L1/consensus
state. Keep these files for resume. The selected fork is saved in
`~/.config/base/snapshot-devnet.json` (or `$XDG_CONFIG_HOME/base/`).

`just devnet snapshot` lists all tasks; append `--help` to a task for its options.
Implementation: [launcher](../scripts/devnet/snapshot_devnet.py),
[Compose services](docker-compose.snapshot.yml), [Just recipes](../just/snapshot.just).
Use the launcher: Compose alone leaves Anvil mining and L2 sequencing disabled.
