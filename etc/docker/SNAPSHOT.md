# Run a snapshot devnet

Run a local Base sequencer and validator from a mainnet snapshot, with a forked Anvil L1
and a batcher. **Experimental; use disposable data and test keys only.**

Always use `just devnet snapshot ...`. Plain `just devnet up/down` runs a different,
fresh-genesis devnet with destructive cleanup. Run commands below from the repository root.

## What you need

- Linux with a local Docker daemon, Compose v2 and Buildx; `just`, Python 3.11+, Rust,
  Foundry `cast`, and `rsync`. The RPC examples below also use `jq`.
- An Ethereum mainnet archive RPC and a Beacon API with historical blobs covering the
  snapshot's derivation history. They can share one URL. **No Base RPC is required.**
- Disk space for two independent snapshot databases, build artifacts, download cache and growth.
  You do not need to download snapshots yourself.

## 1. Set up and start

```sh
just devnet snapshot setup
just devnet snapshot up
```

Setup asks for a working directory (default `~/data/snapshot-devnet`) and L1 endpoints.
It reuses saved settings, including endpoints in `~/.config/base/l1.env`, without prompting.
It builds Base from this checkout, obtains the pinned Anvil/batcher images, downloads one
snapshot, fully copies it for the validator, and generates the fork configuration.
**No hand-written manifest, image selection or `--dir` is needed.**

`up` starts L1, both L2 nodes and the batcher, waits for safe-head advancement and wall-time
catch-up, then schedules Denim. The first run can take hours: Reth may rebuild snapshot indexes
before its RPC responds, then L2 must catch up from the snapshot's timestamp. Progress is printed.
Once `up` returns, containers remain running.

In another terminal, check services, RPC addresses and the Denim schedule:

```sh
just devnet snapshot status
```

Denim currently honors the contract's **one-hour notice**, scheduled after startup completes;
there is no activation bypass yet. The status countdown is not proof that a node activated it.

## 2. Query the validator

Run these on the Docker host. L2 RPCs are private container addresses, not localhost ports;
refresh them after restarting containers.

```sh
VALIDATOR_RPC=$(just devnet snapshot status | jq -r '.rpc_docker_host_only.validator')
cast block-number --rpc-url "$VALIDATOR_RPC"
cast rpc --rpc-url "$VALIDATOR_RPC" eth_getBlockByNumber latest false
cast rpc --rpc-url "$VALIDATOR_RPC" eth_getBlockByNumber safe false
```

Use this URL in your integration tests. Submit transactions to the sequencer, then query the
validator; unsafe gossip can arrive before the block becomes safe through L1 batching.

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
from `cast send` with `cast receipt --rpc-url "$VALIDATOR_RPC" <transaction-hash>`.
Do not use production keys: this fork retains mainnet chain IDs.

## 4. Run the full verification (optional, disruptive)

```sh
just devnet snapshot verify
```

Start promptly after `up`, **before Denim activation**, and do not run other tests concurrently.
It checks deposits, transactions, validator safe hashes/state roots, batch blobs, the Denim
transition and 200ms cadence, clean restarts, and forced L2 interruption/recovery. It writes
`verification.json` in the fork directory only after all checks pass. A missed transition
requires a fresh fork to test fully; the verifier will not reset your data or silently skip it.

`just devnet snapshot test` runs offline launcher tests instead; it does not verify a running
devnet unless you explicitly set `BASE_SNAPSHOT_FORK_DIR` to opt into the live verifier.

## Stop, resume, or recover an interrupted run

```sh
just devnet snapshot down  # stops services; keeps all data
just devnet snapshot up    # resumes the same fork
```

- Interrupted during setup? Rerun `setup`; it resumes unfinished work without repeating completed
  builds, downloads or copies. An incomplete download rechecks existing files, which can be slow.
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

Under the working directory, `builder/` and `validator/` are writable databases; `input.json`
is generated setup input; `fork/manifest.json` records the fork, images, accounts and schedule.
`fork/` also holds keys, endpoint credentials and L1/consensus state. Keep these files for resume.
The selected fork is saved in `~/.config/base/snapshot-devnet.json` (or `$XDG_CONFIG_HOME/base/`).

`just devnet snapshot` lists all tasks; append `--help` to a task for its options.
For existing independently writable snapshot copies, use the advanced `init --config ...
--dir ... --allow-write` entry point rather than asking setup to overwrite them.
Implementation: [launcher](../scripts/devnet/snapshot_devnet.py),
[Compose services](docker-compose.snapshot.yml), [Just recipes](../just/snapshot.just).
Do not run Compose directly: it skips the launcher's initialization and recovery checks.
