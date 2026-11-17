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
  the chain stops; its finding directory holds `prefix.jsonl` (every request of the chain up to
  the failing one) and a minimized `minimized.json` of the failing block.
- `fuzz --corpus <dir> --duration 20m`: coverage-guided fuzzing (below).

Each takes `--adapter name=command...` once per client, `--jobs`, and `--out`. Divergences and
adapter failures go to `<out>/findings/<case id>/`: the request, `verdict.json`, and every
adapter's response.

## Coverage-guided fuzzing

`fuzz` steers case generation towards behavior the reference adapter (`--reference`, default
`base`) has not executed yet. The harness starts the reference with `BASE_DIFF_COVERAGE=1`, and
it reports semantic coverage features with every response (see "Optional coverage" in
[PROTOCOL.md](./PROTOCOL.md)).

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
when the reference reports a feature, or a feature hit-count bucket, that no earlier case did.
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
