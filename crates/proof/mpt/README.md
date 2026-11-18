# `base-proof-mpt`

A recursive, in-memory implementation of Ethereum's hexary Merkle Patricia Trie (MPT).

## Overview

Implements Ethereum's Merkle Patricia Trie with support for retrieval, insertion, deletion, and
root computation via RLP-encoded trie node encoding. Starting from a trie root hash, `TrieNode`
lazily fetches and caches node preimages via `TrieProvider`, enabling stateless block execution
without storing the full state. Designed as the trie backend for [`base-proof-executor`](../executor).

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-proof-mpt = { workspace = true }
```

```rust,ignore
use alloy_primitives::keccak256;
use alloy_trie::Nibbles;
use base_proof_mpt::TrieNode;

let mut root = TrieNode::new_blinded(state_root);
let path = Nibbles::unpack(keccak256(address));
// Fetches and unblinds the nodes along `path` through the `TrieProvider`.
let value = root.open(&path, &provider)?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
