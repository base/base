# base-replay-export

Maps inclusive canonical L2 block ranges to replay record time windows using an
Alloy provider. This infrastructure crate is independent of the builder and
Engine API dependencies in `base-replay-driver`.

`ExportWindow::resolve(provider, N..=M, warmup)` reads the two endpoint headers
and chain ID. The record window is half-open:
`[max(0, ts(N) * 1000 - warmup_ms), ts(M) * 1000 + END_ALLOWANCE_MS)`.
`END_ALLOWANCE_MS` is a fixed 2000ms block-time allowance, not the next header.
This works at the RPC tip without requesting M+1. For subsecond chains it
intentionally overcaptures after M. Header timestamps are Unix seconds; exported
time fields are Unix milliseconds. Warmup must be an integral number of
milliseconds. RPC discovery is bounded to 30s. Missing blocks, reversed ranges,
decreasing timestamps, and arithmetic overflow fail rather than change the range.

`ExportWriter { store, network }.export(window, out)` streams from a local or S3
`ReplayStore`. The network is a storage namespace, not an RPC chain selector.
`out` must not exist. It receives `records.jsonl` containing `ReplayRecord`
objects in `replay-record/v1` format and `manifest.json` with format
`replay-export/v1`, `record_format`, `network`, flattened `ExportWindow` fields,
and `record_count`. Consumers must check both format identifiers before loading
the fixed `records.jsonl` filename. `chain_id` identifies the RPC chain.

The manifest is renamed from a temporary file only after records are flushed;
its absence means an incomplete export. Failed or cancelled exports may leave a
partial directory. Use a fresh output path to retry; existing paths are rejected.
Records stream in timestamp order across minutes, stable within timestamp ties.
Memory is bounded by one store minute plus one serialized record, not the range.
Missing minutes are skipped, not proof of recording coverage. Empty exports are
valid. Malformed or unreadable minutes fail rather than silently truncate.
