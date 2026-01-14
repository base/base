# `base-jwt`

<a href="https://github.com/base/node-reth/actions/workflows/ci.yml"><img src="https://github.com/base/node-reth/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/node-reth/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

JWT secret handling and validation for Base node components.

## Overview

- **`JwtValidator`**: Validates JWT secrets against an Engine API via capability exchange.
- **`default_jwt_secret`**: Loads a JWT from a file or generates a new random secret.
- **`resolve_jwt_secret`**: Resolves JWT from file path, encoded secret, or default file.
- **`JwtError`**: Errors for loading/parsing JWT secrets.
- **`JwtValidationError`**: Errors during engine API validation.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-jwt = { git = "https://github.com/base/node-reth" }
```

Load a JWT secret:

```rust,ignore
use base_jwt::{JwtSecret, default_jwt_secret, resolve_jwt_secret};
use std::path::Path;

// Load from default file or generate new
let secret = default_jwt_secret("jwt.hex")?;

// Resolve with priority: file > encoded > default
let secret = resolve_jwt_secret(
    Some(Path::new("/path/to/jwt.hex")),
    None,
    "fallback.hex",
)?;
```

With engine validation (requires `engine-validation` feature):

```toml
[dependencies]
base-jwt = { git = "https://github.com/base/node-reth", features = ["engine-validation"] }
```

```rust,ignore
use base_jwt::JwtValidator;
use url::Url;

let validator = JwtValidator::new(jwt_secret);
let validated_secret = validator
    .validate_with_engine(Url::parse("http://localhost:8551")?)
    .await?;
```

## License

[MIT License](https://github.com/base/node-reth/blob/main/LICENSE)
