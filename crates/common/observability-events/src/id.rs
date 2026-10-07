use std::fmt::{self, Write as _};

use sha2::{Digest, Sha256};

/// Builder for deterministic event IDs.
#[derive(Debug, Clone)]
pub struct EventIdBuilder {
    hasher: Sha256,
    /// Reused buffer for formatting part values, so adding a part does not allocate.
    value: String,
}

impl EventIdBuilder {
    /// Creates an empty event ID builder.
    pub fn new() -> Self {
        Self { hasher: Sha256::new(), value: String::with_capacity(128) }
    }

    /// Adds a stable component to the ID hash.
    pub fn part(mut self, name: &str, value: impl fmt::Display) -> Self {
        self.value.clear();
        write!(self.value, "{value}").expect("formatting into a String does not fail");
        self.hasher.update(name.as_bytes());
        self.hasher.update([0]);
        self.hasher.update(self.value.len().to_le_bytes());
        self.hasher.update(self.value.as_bytes());
        self.hasher.update([0xff]);
        self
    }

    /// Finalizes the event ID as a `0x`-prefixed, lowercase hex SHA-256 digest.
    pub fn finish(self) -> String {
        let mut hex = [0u8; 64];
        hex::encode_to_slice(self.hasher.finalize(), &mut hex)
            .expect("a 32-byte digest encodes to 64 hex characters");
        let mut id = String::with_capacity(2 + hex.len());
        id.push_str("0x");
        id.push_str(std::str::from_utf8(&hex).expect("hex output is ASCII"));
        id
    }
}

impl Default for EventIdBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::TxHash;

    use super::*;

    /// Event IDs are downstream dedupe keys, so their bytes must never change. The expected
    /// value is SHA-256 over `name 0x00 len_le_u64 value 0xff` for each part, computed
    /// independently of this implementation.
    #[test]
    fn event_id_matches_golden_value() {
        let id = EventIdBuilder::new()
            .part("producer", "base-builder")
            .part("tx_hash", TxHash::repeat_byte(0x11))
            .part("block_number", 36_000_000_u64)
            .part("flashblock_index", "")
            .finish();

        assert_eq!(id, "0x8545eaf77e1987c0fc29d96d788763096d361c252cc05d08de0daf323da2fdf0");
    }
}
