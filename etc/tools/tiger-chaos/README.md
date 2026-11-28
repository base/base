# `base-tiger-chaos`

Chaos testing and Jepsen-style history checking of tiger's database (tiger is the Base execution
client in `tiger/`, which reads and writes base-reth's datadir format). It replays a chain
base-reth accepted into tiger while killing tiger, arming store faults, and flapping the
forkchoice, with reader threads checking every JSON-RPC read against the history of what the
driver did. base-reth is the oracle.

## Recording

`record` runs base-reth (launch name `base`) behind the diff-fuzz state generator
(`base-diff-fuzz`'s `StateSession`): extensions, forks, branch switches and blocks with a wrong
state root, with the state-stress traffic, until the canonical chain reaches `--blocks`. It
writes a recording directory:

- `chain.json`: every Engine API call in order, every block (number, hash, parent, state root,
  transaction hashes, the accounts and slots it touched), and base-reth's
  `eth_getBlockByNumber` and `eth_getBlockReceipts` answers for the final canonical chain;
- `genesis.json`;
- `oracle-datadir/`: base-reth's datadir at the end, stopped cleanly;
- `snapshot-datadir/`: base-reth's datadir halfway, stopped cleanly, with the step it reached.

## Chaos runs

`chaos` replays recordings into tiger, one run after another. A run starts tiger on a wiped
datadir; or, in `--snapshot-percent` of runs, on a copy of base-reth's snapshot datadir (the
base-reth to tiger handoff); or, in `--base-crash-percent` of runs, on the datadir base-reth left
when it followed the recording from genesis and was killed (SIGKILL) up to 300 ms into a random step
(tiger heals base-reth's crash leftovers). The persistence threshold is random (1 to 70), the
same for both clients. Between steps, at
`--intensity` actions per 1000 steps, it:

- kills tiger with SIGKILL between steps, or from another thread up to 400 ms into the next
  steps (inside persistence, unwinds, and heals);
- stops it with SIGTERM, or with SIGTERM and then SIGKILL up to 1.5 s later (inside the
  shutdown drain);
- restarts it with a `TIGER_FAULT` store fault armed (`--fault-modes`): torn writes, lost
  unsynced writes (power loss), full disk, and the other modes the tiger build supports;
- flaps the forkchoice between branch tips above the finalized block, then returns.

In `--duplicate-percent` of Engine API calls the same call is also sent from another thread at
the same time, as a consensus node retrying after its own timeout; both must succeed.

A killed or crashed tiger is restarted unarmed and the blocks it lost are resent, as a consensus
node does. In `--crash-handoff-percent` of crashes base-reth first opens a copy of the crashed
datadir and must reopen on a block tiger had as canonical, with state that proves against it.

After every restart, before readers are let in, the driver checks the reopened head (after a
clean stop it must be the last head; after a crash, a block canonical under some head tiger
had), the block hash at every height (or a sample), and proofs at the head and a few ancestors
against the recorded state roots.

### History checking

`--readers` threads read concurrently: the head, safe and finalized blocks, blocks by number,
`eth_getProof` at `latest`, at a number and at a block hash, plain balance, nonce and storage
reads at a block hash, receipts, and transactions by hash. The driver logs every forkchoice
update with its send and acknowledgement times, and every restart as a new epoch. A read is
checked against the forkchoice states that could have been current while it ran: the last one
acknowledged before it started, plus every update sent before it finished and not acknowledged
before it started (an update naming an ancestor of the head keeps the head, as in base-reth).
Violations include a head or tag outside that set, a block by number off every allowed chain, a
proof whose root is not an allowed block's state root (half-committed or stale state), a proof
that does not verify, plain reads that disagree with a verified proof of the same block, a
canonical block, proof or transaction that went missing, and receipts and `eth_getLogs` by block
hash that differ from base-reth's.

On a datadir base-reth wrote, its history below a block that deleted a storage-holding account is
wrong (finding B1: base-reth 2b20e42's storage-v2 changesets leave the wiped slots out; tiger
keeps them in its own `TigerWipedStorage` table, so only a datadir tiger wrote has them).
Recordings flag such blocks (`wipesStorage`). Runs on base-reth's datadir skip the proof roots of
blocks that base-reth itself answers wrong and a later flagged block at or below base-reth's tip
explains; any other wrong root is still a violation.

### End of a run

tiger moves to the final head, stops with SIGTERM (it must persist through the head) and must
reopen there; every block and its receipts must equal base-reth's answers and proofs at several
heights must verify. Then base-reth opens a copy of tiger's datadir (launch name `handoff`) and
must answer the same, without heal or unwind warnings in its log.

Each run writes `<root>/run-<n>/`: `report.json` (events, read counts, violations),
`actions.json` (the driver's actions by step, to reproduce), `timeline.json`, and tiger's log.
A passing run's datadir is deleted. `--summary` is rewritten after every run with totals.

## Usage

```sh
cargo build --release -p base-tiger-chaos -p base-diff-adapter -p base-diff-node
(cd tiger && cargo build --release)
# The base launch reads launch.env from the root.
mkdir -p rec-root && printf 'PORT_OFFSET=200\nBASE_NODE_BIN=%s\n' "$PWD/target/release/base-diff-node" > rec-root/launch.env
target/release/base-tiger-chaos record --seed 1 --blocks 300 \
  --adapter base=target/release/base-diff-adapter --node base=http://127.0.0.1:18745,http://127.0.0.1:18751 \
  --launch etc/tools/tiger-chaos/launch.sh --root rec-root --out rec-1
target/release/base-tiger-chaos chaos --recording rec-1 --root chaos-root --launch etc/tools/tiger-chaos/launch.sh \
  --tiger-bin tiger/target/release/tiger --base-bin target/release/base-diff-node --hours 4 --summary summary.json
```
