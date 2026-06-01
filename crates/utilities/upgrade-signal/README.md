# `base-upgrade-signal`

Shared utilities for observing network upgrade activation signals from L1.

The crate is intentionally observability-only. It reads an L1 contract interface, records the
announced activation timestamp and expected protocol version, and reports when an L2 timestamp
crosses the announced activation timestamp.
