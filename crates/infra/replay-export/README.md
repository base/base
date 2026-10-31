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
