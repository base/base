//! JWT secret loading and generation utilities.

use std::{fs::File, io::Write, path::Path};

use alloy_rpc_types_engine::JwtSecret;

use crate::JwtError;

/// Reads a JWT secret from the specified file path.
///
/// The file should contain a hex-encoded JWT secret.
pub fn read_jwt_secret(path: impl AsRef<Path>) -> Result<JwtSecret, JwtError> {
    let content = std::fs::read_to_string(path.as_ref())
        .map_err(|e| JwtError::IoError(format!("Failed to read JWT secret file: {e}")))?;
    JwtSecret::from_hex(content).map_err(|e| JwtError::ParseError(e.to_string()))
}

/// Attempts to read a JWT secret from a file in the current directory.
/// Creates a new random secret if the file doesn't exist.
///
/// # Arguments
/// * `file_name` - The name of the JWT file (e.g., "jwt.hex", "l2_jwt.hex")
pub fn default_jwt_secret(file_name: &str) -> Result<JwtSecret, JwtError> {
    let cur_dir = std::env::current_dir()
        .map_err(|e| JwtError::IoError(format!("Failed to get current directory: {e}")))?;

    std::fs::read_to_string(cur_dir.join(file_name)).map_or_else(
        |_| {
            let secret = JwtSecret::random();

            if let Ok(mut file) = File::create(file_name)
                && let Err(e) =
                    file.write_all(alloy_primitives::hex::encode(secret.as_bytes()).as_bytes())
            {
                return Err(JwtError::IoError(format!("Failed to write JWT secret to file: {e}")));
            }

            Ok(secret)
        },
        |content| JwtSecret::from_hex(content).map_err(|e| JwtError::ParseError(e.to_string())),
    )
}

/// Resolves a JWT secret from multiple sources with priority:
/// 1. File path (if Some)
/// 2. Encoded secret (if Some)
/// 3. Default file in current directory
///
/// # Arguments
/// * `file_path` - Optional path to a JWT file
/// * `encoded` - Optional pre-parsed JwtSecret
/// * `default_file` - Fallback file name in current directory
pub fn resolve_jwt_secret(
    file_path: Option<&Path>,
    encoded: Option<JwtSecret>,
    default_file: &str,
) -> Result<JwtSecret, JwtError> {
    if let Some(path) = file_path {
        return read_jwt_secret(path);
    }

    if let Some(secret) = encoded {
        return Ok(secret);
    }

    default_jwt_secret(default_file)
}
