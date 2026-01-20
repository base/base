# `base-fbal`

<a href="https://github.com/base/node-reth/actions/workflows/ci.yml"><img src="https://github.com/base/node-reth/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/node-reth/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

A library to build and process Flashblock-level Access Lists (FBALs).

See the [spec](../../../docs/specs/access-lists.md) for more details.

## Overview

This crate provides types and utilities for tracking account and storage changes during EVM transaction execution, producing access lists that can be used by downstream consumers to understand exactly what state was read or modified.

- `FBALBuilderDb<DB>` - A database wrapper that tracks reads and writes during transaction execution.
- `FlashblockAccessListBuilder` - A builder pattern for constructing access lists from tracked changes.
- `FlashblockAccessList` - The final access list containing all account changes, storage changes, and metadata.

## Usage

Wrap your database with `FBALBuilderDb`, execute transactions, then call `finish()` to retrieve the builder:

```rust,ignore
use base_fbal::{FBALBuilderDb, FlashblockAccessList};
use revm::database::InMemoryDB;

// Create a wrapped database
let db = InMemoryDB::default();
let mut fbal_db = FBALBuilderDb::new(db);

// Execute transactions, calling set_index() before each one
for (i, tx) in transactions.into_iter().enumerate() {
    fbal_db.set_index(i as u64);
    // ... execute transaction with fbal_db ...
    fbal_db.commit(state_changes);
}

// Build the access list
let builder = fbal_db.finish()?;
let access_list = builder.build(0, max_tx_index);
```

## Features

- Tracks balance, nonce, and code changes per account
- Tracks storage slot reads and writes
- Associates each change with its transaction index
- Produces RLP-encodable access lists with a commitment hash
