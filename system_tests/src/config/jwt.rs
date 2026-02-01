//! JWT secret generation for Engine API authentication.

use alloy_primitives::B256;
use rand::RngCore;

/// Generates a random JWT secret as 32 bytes.
pub fn random_jwt_secret() -> B256 {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    B256::from(bytes)
}

/// Generates a random JWT secret encoded as a hex string.
pub fn random_jwt_secret_hex() -> String {
    hex::encode(random_jwt_secret())
}
