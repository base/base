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
Flashblock count stays `None`; subscriptions, injection and CLI remain separate.

`BlockDiff::compare` compares inclusion, ordering (including deposits), gas and fees.
`CanonicalBlock::from_block` computes tips from complete RPC receipts, otherwise unknown.
`DiffReport::new(N, diffs)` requires N+1..M, excluding warmup N+1 from all totals.
Serialize with serde (`replay-diff/v1`) or render `markdown()`; no fidelity threshold.
`missing` includes `recording_gap`; `builder_missing` subtracts those gaps and is
unknown (`null` in JSON) when recording coverage is unknown. Optional aggregate
measurements are unknown if any scored block is unknown; empty scored windows
have known zero totals. `builder_missing` is an additive v1 field; existing fields
retain their meanings.
