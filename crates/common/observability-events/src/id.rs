use std::fmt;

use sha2::{Digest, Sha256};

/// Builder for deterministic event IDs.
#[derive(Debug, Clone)]
pub struct EventIdBuilder {
    hasher: Sha256,
}

impl EventIdBuilder {
    /// Creates an empty event ID builder.
    pub fn new() -> Self {
        Self { hasher: Sha256::new() }
    }

    /// Adds a stable component to the ID hash.
    pub fn part(mut self, name: &str, value: impl fmt::Display) -> Self {
        let value = value.to_string();
        self.hasher.update(name.as_bytes());
        self.hasher.update([0]);
        self.hasher.update(value.len().to_le_bytes());
        self.hasher.update(value.as_bytes());
        self.hasher.update([0xff]);
        self
    }

    /// Finalizes the event ID as a hex-encoded SHA-256 digest.
    pub fn finish(self) -> String {
        format!("0x{}", hex::encode(self.hasher.finalize()))
    }
}

impl Default for EventIdBuilder {
    fn default() -> Self {
        Self::new()
    }
}
