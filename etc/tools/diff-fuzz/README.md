# `base-diff-fuzz`

Differential fuzz and mainnet replay harness for Base execution clients. It sends the same
blocks to several execution clients (base/base, needlefish, Nethermind) through small adapter
binaries and reports every difference in what they compute: rejected transactions, receipts,
post-state, and every header field. The wire format between the harness and an adapter is in
[PROTOCOL.md](./PROTOCOL.md).

## Commands

Generators (`gen`, `fuzz`, `chain`, `chain-fuzz`) target every era from Regolith to Denim. Set
`BASE_DIFF_MAX_ERA` to an era name, such as `azul`, to stop at that era for a client that does
not implement later upgrades yet (chains can still cross into the next era). Everest is not
scheduled on mainnet; `BASE_DIFF_EVEREST_TIMESTAMP=<unix seconds>` (at or after Denim) schedules it
for the harness and every adapter, adds an `everest` era, and lets generators plan Base
transactions (type `0x79`: protocol, 2D and nonce-free nonces, recovered or named sender, self,
named or open payer, delegation changes, multi-phase calls), and `chain-fuzz` then also starts
chains just before Everest. `BASE_DIFF_MIN_ERA` is the lower bound matching `BASE_DIFF_MAX_ERA`.
`BASE_DIFF_NATIVE_WEIGHT` sets the native-precompile share of the traffic mix (default 14 of
about 140); `BASE_DIFF_MIN_ERA=beryl BASE_DIFF_NATIVE_WEIGHT=150` focuses a run on the native
precompiles, and `chain-fuzz` lists the `np:` features it reached under `nativeNames` in
`chain-coverage.json`.

