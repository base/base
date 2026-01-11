# `base-primitives`

<a href="https://github.com/base/node-reth/actions/workflows/ci.yml"><img src="https://github.com/base/node-reth/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/node-reth/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Shared primitives and test utilities for node-reth crates.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-primitives = { git = "https://github.com/base/node-reth" }
```

For test utilities:

```toml
[dev-dependencies]
base-primitives = { git = "https://github.com/base/node-reth", features = ["test-utils"] }
```

## License

Licensed under the [MIT License](https://github.com/base/node-reth/blob/main/LICENSE).
