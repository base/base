//! JWT secret generation for Engine API authentication.

use alloy_rpc_types_engine::JwtSecret;

/// Generates a random JWT secret.
pub fn random_jwt_secret() -> JwtSecret {
    JwtSecret::random()
}

/// Generates a random JWT secret encoded as a hex string.
pub fn random_jwt_secret_hex() -> String {
    hex::encode(JwtSecret::random().as_bytes())
}
