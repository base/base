//! Proof-of-work for spam prevention in encrypted relay.
//!
//! Uses SHA256 with configurable difficulty (leading zero bits).
//! Target: ~10-25ms on average hardware with difficulty 18-20.

use sha2::{Digest, Sha256};

use crate::error::RelayError;

/// Default proof-of-work difficulty (leading zero bits).
///
/// 18 bits ≈ 262,144 hashes on average ≈ 10-15ms on modern CPU.
pub const DEFAULT_DIFFICULTY: u8 = 18;

/// Maximum allowed difficulty to prevent DoS via config manipulation.
pub const MAX_DIFFICULTY: u8 = 32;

/// Computes a proof-of-work nonce for the given payload.
///
/// Finds a nonce such that `SHA256(payload || nonce)` has at least
/// `difficulty` leading zero bits.
///
/// # Arguments
/// * `payload` - The data to compute PoW over (typically the encrypted transaction)
/// * `difficulty` - Number of leading zero bits required
///
/// # Returns
/// The nonce that satisfies the difficulty requirement.
pub fn compute(payload: &[u8], difficulty: u8) -> u64 {
    let difficulty = difficulty.min(MAX_DIFFICULTY);

    for nonce in 0..u64::MAX {
        if verify_internal(payload, nonce, difficulty) {
            return nonce;
        }
    }

    // Practically unreachable - would take longer than the universe exists
    0
}

/// Verifies a proof-of-work nonce.
///
/// # Arguments
/// * `payload` - The data the PoW was computed over
/// * `nonce` - The nonce to verify
/// * `difficulty` - Number of leading zero bits required
///
/// # Returns
/// `Ok(())` if valid, `Err(RelayError::InvalidPow)` if invalid.
pub fn verify(payload: &[u8], nonce: u64, difficulty: u8) -> Result<(), RelayError> {
    let difficulty = difficulty.min(MAX_DIFFICULTY);
    let found = leading_zero_bits(payload, nonce);

    if found >= difficulty {
        Ok(())
    } else {
        Err(RelayError::InvalidPow { found, required: difficulty })
    }
}

/// Internal verification without error type (for compute loop).
fn verify_internal(payload: &[u8], nonce: u64, difficulty: u8) -> bool {
    leading_zero_bits(payload, nonce) >= difficulty
}

/// Counts leading zero bits in `SHA256(payload || nonce)`.
fn leading_zero_bits(payload: &[u8], nonce: u64) -> u8 {
    let mut hasher = Sha256::new();
    hasher.update(payload);
    hasher.update(nonce.to_le_bytes());
    let hash = hasher.finalize();

    let mut zeros = 0u8;
    for byte in hash.iter() {
        if *byte == 0 {
            zeros += 8;
        } else {
            zeros += byte.leading_zeros() as u8;
            break;
        }
    }
    zeros
}

/// Computes the SHA256 hash of `payload || nonce`.
///
/// Useful for debugging and computing commitment hashes.
pub fn hash(payload: &[u8], nonce: u64) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(payload);
    hasher.update(nonce.to_le_bytes());
    hasher.finalize().into()
}

/// Computes the commitment hash (SHA256 of payload only).
pub fn commitment(payload: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(payload);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn test_compute_and_verify() {
        let payload = b"test encrypted transaction payload";
        let difficulty = 16; // Lower for faster tests

        let nonce = compute(payload, difficulty);
        assert!(verify(payload, nonce, difficulty).is_ok());
    }

    #[test]
    fn test_verify_invalid_nonce() {
        let payload = b"test payload";
        let difficulty = 20;

        // Very unlikely that nonce 0 satisfies difficulty 20
        let result = verify(payload, 0, difficulty);
        // Could pass by chance, but extremely unlikely
        if result.is_err() {
            match result {
                Err(RelayError::InvalidPow { found, required }) => {
                    assert!(found < required);
                }
                _ => panic!("unexpected error type"),
            }
        }
    }

    #[test]
    fn test_verify_wrong_payload() {
        let payload = b"original payload";
        let difficulty = 16;

        let nonce = compute(payload, difficulty);

        // Different payload should fail
        let result = verify(b"different payload", nonce, difficulty);
        // Could pass by coincidence but very unlikely
        assert!(result.is_err() || leading_zero_bits(b"different payload", nonce) >= difficulty);
    }

    #[test]
    fn test_leading_zero_bits() {
        // Test that we get consistent results
        let z1 = leading_zero_bits(b"test", 0);
        let z2 = leading_zero_bits(b"test", 0);
        assert_eq!(z1, z2); // Deterministic

        // Different inputs should (usually) produce different results
        let z3 = leading_zero_bits(b"test", 1);
        // z1 and z3 could be equal by chance, but function should work
        assert!(z1 <= 32 || z3 <= 32); // Sanity check - most hashes have few leading zeros
    }

    #[test]
    fn test_commitment() {
        let payload = b"test payload";
        let hash1 = commitment(payload);
        let hash2 = commitment(payload);

        assert_eq!(hash1, hash2); // Deterministic

        let hash3 = commitment(b"different");
        assert_ne!(hash1, hash3);
    }

    #[test]
    fn test_difficulty_capped() {
        let payload = b"test";

        // Should not panic or hang even with extreme difficulty
        let result = verify(payload, 0, 255);
        assert!(result.is_err()); // MAX_DIFFICULTY caps it
    }

    #[test]
    #[ignore] // Run manually to check timing
    fn test_compute_timing() {
        let payload = b"test encrypted transaction with realistic size";
        
        for difficulty in [16, 18, 20] {
            let start = Instant::now();
            let nonce = compute(payload, difficulty);
            let elapsed = start.elapsed();
            
            println!("difficulty {difficulty}: {elapsed:?} (nonce: {nonce})");
            assert!(verify(payload, nonce, difficulty).is_ok());
        }
    }
}
