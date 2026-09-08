# `base-consensus-engine`

Consensus coordination for the co-located Base execution node.

`Engine` owns the prioritized task queue and publishes execution state updates. Insert,
consolidate, finalize, build, and synchronize tasks call `LocalEngineClient`, which holds
the execution driver, payload builder, and local database provider. These calls use Rust
values and typed responses; no Engine RPC server, authentication, or transport is involved.

Local block reads return native sealed blocks. L1 reads continue to use the configured
remote provider. Payload envelopes remain boundary formats for gossip and remote sources.

Build resolution uses the build identifier. Chain rules are validated by the execution
services, without selecting versioned Engine RPC methods. Forkchoice, validation, and
persistence retain their serialized execution order.

The `metrics` feature enables Prometheus metrics. Test utilities provide scriptable native
command responses and call recording for consensus behavior tests.
