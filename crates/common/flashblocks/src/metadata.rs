//! Contains the [`Metadata`] type used in Flashblocks.

use std::{fmt, num::ParseIntError, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

/// Identifies a flashblock within a canonical block build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FlashblockId {
    /// Canonical block number this flashblock belongs to.
    pub block_number: u64,
    /// Index of the flashblock within the block.
    pub index: u64,
}

/// Error returned when parsing a [`FlashblockId`] from its compact string form.
#[derive(Debug, Error)]
pub enum FlashblockIdParseError {
    /// The input did not use the expected `<block_number>-<index>` format.
    #[error("invalid flashblock id '{value}': expected '<block_number>-<index>' format")]
    InvalidFormat {
        /// Original input value.
        value: String,
    },
    /// The block number component was not a valid integer.
    #[error("invalid flashblock id '{value}': block number '{block_number}' must be an integer")]
    InvalidBlockNumber {
        /// Original input value.
        value: String,
        /// Block number component.
        block_number: String,
        /// Parse error for the block number component.
        source: ParseIntError,
    },
    /// The flashblock index component was not a valid integer.
    #[error("invalid flashblock id '{value}': index '{index}' must be an integer")]
    InvalidIndex {
        /// Original input value.
        value: String,
        /// Index component.
        index: String,
        /// Parse error for the index component.
        source: ParseIntError,
    },
}

impl fmt::Display for FlashblockId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.block_number, self.index)
    }
}

impl FromStr for FlashblockId {
    type Err = FlashblockIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some((block_number, index)) = value.split_once('-') else {
            return Err(FlashblockIdParseError::InvalidFormat { value: value.to_owned() });
        };

        Ok(Self {
            block_number: block_number.parse().map_err(|source| {
                FlashblockIdParseError::InvalidBlockNumber {
                    value: value.to_owned(),
                    block_number: block_number.to_owned(),
                    source,
                }
            })?,
            index: index.parse().map_err(|source| FlashblockIdParseError::InvalidIndex {
                value: value.to_owned(),
                index: index.to_owned(),
                source,
            })?,
        })
    }
}

impl From<FlashblockId> for String {
    fn from(value: FlashblockId) -> Self {
        value.to_string()
    }
}

impl Serialize for FlashblockId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for FlashblockId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?.parse().map_err(de::Error::custom)
    }
}

/// Metadata associated with a flashblock.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, Default)]
pub struct Metadata {
    /// Block number this flashblock belongs to.
    pub block_number: u64,
    /// Identifier of the previously emitted flashblock.
    #[serde(default)]
    pub prev_flashblock_id: FlashblockId,
}

impl Metadata {
    /// Creates metadata for legacy tests and callers that only need a block number.
    pub const fn new(block_number: u64) -> Self {
        Self { block_number, prev_flashblock_id: FlashblockId { block_number: 0, index: 0 } }
    }
}

#[cfg(test)]
mod tests {
    use super::{FlashblockId, Metadata};

    #[test]
    fn flashblock_id_serializes_as_compact_key() {
        let json = serde_json::to_value(FlashblockId { block_number: 123, index: 4 })
            .expect("flashblock id should serialize");

        assert_eq!(json, serde_json::json!("123-4"));
    }

    #[test]
    fn flashblock_id_deserializes_from_compact_key() {
        let id: FlashblockId = serde_json::from_value(serde_json::json!("123-4"))
            .expect("flashblock id should deserialize");

        assert_eq!(id, FlashblockId { block_number: 123, index: 4 });
    }

    #[test]
    fn metadata_deserializes_missing_flashblock_id_as_default() {
        let metadata: Metadata = serde_json::from_value(serde_json::json!({"block_number": 123}))
            .expect("legacy metadata should deserialize");

        assert_eq!(metadata.prev_flashblock_id, FlashblockId::default());
    }
}
