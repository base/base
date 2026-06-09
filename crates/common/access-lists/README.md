# `base-access-lists`

A library to build and process block access lists.

## Overview

This crate provides types and utilities for tracking account and storage changes during EVM transaction execution, producing access lists that can be used by downstream consumers to understand exactly what state was read or modified.

- `AccessListBuilderDb<DB>` - A database wrapper that tracks reads and writes during transaction execution.
- `BlockAccessListBuilder` - A builder pattern for constructing access lists from tracked changes.
- `BlockAccessList` - The final access list containing all account changes, storage changes, and metadata.

## Usage

Wrap your database with `AccessListBuilderDb`, execute transactions, then call `finish()` to retrieve the builder:

```rust,ignore
use base_access_lists::{AccessListBuilderDb, BlockAccessList};
use revm::database::InMemoryDB;

// Create a wrapped database
let db = InMemoryDB::default();
let mut access_list_db = AccessListBuilderDb::new(db);

// Execute transactions, calling set_index() before each one
for (i, tx) in transactions.into_iter().enumerate() {
    access_list_db.set_index(i as u64);
    // ... execute transaction with access_list_db ...
    access_list_db.commit(state_changes);
}

// Build the access list
let builder = access_list_db.finish()?;
let access_list = builder.build(0, max_tx_index);
```

## Features

- Tracks balance, nonce, and code changes per account
- Tracks storage slot reads and writes
- Associates each change with its transaction index
- Produces RLP-encodable access lists with a commitment hash

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
