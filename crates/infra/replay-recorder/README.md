# base-replay-recorder

JSON-RPC ingress parsing into `ReplayRecord`, without executing calls. Raw params,
IDs, remote socket addresses, and transaction hashes retain their wire meaning.
Batches share the supplied session-local ID; single calls have no batch ID.
Missing params become null. Empty batches and literal CR/LF params are rejected
because the existing v1 JSONL format requires one physical line per record.

`Recorder` serves HTTP bodies up to 8 `MiB`, with a 10-second body-read timeout.
It assigns `GlobalClock` timestamps and serializes writes to `ReplayWriter`.
Responses return null results with original IDs; notifications return HTTP 204.
Invalid envelopes return HTTP 400. Success means buffered acceptance, not durable
publication. Cancelling the supplied token drains requests and closes the writer;
recording and final-flush failures propagate to the caller. Use a dedicated store
root: existing network/minute objects and minutes over 64 `MiB` are rejected.
