# base-proof-transport

Witness transport abstraction for proof backends.

Defines `WitnessTransport`, a trait that abstracts how witness bundles reach
proof backends and how results come back. Implementations handle the
underlying channel — in-process, `AF_VSOCK`, or guest stdin — so callers remain
transport-agnostic.

## Transports

| Transport | Purpose |
|-----------|---------|
| `NativeTransport` | In-process channel for testing and single-process provers |
