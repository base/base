# `base-consensus-rpc`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

## Overview

jsonrpsee trait definitions for the Base rollup node RPC API. Provides `SyncStatusApiServer`
and `SyncStatusApiClient` for the `optimism_syncStatus` method, which returns current L1 and L2
block references (unsafe, safe, and finalized heads). Enable the `client` feature for the
generated HTTP client.

## `getrandom` and the `wasm_js` feature

This crate lists `getrandom` with the **`wasm_js`** feature enabled in `Cargo.toml`, and references
it with `use getrandom as _` in `jsonrpsee.rs` (with a wasm-only `allow(unused_imports)` on that
import) so **WebAssembly** builds resolve a supported randomness backend for transitive callers
that need `getrandom`.

**Who needs it**

- **WASM targets** (`wasm32-unknown-unknown`, and similar) where this crate appears in the
  dependency graph. Transitive code (for example RPC stacks using `getrandom`) must be able to
  obtain random bytes; without `wasm_js`, `getrandom` has no viable implementation on that target
  and builds can fail.

**When it is actually used**

- On **native** OS targets, `getrandom` uses the normal platform source; the `wasm_js` feature is
  there so the **same dependency line** satisfies both native and WASM builds. It is not an
  invitation to enable unrelated WASM-only features elsewhere in the workspace.

**Avoid feature misuse**

- **Do not remove** `wasm_js` from this crate’s `getrandom` dependency just because you only run
  the node on Linux/macOS—doing so breaks WASM consumers and any CI or tooling that compiles this
  crate for `wasm32-unknown-unknown`.
- **Do not** patch or override `getrandom` downstream in a way that drops `wasm_js` while still
  depending on `base-consensus-rpc`, unless you explicitly do not support WASM at all and accept
  that breakage.
- **Do not** enable `wasm_js` on workspace-root `getrandom` for unrelated crates; per workspace
  policy, enable dependency features only on the crates that need them (this crate already does).

## RPC Methods

### `optimism_syncStatus`

Returns the current sync status of the node.

**Parameters:** None

**Returns:**
- `SyncStatus`: Current L1/L2 block references including unsafe, safe, and finalized heads.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-consensus-rpc = { git = "https://github.com/base/base" }
```

### Server Implementation

Implement the `SyncStatusApiServer` trait for your RPC handler:

```rust,ignore
use base_consensus_rpc::SyncStatusApiServer;
use base_protocol::SyncStatus;
use jsonrpsee::core::RpcResult;

struct MyRpcHandler;

#[async_trait::async_trait]
impl SyncStatusApiServer for MyRpcHandler {
    async fn op_sync_status(&self) -> RpcResult<SyncStatus> {
        // Return current sync status
        Ok(SyncStatus::default())
    }
}
```

### Client Usage

Enable the `client` feature to use the generated RPC client:

```toml
[dependencies]
base-consensus-rpc = { git = "https://github.com/base/base", features = ["client"] }
```

```rust,ignore
use base_consensus_rpc::SyncStatusApiClient;
use jsonrpsee::http_client::HttpClientBuilder;

let client = HttpClientBuilder::default()
    .build("http://localhost:8545")?;

let status = client.op_sync_status().await?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