- `run <paths>`: saved requests (JSON files, JSON-lines files, or directories of them).
- `gen --seed S --count N`: single-block generator cases `0..N`.
- `chain --seed S --chains N --length L`: multi-block synthetic chains. A chain advances only
  while every adapter agrees, on the reference's response. On a divergence or adapter failure
  the chain stops; its finding directory holds `prefix.jsonl` (the last 32 requests of the chain
  up to the failing one) and a minimized `minimized.json` of the failing block.
  System config changes reach a chain as L1 `ConfigUpdate` logs at the first block of an epoch,
  applied with base/base's `SystemConfig::update_with_receipts`. An update sets one L2-affecting
  parameter (gas limit, overhead, base fee and blob base fee scalars, EIP-1559 denominator and
  elasticity, operator fee scalar and constant, minimum base fee, DA footprint gas scalar) to an
  edge or realistic value, sets two to four at once, updates one twice in a block, or resets
  all of them to a fresh realistic config. Upgrade activation blocks start a new epoch when they
  can and usually carry an update (Denim's gas parameter scaling then applies on top), and
  bursts give up to four consecutive blocks an update each while the L1 origin lags far enough
  behind to advance every block.
- `system-config --seed S --count N`: `systemConfig` cases `0..N`, L1 `ConfigUpdate` logs applied
  to a system config. Each case is a starting config, an L1 timestamp (often within a few blocks
  of an upgrade activation, Ecotone most of all), and receipts mixing well-formed updates of
  every kind with edge values, malformed ones (data pointer, data length word, data size,
  nonzero value padding, version topic, unknown or high-bit update type, too few or too many
  topics), and logs that are not config updates (another emitter, another event, no topics,
  failed receipts); one case in five is a single well-formed update.
- `fuzz --corpus <dir> --duration 20m`: coverage-guided fuzzing (below).
- `chain-fuzz --duration 3h [--unguided]`: coverage-guided chain fuzzing (below).
- `mainnet --cache <dir> [--blocks 9101527,58500000..58500100] [--tip N] [--per-era N]`:
  validate-mode replay of Base mainnet blocks (below), tallied per fork era.
- `mainfuzz --cache <dir> --duration 8h`: coverage-guided fuzzing seeded from cached mainnet
  blocks (below).
- `coverage-names --adapter base=... [--corpus <dir>] [--main-corpus <dir> --cache <dir>]
  [--gen-cases N] --out names.txt`: the coverage feature names the reference reports for a fuzz
  corpus, a `mainfuzz` corpus, and fresh generator cases, with the number of cases hitting each
  and the total hits (`name<TAB>cases<TAB>hits`). `mainfuzz --baseline` reads the first column.
- `build-case --cache <dir> <case.json>`: prints the request a `mainfuzz` case builds, such as a
  finding's `minimized-case.json`.
- `state --launch <script> --root <dir> --node name=http,auth ...`: differential state fuzzing of
  real nodes through reorgs and restarts (below).
- `sync --launch <script> --root <dir> --source name=http,auth --target name=http,auth`: checks
  that a target node syncs over devp2p from a source node (below).
- `trie-fuzz --duration 2h [--unguided]`: coverage-guided differential fuzzing of state root
  code in process (below); `trie-run [--minimize N] <case.json>...` reruns saved trie cases.

Each takes `--adapter name=command...` once per client, `--jobs`, and `--out`. Divergences and
adapter failures go to `<out>/findings/<case id>/`: the request, `verdict.json`, and every
adapter's response.

## Coverage-guided fuzzing

`fuzz` steers case generation towards behavior the adapters have not executed yet. The harness
starts each `--coverage-from` adapter (repeatable, default the `--reference` adapter, `base`)
with `BASE_DIFF_COVERAGE=1`, and each reports coverage features with every response (see
"Optional coverage" in [PROTOCOL.md](./PROTOCOL.md)). Their features are merged, so a case that
reaches new code in any client is kept; clients other than base prefix their feature names.

A corpus case is a descriptor, not a request: a generator seed and index (which fix the fork,
block, genesis state, system config, and L1 origin) plus a list of transaction slots. Each slot
names a planner action (random traffic, a program call, a precompile call, a native precompile
call, a deposit, an EIP-7702 delegation, a deliberately invalid transaction, a deployment of a
program with edited bytecode followed by calls to it, a selector-level native precompile call
naming the precompile family, one function of any of its interface versions, the sender, and an
ABI mutation, or a call into one of the template contracts every genesis holds: a self-recursing
contract for deep call stacks, a creator for every CREATE/CREATE2 outcome, a self-destructor, and
an SSTORE refund exerciser) and the seed of its own random stream,
plus optional tweaks (gas limit towards the intrinsic cost or the per-transaction cap, value,
calldata words and bits, fee caps at the base fee, deposit mint). Optional L1 tweaks set fields
of the L1 attributes deposit. The generator rebuilds the request from the descriptor, re-planning
nonces and balances and recomputing the witness and code hashes, so every mutation yields a
well-formed request. Tweaked transactions are re-signed.

Mutations re-roll one slot's stream, change its kind, insert, remove, duplicate, or swap slots,
splice slots from another corpus case (preferring one on the same pre-state), add tweaks, edit
program bytecode, edit L1 attributes, move the slots onto another pre-state, or re-roll a native
precompile call's function, sender, ABI mutation, or arguments. A case is kept
when an adapter reports a feature, or a feature hit-count bucket, that no earlier case did.
Parents are picked by energy: the sum over their features of one over the number of cases that
hit the feature, so cases reaching rare behavior get most mutations. A share of rounds
(`--explore`) runs fresh generator cases instead. `--unguided` runs only fresh cases and still
measures coverage, as a baseline.

Every round runs on all adapters. Findings are grouped by signature (the clients that disagree
with the reference and the most specific differing field kind); the first few of each signature
are recorded and minimized: L1 tweaks, chunks of slots, tweaks, and bytecode edits are dropped
while the finding reproduces, then single transactions of the request. The minimized request,
its verdict, its responses, and its descriptor are saved as `minimized*.json` next to the
original.

`<out>/coverage.json` is rewritten every ten seconds with counters, distinct features and
feature names (per family), corpus size, cases per second, findings per signature with their
minimized reproducers, mutation operator yields, and a timeline sampled every `--sample-secs`.

```sh
cargo build --release -p base-diff-fuzz -p base-diff-adapter
target/release/base-diff-fuzz fuzz --corpus corpus --out fuzz-out --jobs 6 --duration 20m \
  --adapter base=target/release/base-diff-adapter \
  --adapter "needlefish=$HOME/ext/needlefish/zig-out/bin/base diff-adapter" \
  --adapter "nethermind=$HOME/.dotnet/dotnet $HOME/ext/nethermind/tools/artifacts/bin/BaseDiffAdapter/release/BaseDiffAdapter.dll"
```

## Mainnet replay

`mainnet` fetches blocks with `MainnetFetcher` (endpoints from `BASE_DIFF_WITNESS_RPC`,
`BASE_DIFF_BLOCK_RPC`, and `BASE_DIFF_PROOF_RPC`, comma-separated) into a cache of one validate-mode
request per block (`<number>.json`, plus `<number>.keys.json` with the witness's key preimages when
the endpoint returns them), then runs every adapter on each block. Endpoints without
`debug_getRawBlock` work too: the block is rebuilt from `eth_getBlockByNumber` and checked against
its hash and transactions root. With `BASE_DIFF_WITNESS_FROM_PROOFS=1` the witness is rebuilt
instead, for endpoints without `debug_executionWitness` (`ProofWitness` in `src/mainnet/proofs.rs`):
the accounts and slots the block reads come from `debug_traceBlockByNumber` struct logs on the
witness endpoints plus the state system logic reads outside the EVM, and their `eth_getProof` nodes
and code at the parent come from the proof endpoints. Native precompiles read storage no struct log
shows, so `BASE_DIFF_WITNESS_COMPLETER` can name an adapter command whose errors name the missing
account and slot (needlefish's do); the fetcher adds those proofs until the adapter runs the block.
A cleared slot that collapses a storage branch needs the remaining sibling node, which is fetched by
proving a searched slot whose hashed key leads into it; deletions in the account trie are not
covered, so such blocks fail with a missing node. For eras no endpoint serves witnesses or proofs
for, `BASE_DIFF_WITNESS_FROM_PRESTATE=1` builds a synthetic pre-state instead (`PrestateWitness` in
`src/mainnet/prestate.rs`, "Synthetic pre-state" in `PROTOCOL.md`): the touched accounts and slots
from archive `prestateTracer` traces on the witness endpoints, as a full trie sent as a build-mode
request with `witness.root`. Receipts, bloom and gas are checked against the canonical block; the
state root, withdrawals root and hash only between clients, and results carry `"prestate": true`.
The 256 ancestor headers come in one batch call, so the block endpoints must allow large batches
(blastapi does; drpc's free plan allows 3). Keep these blocks in their own cache directory. `--tip N` takes the last N blocks before the head,
`--per-era N` samples N blocks evenly from every era plus each activation block and its successor.
Every block's verdict is appended to `<out>/results.jsonl` (a rerun skips blocks that already agreed
and reruns the rest), and `<out>/eras.json` holds blocks, agreeing blocks, and their transactions
per era.

## Fuzzing from mainnet blocks

`mainfuzz` builds build-mode cases from cached mainnet blocks, keeps the ones that reach new
reference coverage, and records and minimizes findings like `fuzz`. Its cases are of two kinds.

- **Real** (`RealCase` in `src/mainnet/mutate.rs`): the pre-state, ancestors, header inputs, and
  system deposits of a real block, with a body drawn from the block and up to three successors:
  real transactions dropped, reordered, duplicated, and spliced across blocks, and deposits that
  replay a real call as its real sender (or another address of the window) with a different gas
  limit, value, mint, recipient, or calldata (words overwritten, bits flipped, truncated,
  extended). Base fee, gas limit, and `prevRandao` edits apply to the header. The witness is the
  union of the window's witnesses, which covers every path a successor reads at the case's
  pre-state. A mutated call can still read state no source block read; an adapter failure on a
  missing trie node is counted as a witness gap, not a finding.
- **Transplant** (`Transplant` in `src/mainnet/transplant.rs`): every account and storage slot a
  real block read, recovered from its witness and key preimages, is copied into the genesis of a
  generator case (any era from Fjord on; code starting with `0xEF` only where it can exist), and
  the block's real transactions run there verbatim, re-signed from generator keys with edits, and
  as deposits from their real senders, next to the generator's own traffic. The synthetic state
  is a complete trie, so there are no witness gaps.

Findings that match known differences are counted as `known` and not recorded: base/base alone
rejecting a transaction whose `gas * maxFeePerGas` overflows u128, and any block with a transaction
from an account at nonce `u64::MAX`. `<out>/coverage.json` holds counters, feature families,
findings, and the number of feature names beyond `--baseline`; `<out>/beyond-baseline.txt` lists
those names.

## Coverage-guided chain fuzzing

`chain-fuzz` guides `chain`-style runs with the merged coverage of the `--coverage-from` adapters
(default the reference), read for every block. A block that reaches a feature, or a hit-count
bucket, that no earlier block did keeps its chain prefix (with `--no-buckets`, only a new feature
name counts: with several clients' coverage merged, nearly every block reaches some new bucket): the descriptor (chain seed, edits by
block index, length) and, in memory, a snapshot of the chain generator after the block. Requests
are self-contained, so continuing a snapshot needs no replay. A guided run picks a kept prefix by
energy (as `fuzz` picks cases) and applies one to three edits before running 16 to 96 more
blocks:

- `reroll`: re-plans the next block from a new random stream, which the rest of the run follows;
- `configUpdate`: starts a new epoch at the block with a system config update, when the L1 origin
  can advance;
- `gap`: moves the block, and every later one, past the schedule: by seconds, an epoch, an hour,
  or a day, or exactly onto (or one second past) the next upgrade activation. Denim's gas
  parameter scaling applies when a gap moves a block onto the Denim activation;
- `fork`: any of these applied 1 to 15 blocks after the prefix instead of at its next block.

`--explore` percent of guided runs, and every `--unguided` run, start fresh chains of a
`--length` picked uniformly (default 100 to 1,000 blocks). Guided fresh chains rotate through
the eras, Regolith to Denim, so each gets an equal share; unguided ones weight recent eras as
`chain` does. At most `--max-snapshots` snapshots are held; the lowest-energy tenth is dropped
when the cap is reached, and only entries with a snapshot keep their feature lists. Findings are grouped by signature; the
first five of each are recorded like `chain` findings, with `case.json` holding the run's
descriptor, and the first two are minimized. `<out>/chain-coverage.json` is rewritten every ten
seconds with counters, distinct features and names, distinct edges per client (`edge:` for base,
`<client>:edge:` for the others), kept prefixes, operator yields, and a timeline.

## Coverage-guided JSON-RPC fuzzing

`rpc --coverage-from <node>` (repeatable) guides the JSON-RPC campaign with per-request edge
coverage from the nodes themselves. A coverage node is an opt-in build that serves
`fuzz_coverage`: no parameters, result `{"counters": N, "hits": [[index, count], ...]}` with every
edge counter that is nonzero since the previous call (an 8-bit count), after which the node
zeroes its counters. Production builds do not have the method. The builds are:

- base/base: `base-diff-node` ([`../diff-node`](../diff-node/README.md)), `base-reth-node` with
  `SanitizerCoverage` counters in the RPC, RPC type, tracer, and EVM crates.
- Nethermind: `tools/RpcCoverage/instrument.sh` builds a runner with `SharpFuzz` edge counters in
  its JSON-RPC, Facade, serialization, EVM, precompile, tracer, state, and Optimism assemblies and
  an unreferenced `fuzz` RPC plugin; start it with its `run-coverage.sh` and add `fuzz` to
  `--JsonRpc.EnabledModules`.
- needlefish: the `-Ddiff-coverage` build, which keeps the `-ffuzz` counters of the RPC, JSON,
  HTTP, and EVM functions.
- tiger: the `edge-coverage` build, which answers with the adapter protocol's coverage map
  (`{"tiger:edge:<index>": count}`) instead; the harness reads either form.

Counters are process-wide, so the harness sends each coverage node one request at a time and
reads its coverage right after every request, Engine API calls and the campaign's own extra
requests (determinism checks, minimization, filter polls) included; only the readings of
generated requests count. Workers still overlap across nodes. Features are `<node>:<edge index>`
with the hit-count bucket, merged over the coverage nodes, so a request that reaches new code in
any client is kept in `<out>/rpc-corpus/`.

Guided requests are a mix of fresh generator requests (`--explore`, default 40%) and mutants of
corpus requests picked by energy (as for `fuzz`). Mutations replace a parameter, a subtree, or an
object key with the one from a fresh request of the same family (which holds valid block numbers,
hashes, addresses, and call objects for the chain so far), splice a subtree from another corpus
request of the same method, drop or duplicate array elements and object keys, nudge hex quantities
across boundaries (including leading zeros and decimal numbers), cut, extend, or flip hex data, swap
in block tags, change a value's JSON type, switch to a sibling method with the same parameters (such
as `eth_call` to `eth_estimateGas`), or edit single characters of the body to reach parser errors.
`--unguided` sends only fresh requests and still reads coverage, as a baseline.

`<out>/rpc-coverage.json` is rewritten every round: requests and requests per second, distinct
features, distinct edges per node, corpus size, kept requests by origin, mutation operator, and
method, and a timeline with one sample per round.

## Differential trie fuzzing

`trie-fuzz` compares state root code directly, without executing transactions: every adapter gets
a series of blocks of hashed-key writes (account create, update, and delete; storage set, change,
and clear; storage wipes; recreated accounts), applies them to its trie code over its own on-disk
database, and reports the state root, storage roots, and account and storage proofs after every
block ([`TRIE_PROTOCOL.md`](./TRIE_PROTOCOL.md)). Adapters report more code paths as variants
(`<adapter>/<variant>`), and the harness adds its own from-scratch trie as `model`. Every block
of every implementation is compared field by field against the majority; proofs also have to
verify against their own roots.

A case (`TrieCase`) is the block list itself. The generator picks keys in hashed-key space so
cases aim at trie shapes: keys sharing prefixes of every length, branches collapsing to an
extension or a leaf, extensions splitting, embedded nodes, deletes down to an empty trie, many
writes under one storage trie, wide fills of hundreds of spread keys, whole subtrees cleared at
once, keys written and set back within a block or across blocks, and
unwinds (with the `revert` ops an adapter without native unwinds applies). Proof reads target
touched, untouched, and absent keys next to existing ones. Most cases keep every two keys to at
most 12 shared nibbles, which keccak keys of a real trie reach (about 2^48 hashes); a fifth go
deeper. A quarter use keys with preimages, for implementations keyed by address and slot.

Guidance is edge coverage of the coverage adapters (by default every adapter): `edge:` for
base/base (`base-diff-trie-adapter` built with `edge-coverage` and the trie crates in
`SANCOV_CRATES`), `nm:edge:` and `nf:edge:` for the others. A case is kept when it reaches an edge
name no earlier case reached and a second run of it reaches that edge again; hit counts do not
count. The second run filters edges that differ between runs of one request, such as
Nethermind's, whose previous-location state is per thread. Mutations re-roll, insert, remove,
duplicate, or swap blocks and ops, change values, move a key or slot (everywhere it appears) to
share a chosen prefix length with another, add or drop proof reads, add or drop unwinds, splice
blocks from another case, add a block that empties the trie or one storage trie, or truncate.
`--unguided` runs fresh cases only and still measures coverage.

Findings are grouped by signature: the most telling differing field kind in the first diverging
block, the implementations that differ there, and for errors the error text with numbers elided.
The first five of each are recorded in `<out>/findings/` (`case.json`, `request.json`,
`results.json`, `verdict.json`) and the first two minimized (`minimized-*`): blocks after the
divergence, whole blocks, runs of ops, proof reads, and unwinds are dropped while the signature
reproduces. Each finding counts occurrences at realistic and deep key depths and the shallowest
depth seen. Signatures of known causes (`TrieFuzzer::KNOWN`, plus `--known`) are only counted.
`<out>/trie-coverage.json` holds counters, distinct edges per client, findings, known causes,
mutation yields, and a timeline sampled every `--sample-secs`.

```sh
target/release/base-diff-fuzz trie-fuzz --duration 2h --jobs 4 --out trie-out --corpus trie-corpus \
  --adapter base=target/sancov-trie/release/base-diff-trie-adapter \
  --adapter "nethermind=dotnet $HOME/nethermind/tools/artifacts/bin/BaseDiffAdapter/instrumented-trie/BaseDiffAdapter.dll" \
  --adapter "needlefish=$HOME/needlefish/zig-out-coverage/bin/base diff-adapter"
```

## Differential state fuzzing of real nodes

`state` checks that real nodes compute and persist state identically: their state roots, the
state they serve back through reorganizations, unwinds of persisted blocks, and restarts. Every
node follows one synthetic chain over the Engine API, built block by block by the reference
adapter, and the harness starts, stops, kills, and wipes the node processes itself: the
`--launch` script is run as `<script> <name> <datadir>` and must exec node `name` there, with
`genesis.json` and `jwt.hex` in `--root`. The genesis file puts the genesis block at block zero
(with mainnet's timestamps at the original numbers), since base/base cannot reopen a datadir
whose genesis is above it.

An input is a short sequence of chain operations:

- `extend`: build one to twelve blocks on the head, with a forkchoice update after each block or
  only after the last;
- `fork`: build a sibling branch on an ancestor up to `--max-depth` blocks back (never below the
  finalized block, `--finalized-lag` behind the head) and make it canonical or leave it aside;
- `switch`: make an earlier branch tip the head again;
- `restart`: stop some or all nodes cleanly (`SIGTERM`) or kill them (`SIGKILL`) and start them on
  the same datadir; blocks a node lost are sent again, and counted;
- `badRoot`: submit the next block with a wrong state root (random, the parent's, one bit flipped,
  or a sibling's), which every node must answer `INVALID`;
- `fault`: restart every node cleanly with fault injection armed for that run: the launch script
  gets `BASE_DIFF_FAULT=seed=N,crash_after_writes=K,mode=torn|lose_unsynced` (tiger's
  `TIGER_FAULT` syntax; other clients ignore it). The node crashes at its `K`-th store write,
  tearing that write or losing the writes since the file's last fsync. When a node's log shows
  the crash line, what the operation saw on it is dropped: the harness restarts it unarmed on
  the same datadir and moves it to the head of the forkchoice update the crash interrupted (as a
  consensus node retries an unanswered update) or else the head, so the reopened chain and state
  must be intact
  (`observations` count the crashes and recoveries);
- `ancestor`: a forkchoice update naming a canonical ancestor of the head (not below finalized,
  with safe and finalized at or below it); every node must answer `VALID` with that block as the
  latest valid hash and keep its head, as base-reth does;
- `check`: read state back at several heights.

Blocks carry state-stress traffic (`StressPlan`, per operation) besides ordinary transactions:
calls to a genesis contract that runs scripts of slot writes on keys whose hashed paths share one
to five nibbles (so writes and deletions split and collapse branch nodes), deletions of existing
slots, a hot set every block rewrites, slots written and reset within a transaction or by a later
transaction of the block, dense runs of consecutive slots, value sprayed to hundreds of fresh
accounts, `CREATE2` children that write storage and self-destruct in the same transaction and are
recreated later, code blobs up to and past the maximum code size, and calls that touch empty
genesis accounts (some with storage) so EIP-161 removes them.

After every operation, and at more heights at the end of an input, the harness reads back the
header and, for the accounts and slots the block touched plus random present and absent ones,
`eth_getProof` (values checked and proofs verified against the expected roots),
`eth_getStorageAt`, `eth_getBalance`, `eth_getTransactionCount`, and `eth_getCode`, and compares
them with the generator's expected state. Payload statuses (`VALID`, or `ACCEPTED` for side
branches), forkchoice results, and the head every node reports are checked too. A read that
diverges is read again five seconds later and marked `(transient)` if it is gone.

A divergence wipes the nodes and restarts the chain; a new signature (kind, node, field, and the
validation error with hashes blanked) gets a reproducer in `<out>/findings/<signature>/`: the
genesis file, `ops.json` (the operations since the last wipe), `trace.jsonl` (every Engine API
call in order, with restarts), and `divergences.json`, plus a `minimized/` copy when
`--minimize-budget` allows replays from genesis. `state --replay ops.json` replays a reproducer
and minimizes it. The chain also starts over after `--max-blocks`.

With `--coverage-from <node>` (repeatable) the run is guided by the coverage builds above scoped
to trie, state, database, and persistence code (see the diff-node README for base/base's crate
set; needlefish takes `BASE_RPC_COVERAGE_PREFIXES`, Nethermind an instrument script over its
Trie, State, Db, Blockchain, Consensus, and Merge assemblies). Every reading of an input, Engine
API calls and state reads alike, is summed and divided by the blocks the input built (so a long
input does not reach new hit-count buckets just by being long), and an input that reaches a new
feature in any node is kept in `<out>/state-corpus/`. Guided inputs are fresh (`--explore`,
default 25%) or mutants of corpus inputs: operations inserted, deleted, duplicated, swapped,
spliced from another input, reseeded, retuned (one stress knob or a fresh plan), or tweaked
(block counts, depths, flags).
`--unguided` runs only fresh inputs and still reads coverage. `<out>/state-summary.json` holds
operation counts, blocks, wipes, findings, observations, features, edges per node, corpus size,
kept inputs by origin and operator, and a timeline.

## Execution-layer sync check

`sync` checks that a node downloads and executes a chain from a peer over the eth protocol when
it only hears forkchoice updates. The source node (`--source`, which must serve
`admin_nodeInfo`) is fed a synthetic state-stress chain of `--blocks` blocks over the Engine API,
as in `state`. Then the target (`--target`) starts on a wiped datadir with the source's enode in
`BASE_DIFF_PEERS` (the launch script passes it as trusted peers) and gets a forkchoice update to
the source's head. Once it has downloaded its first block (or after `--reorg-after-ms`), the
source reorganizes, and the target gets the new head. Later phases add a 40-block gap, a 5-block
reorg, a 30-block reorg to a lower height, and a 50-block gap across a killed restart of the
target. In each phase the target must reach `VALID` within `--timeout`. Then every height is
compared with the source (block hash, state, receipts, and transactions roots, logs bloom), plus
the balance, nonce, and code of the head block's accounts. The report (`--out`) lists, per phase,
the statuses seen, the target's height when the reorg happened, the time taken, and any mismatches.

With `--stateless-start <block|tag>` the target starts without state at that block of the source,
with the source as its execution witness upstream (`BASE_DIFF_STATELESS="<source http url> <start>"`
for the launch script; tiger takes `--stateless.upstream` and `--stateless.start`). Heights below
the first block the target serves are absent; that block must stay the same across phases, and
the report records it with the source's finalized block at launch. The target fills reads
of state it does not hold from the upstream's verified `eth_getProof`, so a refused account read
is a mismatch unless it is tiger's "state not local" refusal (code -32020) and the source cannot
prove that account at that block either; only then is it listed under `partialStateErrors`. A different value is always a mismatch.

With `--handoff` the check also covers datadir compatibility between the clients. The target
starts on a copy of the source's datadir (taken after a clean stop, without its p2p key) and
continues the chain through the phases. At the end the source's client opens a copy of the
target's datadir in `<root>/reopen` and must answer as the source did: every block, the receipts
and transactions of the last blocks, and account reads at the head and 20 and 60 blocks back.
Warnings in its log about healing, rebuilding, or unwinding are reported as mismatches. `latest` cannot pass the first
phase: the mid-sync reorg drops the start block.
