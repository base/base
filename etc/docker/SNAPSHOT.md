# Run a snapshot devnet

Run a local Base sequencer, optionally with a validator, from mainnet snapshot data, with a forked
Anvil L1 and a batcher. **Experimental; use disposable data and test keys only.**

Always use `just devnet snapshot ...`. Plain `just devnet up/down` runs a different,
fresh-genesis devnet with destructive cleanup. Run commands below from the repository root.

## What you need

- Linux with a local Docker daemon, Compose v2 and Buildx; `just`, Python 3.11+, Rust,
  Foundry `cast` and `forge`, and `rsync` for downloaded snapshot copies.
  The RPC examples below also use `jq`.
- An Ethereum mainnet archive RPC and a Beacon API with historical blobs covering the
  snapshot's derivation history. They can share one URL. **No Base RPC is required.**
- Either nothing (setup downloads a snapshot), or an existing Base mainnet Reth datadir.
- Disk space for the databases you use, build artifacts and growth. Setup also needs space for
  two independent snapshot copies and its download cache.

## 1. Set up and start

Pick one path. Both build Base from this checkout, obtain and pin the Anvil/batcher images and
snapshot inspector, generate the fork configuration, and prompt only for missing L1 execution and
Beacon endpoints, reusing saved settings and `~/.config/base/l1.env`. **No hand-written manifest,
image selection, `--config` or `--allow-write` is needed.**

**Download a snapshot (sequencer + validator):**

```sh
just devnet snapshot setup
just devnet snapshot up
```

Setup asks for a working directory (default `~/data/snapshot-devnet`), downloads one snapshot,
and fully copies it so the sequencer and validator have independent databases.

**Use an existing Reth datadir in place (sequencer only by default):**

```sh
just devnet snapshot init --dir /mnt/external/denim/data
just devnet snapshot up
```

This runs Anvil, the sequencer and the batcher, using that datadir for L2 without copying it. To add a
validator, pass `--validator-dir /path/to/other-datadir`: a separate existing datadir from the
same snapshot with the same head, also used in place. Fork state goes to the sibling
`<datadir-name>-devnet/fork`, here `/mnt/external/denim/data-devnet/fork`.

- **Cleanly stop the mainnet node first.** Init turns each datadir into a writable local fork;
  never run it against mainnet again.
- Storage v2 is detected automatically. Pruning settings in `reth.toml` are kept and `--full` is
  not added. Pruning distances count blocks: 1,339,200 blocks is about 31 days of history before
  Denim but only about 3.1 days after it, at 200ms blocks.
- A datadir stopped at the tip may hold unsafe blocks whose batches are not yet in finalized L1;
  init then refuses it. Wait for L1 finality and rerun the same `init`; no download or copy repeats.
  Startup later repairs or rederives as needed and catches up the time since the node stopped.

**Both paths:** the selected fork is remembered, so `up`, `status` and `down` take no arguments.
`up` starts L1, the L2 nodes and the batcher, waits for safe-head advancement and wall-time
catch-up, then schedules Denim. The first run can take hours: Reth may rebuild snapshot indexes
before its RPC responds, then L2 must catch up from the snapshot's timestamp. Progress is printed.
Once `up` returns, containers remain running.

In another terminal, check services, RPC addresses and the Denim schedule:

```sh
just devnet snapshot status
```

New devnets automatically schedule Denim **about 60 seconds after wall-time catch-up**, without
the production contract's one-hour notice. They use a local MockProtocolVersions contract seeded
with the historical schedule and minimum version. Node activation behavior is unchanged; the
production contract's scheduling restrictions are not tested. Existing experiments retain their
saved contract and schedule on resume. The status countdown is not proof that a node activated Denim.

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

- Interrupted during setup or init? Rerun the same command; it resumes unfinished work without
  repeating completed builds, downloads or copies. An incomplete download rechecks existing
  files, which can be slow.
- Interrupted during `up`? Let shutdown finish, then rerun `up`. Data is preserved, but unfinished
  index repair may repeat. Do not redownload or recopy.
- Waiting for execution RPC? Reth may still be repairing indexes. This wait has no deadline while
  the container runs. Catch-up waits while heads advance; its default stall budget is two hours.
- Denim scheduling failed after startup? The fork stays running; retry `just devnet snapshot schedule-denim`.
- Existing initialized fork? Select it with `just devnet snapshot setup --dir /path/to/fork`.
- New experiment? Use `just devnet snapshot setup --workdir /path/to/new-directory`.
  Resuming an existing fork keeps its pinned images; it does not rebuild from newer source.

Never copy databases while nodes are using them, share a datadir between nodes, or reconnect
locally modified copies to production. Do not expose these RPCs publicly or share `keys.json`,
`upstreams.json`, or raw Docker logs/configuration that may contain credentials.

## Where things live

With `setup`, `builder/` and `validator/` under the working directory are writable databases and
`input.json` is generated setup input. With `init`, your datadirs are the databases and state
lives in the sibling `<datadir-name>-devnet/`. Either way, `fork/manifest.json` records the fork,
images, accounts and schedule; `fork/` also holds keys, endpoint credentials and L1/consensus
state. Keep these files for resume. The selected fork is saved in
`~/.config/base/snapshot-devnet.json` (or `$XDG_CONFIG_HOME/base/`).

`just devnet snapshot` lists all tasks; append `--help` to a task for its options.
Implementation: [launcher](../scripts/devnet/snapshot_devnet.py),
[Compose services](docker-compose.snapshot.yml), [Just recipes](../just/snapshot.just).
Do not run Compose directly: it skips the launcher's initialization and recovery checks.
