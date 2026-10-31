# base-replay-recorder

JSON-RPC ingress parsing into `ReplayRecord`, without executing calls. Raw params,
IDs, remote socket addresses, and transaction hashes retain their wire meaning.
Batches share the supplied session-local ID; single calls have no batch ID.
Missing params become null. Empty batches and literal CR/LF params are rejected
because the existing v1 JSONL format requires one physical line per record.
