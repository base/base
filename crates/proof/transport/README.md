# base-proof-transport

Proof transport abstraction for proof backends.

Defines `ProofTransport`, a trait with a single `prove(bundle) -> result`
method. Implementations handle the underlying mechanics — in-process call,
vsock connection, or remote API — so callers remain transport-agnostic.

## Transports

| Transport | Purpose |
|-----------|---------|
| `NativeTransport` | In-process channel for testing and single-process provers |
| `VsockTransport` | Connect-per-request bincode over `AF_VSOCK` for Nitro Enclaves (feature `vsock`, unix-only) |
