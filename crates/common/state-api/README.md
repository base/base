# Base state API

Shared account representations and database interfaces for Base storage and execution.

Immutable and mutable reads remain distinct so execution caches can populate on reads.
Missing accounts return `None`; execution reads return empty bytecode, zero storage, and
zero block hashes for absent values. Persisted account and bytecode encodings remain
unchanged. `DatabaseCommit` applies execution changes to in-memory state, not durable
storage transactions.

The crate supports `no_std`. Database codecs and async database support are optional.
