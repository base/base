# `base-client-node`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Primitive types and traits for Base node runner extensions. Provides extension traits and type aliases for building modular node extensions.

## Overview

- **`BaseNodeExtension`**: Trait for node builder extensions that can apply additional wiring to the builder.
- **`ConfigurableBaseNodeExtension`**: Trait for extensions that can be constructed from a configuration type.
- **`OpBuilder`**: Type alias for the OP node builder with launch context.
- **`OpProvider`**: Type alias for the blockchain provider instance.

Configuration types are located in their respective feature crates:
- **`FlashblocksConfig`**: in `base-flashblocks` crate
- **`TxpoolConfig`**: in `base-txpool` crate

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-client-node = { git = "https://github.com/base/base" }
```

Implement a custom node extension:

```rust,ignore
use base_client_node::{BaseNodeExtension, ConfigurableBaseNodeExtension, OpBuilder};
use eyre::Result;

#[derive(Debug)]
struct MyExtension {
    // extension state
}

impl BaseNodeExtension for MyExtension {
    fn apply(self: Box<Self>, builder: BaseBuilder) -> BaseBuilder {
        // Apply custom wiring to the builder
        builder
    }
}

impl ConfigurableBaseNodeExtension<MyConfig> for MyExtension {
    fn build(config: &MyConfig) -> Result<Self> {
        Ok(Self { /* ... */ })
    }
}
```
