# base-consensus-safedb

Persistent safe head tracking for the Base derivation pipeline.

This crate provides traits and a [redb](https://docs.rs/redb)-backed implementation for recording
which L2 safe head was derived from each L1 block. The RPC layer queries this store to answer
`optimism_safeHeadAtL1` requests.

## Overview

- **`SafeHeadListener`** — write interface called by the derivation actor on safe head updates and reorgs.
- **`SafeDBReader`** — read interface called by the RPC layer to query historical safe heads.
- **`SafeDB`** — redb-backed implementation of both traits.
- **`DisabledSafeDB`** — no-op implementation for when safe head tracking is disabled.

## Multi-process safety

`SafeDB` is **not** multi-process safe. Only one node instance may open a given
database file at a time. Pointing two node processes at the same `safedb` path
results in undefined behaviour and likely data corruption. Process isolation is
the caller's responsibility.
