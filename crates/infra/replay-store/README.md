# base-replay-store

Minute-keyed zstd JSONL storage for `replay-record/v1` datasets on local disk.
This infrastructure crate keeps storage dependencies out of the shared record schema.

Keys are `<network>/<yyyy>/<mm>/<dd>/<hh>/<mm>.jsonl.zst`, in UTC from
`received_at_ms`. Networks contain only ASCII letters, digits, `_`, or `-`.
Selecting this API selects `replay-record/v1`; do not mix other formats in its root.

Create `ReplayStore::Local(root)` and `ReplayWriter::new(store, network)`.
`push` buffers one minute per writer, serializing through `to_jsonl_line`.
The next minute's first record publishes the previous minute; callers must explicitly
`close` to publish the last minute, including during idle shutdown. Drop does not flush.
Older-minute records are rejected. Same-minute records may arrive out of order.
Only one writer may own a network/minute. Atomic create-only publication rejects
collisions, including restarts; it never appends to or overwrites completed objects.
Publication failures are terminal; cancellation can leave an object already committed.

`read(network, start_ms..=end_ms)` lazily enumerates minute keys, skips missing objects,
and streams records in timestamp order, preserving arrival order for ties.
Reads and writes cap both compressed and uncompressed objects at 64 `MiB`, rejecting
oversized minutes instead of splitting them. Decoding and sorting use one object's
memory, not the full range. Missing minutes are not evidence of a complete capture.
