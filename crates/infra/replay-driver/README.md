# base-replay-driver

Derives shadow-build payload attributes from a trusted canonical block using the
chain's fork schedule. Only the leading deposits are forced; the builder remains
free to select transactions from its pool. Holocene and Jovian fee parameters
come from the canonical block's extra data, not its parent's.

`ShadowDriver::replay_block` runs FCU(canonical parent, derived attributes),
getPayload, then newPayload(canonical block) and FCU(canonical head, no attributes).
Call it sequentially for each canonical block; candidates are recorded but never
imported or selected. Engine versions follow Ecotone (V3), Isthmus (V4), and
Azul (getPayload V5/newPayload V4), with V2 retrieval before Ecotone. Base's
engine client supplies empty versioned hashes and execution requests.

`BuildWait::UntilTimestamp` waits until attributes.timestamp using `GlobalClock`.
Install an `OffsetClock` before builder startup, anchored before the first replay
timestamp, and drive blocks at replay speed: historical timestamps without that
clock leave the builder deadline in the past and produce only fallback payloads.
`BuildWait::For` overrides the retrieval wait, not the builder deadline. Every RPC,
status-retry phase, and build wait is bounded by `ShadowDriver::timeout` (15s by
default). SYNCING/ACCEPTED retry every 50ms; INVALID and RPC errors terminate.
Cancellation may leave an engine build/import in flight; resume from a known head.

Candidate metrics include ordered transaction hashes, gas used, and monotonic
build-to-getPayload latency (`std::time::Instant`, including configured wait).
Exact coinbase fees need receipts and remain `None`; flashblock count remains
`None` until subscriptions are wired. Transaction injection, comparison reports,
and a replay CLI are separate work.
