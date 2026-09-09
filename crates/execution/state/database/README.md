# `base-execution-state-database`

Typed database transactions, cursors, tables, and persistent storage for Base.

The database interfaces and their MDBX implementation live together. Writes are grouped in
transactions and become visible atomically at commit; cursors traverse ordered keys and duplicate
values. Table codecs preserve the existing on-disk representation.

The `mdbx` feature enables the native-backed database implementation. Consumers that only need
interfaces or table definitions can leave it disabled. The safe MDBX wrapper is an internal
module; its native bindings remain a separate compilation unit. See [MDBX provenance](MDBX.md). The `test-utils` feature provides temporary
databases and fixtures. Static-file readers share the same table definitions.

The local implementation originates from Reth and retains the repository's license terms.
