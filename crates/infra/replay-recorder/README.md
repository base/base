# base-replay-recorder

HTTP JSON-RPC recording into `base-replay-store`, without executing calls or tapping
the builder. Success means buffered acceptance, not durable publication.

Requests are limited to 8 `MiB`, with a 10-second body-read timeout. Calls retain
method, raw params, remote socket address, and a `GlobalClock` timestamp. Batches
retain order and share a session-local ID. Insert calls attributable to one raw
transaction carry its hash. Responses return null results with original IDs;
notifications are recorded without a response. Missing params become null.

The v1 JSONL format rejects literal CR/LF inside params. Such requests, empty
batches, and invalid envelopes return HTTP 400 without recording. Escaped line
breaks are preserved. Unknown methods are accepted without execution.

Cancel the supplied token to stop accepting connections, drain requests, and
close the writer. Recording and final publication errors propagate to the caller.
Use a dedicated capture root: the store rejects existing network/minute objects
and minutes over 64 `MiB` instead of overwriting or splitting them.
