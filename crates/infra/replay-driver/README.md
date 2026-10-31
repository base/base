# base-replay-driver

Derives shadow-build payload attributes from a trusted canonical block using the
chain's fork schedule. Only the leading deposits are forced; the builder remains
free to select transactions from its pool. Holocene and Jovian fee parameters
come from the canonical block's extra data, not its parent's.

`ShadowDriver::replay_block` runs FCU(parent, attributes), getPayload, then
newPayload(canonical) and FCU(canonical, no attributes). Call sequentially; never
import candidates. Versions: V2 before Ecotone, V3 Ecotone, V4 Isthmus, getPayload
V5/newPayload V4 Azul. `BaseEngineApi` supplies empty versioned hashes and requests.

`BuildWait::UntilTimestamp` waits until attributes.timestamp using `GlobalClock`.
Install an `OffsetClock` before builder startup, anchored before the first replay
timestamp; drive at replay speed. Without that clock, historical deadlines expire
and builds use fallback payloads.
`BuildWait::For` overrides the retrieval wait, not the builder deadline. Every RPC,
status-retry phase, and build wait is bounded by `timeout` (15s default).
SYNCING/ACCEPTED retry every 50ms; INVALID and RPC errors terminate.
Cancellation may leave an engine build/import in flight; resume from a known head.

Metrics: ordered transaction hashes, gas, builder-reported priority-fee revenue
(`block_value`, wei), and monotonic build-to-getPayload latency (including wait).
Flashblock count stays `None`; subscriptions and CLI remain separate.

`InjectionSchedule::warmup(injector, cutoff_ms)` awaits immediate delivery before first
FCU. `run(injector, cancellation)` orders remaining calls against `GlobalClock`, bounded
by `timeout`. `BuilderIngressInjector` preserves method/raw params through tx-forwarding:
transport failures retry, RPC rejections terminate. Ordered acknowledgements can make
sends late, never early. Drop aborts transport work; cancelled calls may have arrived.
Mempool ingress is reserved. Infra permits the execution dependency utilities forbid.

`BlockDiff::compare` compares inclusion, ordering (including deposits), gas and fees.
`CanonicalBlock::from_block` computes tips from complete RPC receipts, otherwise unknown.
`DiffReport::new(N, diffs)` requires N+1..M, excluding warmup N+1 from all totals.
Serialize with serde (`replay-diff/v1`) or render `markdown()`; no fidelity threshold.
`missing` includes `recording_gap`; `builder_missing` subtracts those gaps and is
unknown (`null` in JSON) when recording coverage is unknown. Optional aggregate
measurements are unknown if any scored block is unknown; empty scored windows
have known zero totals. `builder_missing` is an additive v1 field; existing fields
retain their meanings.

`ReplayDataset::load` reads an export's `manifest.json` and `records.jsonl` using
the shared export schema. It rejects unknown versions, inconsistent windows,
record counts, out-of-window records, and reserved streams before builder calls.
Exports beginning at genesis are unsupported because replay needs a parent.
Loading retains all records in memory; size exports to fit the runner's memory.
`clock_start_ms()` suggests the window start minus a 30-second startup allowance,
saturating at the Unix epoch. This is an epoch, not clock synchronization.
