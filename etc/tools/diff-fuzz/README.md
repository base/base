# `base-diff-fuzz`

Differential fuzz and mainnet replay harness for Base execution clients. It sends the same
blocks to several execution clients (base/base, needlefish, Nethermind) through small adapter
binaries and reports every difference in what they compute: rejected transactions, receipts,
post-state, and every header field. The wire format between the harness and an adapter is in
[PROTOCOL.md](./PROTOCOL.md).

## Commands

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
