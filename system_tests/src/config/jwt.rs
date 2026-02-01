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

#[cfg(test)]
mod tests {
    use super::random_jwt_secret_hex;

    #[test]
    fn jwt_secret_hex_is_32_bytes() {
        let secret = random_jwt_secret_hex();

        assert_eq!(secret.len(), 64);
        assert!(hex::decode(&secret).is_ok());
    }
}
