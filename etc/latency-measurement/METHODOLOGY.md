# P2P latency measurement — methodology notes

How we measure CL gossip block-arrival latency and decompose it, to answer:
**is the current P2P (gossip) layer viable for 200 ms blocks?**

## Metrics

### 1. Absolute latency (coarse)
`abs_ms = recv_wallclock_ns/1e6 − produced_sec*1000`, per first-seen block, per region.

Limitation: the block header timestamp is **whole seconds** (pre-Holocene, `produced_millis_part = 0`),
so `abs_ms` mixes true propagation with *where inside the ~2 s slot the block was produced*. Treat as an
upper-bound proxy, not clean propagation. Precision floor ~1 s.

### 2. Cross-observer spread (the defensible, sub-second metric)
Run observers in multiple regions **concurrently**. For each block, `t0 = min(recv)` across the regions
that saw it; `rel_ms = recv − t0` = ms behind the fastest observer of that block.

Because it differences nanosecond receive times of the *same* block, the unknown publish instant cancels
and the whole-second timestamp is irrelevant → sub-second, clock-sync-limited. Requires overlapping
capture windows (same blocks seen by ≥2 regions) and tight NTP/chrony.

## Decomposition — geography vs P2P

The cross-observer spread is **geographic floor + P2P/gossip overhead**. To separate them, alongside each
observer also **timestamp flashblock arrivals** (flashblocks stream over a direct websocket; gossip travels
the multi-hop mesh). Then two different comparisons isolate the two components:

- **Across regions, same protocol** → isolates **geography**:
  `geographic_floor(APAC) = flashblock.recv[apac] − flashblock.recv[us-east]`
  (same single-hop websocket path, different distance). This is the floor *any* protocol pays, and the
  floor a websocket fast path would hit.

- **Across protocols, same node** → isolates **P2P mesh cost**:
  `p2p_overhead = gossip.recv − flashblock.recv` (same node, same block).
  The flashblock and the gossip block carry the same block from the same sequencer to the same node, so
  they travel the same distance — geography cancels, leaving only the extra time the mesh adds. This is the
  part a websocket fast path for tip blocks would eliminate.

Cross-check: `cross-observer spread ≈ geographic_floor + p2p_overhead`.

Join the flashblocks CSV to the gossip observer CSV on `block_number` (use the last flashblock seen per
height as the block-complete arrival).

## Tooling
- `../deploy/` — run `base-consensus observe` (gossip observer) per region; chrony setup; log collection.
- `flashblocks/` — flashblocks arrival recorder (sidecar) + join/decomposition notes.
- `analysis/` — offline aggregation (absolute + cross-observer spread) → charts + summaries.
