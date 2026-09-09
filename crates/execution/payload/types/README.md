# `base-execution-payload-types`

Payload attributes, built payloads, transaction bundles, validation errors, and payload events.

This crate contains the data shared by execution, the transaction pool, RPC, and payload
construction. Bundle types cover submitted and parsed transactions, acceptance and rejection,
and metering responses. Payload job scheduling and builder services live in
`base-execution-payload-builder`.

The bundle serialization formats are preserved. `test-utils` enables bundle fixtures used by
builder and pool tests.

```sh
cargo test -p base-execution-payload-types --features test-utils
```
