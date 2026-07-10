# P2P Block-Latency Measurement — Offline Analysis Toolkit

Throwaway, self-contained Python toolkit for the one-off P2P block-latency
measurement. Base consensus observer nodes in 6 regions each write an
append-only CSV of first-seen canonical-block gossip arrivals. We aggregate
those CSVs offline here to decide whether the P2P layer is viable for 200ms
blocks.

This directory is intentionally **not** a Cargo workspace member — pure Python,
nothing to build.

## Run it end-to-end

```sh
pip install -r requirements.txt
python gen_synthetic.py --out-dir data
python analyze.py --input-glob 'data/*.csv' --out-dir out
```

`gen_synthetic.py` fabricates realistic multi-region CSVs so `analyze.py` runs
today, before any real observer data exists. Once you have real observer CSVs,
point `--input-glob` at them and skip the generator.

If you can't install packages globally, use a venv:

```sh
python3 -m venv .venv && ./.venv/bin/pip install -r requirements.txt
./.venv/bin/python gen_synthetic.py --out-dir data
./.venv/bin/python analyze.py --input-glob 'data/*.csv' --out-dir out
```

## CSV schema (exact column order)

```
recv_wallclock_ns,block_number,block_hash,produced_sec,produced_millis_part,region,peer_id
```

- `recv_wallclock_ns` — u128 ns since UNIX epoch, NTP-synced wall clock, when
  the node first received the block over gossip.
- `block_number` — u64.
- `block_hash` — 0x-prefixed hex; the join key across regions.
- `produced_sec` — u64 block-header timestamp, **whole seconds**.
- `produced_millis_part` — u16, one of {0,200,400,600,800} (post-Holocene
  BaseTime sub-second), else 0.
- `region` — one of: us-east, us-west, eu-central, eu-north, ap-northeast,
  ap-southeast.
- `peer_id` — libp2p PeerId string (the peer we received the block from).

## Scripts

### `gen_synthetic.py`

```sh
python gen_synthetic.py --out-dir data --blocks 5000 [--holocene] [--seed 42]
```

Writes one CSV per region (`data/<region>.csv`). Same `block_hash` per block
across regions. `produced_sec` increments by 2 per block; `produced_millis_part`
is 0 (pre-Holocene default) unless `--holocene` is passed, which cycles it
through 0/200/400/600/800 on a 200ms cadence. Per-region base one-way latency
(us-east ~30ms closest to sequencer, us-west ~70, eu-central ~110, eu-north
~130, ap-northeast ~160, ap-southeast ~230ms) plus lognormal jitter and a heavy
P99 tail. Each region independently drops ~1% of blocks.

### `analyze.py`

```sh
python analyze.py --input-glob 'data/*.csv' --out-dir out
```

Loads and concatenates all matching CSVs and computes two latency views:

**(a) Absolute latency** per row:

```
abs_ms = recv_wallclock_ns/1e6 - (produced_sec*1000 + produced_millis_part)
```

Per region: count, P50, P90, P99, max. Negative values (clock skew) are
reported separately in a `negative_count` column and a stderr warning — never
silently dropped. Raw `abs_ms` is kept; a separate `abs_ms_clamped` column
clamps to `>= 0`.

**(b) Cross-observer relative spread** (the precise, defensible metric):
group by `block_hash`, take `t0 = min(recv_wallclock_ns)` across all regions for
that hash, then `rel_ms = (recv_wallclock_ns - t0)/1e6`. Per region: P50, P90,
P99, plus each region's "fastest-observer share" (how often it holds the
per-block min).

## Outputs (in `--out-dir`)

- `merged.csv` — every input row plus computed `abs_ms`, `abs_ms_clamped`,
  `rel_ms`.
- `report.html` — a single self-contained interactive Plotly file
  (`include_plotlyjs='inline'`, opens fully offline): grouped bar of P50+P99
  absolute latency per region (ordered us-east, us-west, eu-central, eu-north,
  ap-northeast, ap-southeast); box plot of `rel_ms` per region; per-region CDF
  of `abs_ms`; and a summary table.
- `latency_by_region.png` — static headline grouped bar (P50 vs P99 absolute).
  Requires the `kaleido` static-image engine; if it's missing, `analyze.py`
  prints a clear message and skips just the PNG (HTML + CSV still produced).

The summary table is also printed to stdout.

## Key caveats (read before quoting numbers)

- **Whole-second produced time makes absolute latency coarse pre-Holocene.**
  Before Holocene, `produced_millis_part` is 0, so `abs_ms` is measured against a
  timestamp truncated to the second and carries up to ~1s of quantization. Treat
  pre-Holocene absolute latency as a rough magnitude, not a precise figure.
- **`rel_ms` is the defensible number.** Cross-observer relative spread does not
  depend on the produced timestamp at all — it compares observers against the
  fastest observer of the same block. This is the metric to cite for the 200ms
  viability decision.
- **`rel_ms` accuracy depends on NTP sync.** Because `rel_ms` differences the
  wall clocks of different nodes, any inter-node clock skew shows up directly in
  the spread. Keep observers tightly NTP-synced; skew inflates (or, for the
  fastest observer, can even negate) `rel_ms`. Absolute `abs_ms` negatives are a
  direct symptom of skew and are surfaced in the report.
```
