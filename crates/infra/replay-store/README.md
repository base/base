# base-replay-store

Minute-keyed zstd JSONL storage for versioned replay datasets on local disk or S3.
This infrastructure crate keeps storage dependencies out of the shared record schema.
This crate targets Unix, including Linux and macOS. The recorder owns the local root;
directory-relative no-follow handles contain local reads and create-only publication,
including when an opened directory's name or final object is replaced with a symlink.

Keys are `<network>/<yyyy>/<mm>/<dd>/<hh>/<mm>.jsonl.zst`, in UTC from
`received_at_ms`. Networks contain only ASCII letters, digits, `_`, or `-`.
Writers emit `replay-record/v2` envelopes; readers also accept legacy unframed v1.
The v2 frame preserves multiline raw params byte-for-byte without physical newlines.

Create `ReplayStore::Local(root)` and `ReplayWriter::new(store, network)`.
`push` buffers one minute per writer, serializing through `to_jsonl_line`.
The next minute's first record publishes the previous minute; callers must explicitly
`close` to publish the last minute, including during idle shutdown. Drop does not flush.
Older-minute records are rejected. Same-minute records may arrive out of order.
Only one writer may own a network/minute. Atomic create-only publication rejects
collisions, including restarts; it never appends to or overwrites completed objects.
Failed publication retains buffered records for retry after repairing the backend.
Successful close is idempotent and rejects further pushes; cancellation can leave an
object already committed, so a retry can report a create-only conflict.

`read(network, start_ms..=end_ms)` lazily enumerates minute keys, skips missing objects,
and streams records in timestamp order, preserving arrival order for ties.
Reads and writes cap both compressed and uncompressed objects at 64 `MiB`, rejecting
oversized minutes instead of splitting them. Decoding and sorting use one object's
memory, not the full range. Missing minutes are not evidence of a complete capture.

For S3-compatible storage, load `aws_config::defaults(BehaviorVersion::latest())`
with `.endpoint_url(endpoint).region(Region::new(region)).load().await`, then use
`ReplayStore::S3(ReplayS3::new("s3://bucket/prefix", &config)?)`. Credentials come
from the standard SDK chain, including `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
and optional `AWS_SESSION_TOKEN`. Cloudflare R2 uses endpoint
`https://<account>.r2.cloudflarestorage.com` and region `auto`. The bucket and its
90-day lifecycle are managed separately. Requests use path-style addressing,
bounded timeouts and conditional `If-None-Match: *` publication. Incompatible servers
must fail conditional writes rather than silently overwrite objects.

Run `cargo test -p base-replay-store` with Docker running to exercise a real,
digest-pinned `MinIO` container, following the snapshotter test harness convention.
