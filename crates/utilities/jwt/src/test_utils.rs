//! Test utilities for JWT handling.

use alloy_rpc_types_engine::JwtSecret;

/// Creates a random JWT secret for testing.
pub fn random_jwt_secret() -> JwtSecret {
    JwtSecret::random()
}
