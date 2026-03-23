# `base-reth-cli`

Reth-specific CLI utilities for Base execution layer binaries.

## Overview

- **`init_reth!`**: Initializes Reth's global version metadata for P2P identification and logging.

## Usage

```toml
[dependencies]
base-reth-cli = { git = "https://github.com/base/base" }
```

```rust,ignore
fn main() {
    base_reth_cli::init_reth!();
}
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
