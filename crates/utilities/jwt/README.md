# `base-jwt`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

JWT secret handling and validation for Base node components.

## Overview

- **`JwtValidator`**: Validates JWT secrets against an Engine API via capability exchange.
- **`JwtSecretReader`**: Reads and writes secret files with `read_from_path` and `write_to_path`.
  `default_jwt_secret` loads a file in the current directory or generates and writes a new random
  secret. `resolve_jwt_secret` prioritizes a file path, then an encoded secret, then the default file.
- **`JwtError`**: Errors for loading/parsing JWT secrets.
- **`JwtValidationError`**: Errors during engine API validation.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-jwt = { git = "https://github.com/base/base" }
```

Load a JWT secret:

```rust,ignore
use alloy_rpc_types_engine::JwtSecret;
use base_jwt::JwtSecretReader;
use std::path::Path;

// Load from default file or generate new
let jwt_secret: JwtSecret = JwtSecretReader::default_jwt_secret("jwt.hex")?;

// Resolve with priority: file > encoded > default
let jwt_secret = JwtSecretReader::resolve_jwt_secret(
    Some(Path::new("/path/to/jwt.hex")),
    None,
    "fallback.hex",
)?;
```

With engine validation (requires `engine-validation` feature):

```toml
[dependencies]
base-jwt = { git = "https://github.com/base/base", features = ["engine-validation"] }
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

[MIT License](https://github.com/base/base/blob/main/LICENSE)
