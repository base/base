# `base-txpool`

Transaction pool for Base.

## Overview

Extends Reth's transaction pool with Base-specific validation and ordering for the Base node.
`BaseTransactionValidator` enforces L1 data fee checks and Base-specific validity rules.
`BaseOrdering` and `TimestampOrdering` provide customizable transaction prioritization strategies.
Also includes a `BuilderApiImpl` for builder-specific pool management.

### Pluggable builder wire format

`ValidatedTransaction<E>` is the payload of `base_insertValidatedTransaction`, the endpoint mempool
nodes use to forward transactions to a builder. `E` carries additional wire fields and defaults to
`NoExtensions`, which encodes to exactly the same bytes as a struct without the field at all — so
the default is wire-compatible in both directions with peers that predate the parameter.

Downstream node builds substitute their own payload by implementing
`ValidatedTransactionExtensions<T>`, then registering the generic monomorphizations:

```rust,ignore
use base_execution_txpool::{
    BuilderApiImpl, BuilderApiServer, ExtensionError, ValidatedTransactionExtensions,
};

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct MyExtensions {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    my_field: Option<u64>,
}

impl ValidatedTransactionExtensions<MyPooledTx> for MyExtensions {
    fn extract(tx: &ValidPoolTransaction<MyPooledTx>) -> Self { /* ... */ }
    fn apply(self, tx: MyPooledTx) -> Result<MyPooledTx, ExtensionError> { /* ... */ }
}

// Builder (ingress) side, in place of the stock `BuilderApiExtension`:
let api = BuilderApiImpl::<_, MyExtensions>::with_extensions(pool);
modules.merge_configured(api.into_rpc())?;

// Mempool (egress) forwarding is provided by `base-tx-forwarding`.
```

Extension payloads must serialize as a JSON map (a braced struct, not a unit struct) and must avoid
`u128`/`i128` fields, which `serde_json` cannot represent through `#[serde(flatten)]`.

### Per-transaction extension data

For extension data that must outlive the RPC hop and be readable during block building,
`BasePooledTransaction` carries an extension slot: `BasePooledTransaction<Cons, Pooled, Extension = ()>`,
exposed through `BasePooledTx::Extension` with `extension()` / `set_extension()`. `set_extension`
takes `&self` because the pool stores transactions behind an `Arc`, and writes once.

```rust,ignore
// At admission, from the RPC ingress path:
tx.set_extension(MyExtension { .. });

// During block building, wherever `T: BasePooledTx` is in scope:
if let Some(extension) = tx.extension() { /* ... */ }
```

The default `Extension = ()` costs one word per pooled transaction.

> **Read the extension data before `into_consensus()`.** The payload builder converts each pooled
> transaction to its consensus form partway through the build loop (see
> `crates/execution/payload/src/builder.rs`). That discards the pooled wrapper and everything
> reachable through `BasePooledTx`, including `extension()`. Reads placed after the conversion will
> not compile against the pooled type — but reads that are simply *omitted* before it will silently
> see nothing.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-txpool = { workspace = true }
```

```rust,ignore
use base_txpool::{BaseOrdering, BaseTransactionPool, BaseTransactionValidator};

let pool = Pool::new(
    BaseTransactionValidator::new(client, evm),
    BaseOrdering::default(),
    config,
);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
