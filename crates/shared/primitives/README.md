# `base-primitives`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Shared primitives, flashblock types, and test utilities for base crates.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-primitives = { git = "https://github.com/base/base" }
```

For flashblock types:

```toml
[dependencies]
base-primitives = { git = "https://github.com/base/base", features = ["flashblocks"] }
```

For test utilities:

```toml
[dev-dependencies]
base-primitives = { git = "https://github.com/base/base", features = ["test-utils"] }
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
