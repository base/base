use std::collections::BTreeSet;

use alloy_consensus::TxEnvelope;
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::B256;

use crate::{ActionFixture, BlockId, CURRENT_SCHEMA_VERSION, FixtureL1Block, FixtureL2Block};

/// Validates fixture consistency before action tests consume captured data.
#[derive(Debug, Clone, Copy, Default)]
pub struct FixtureValidator;

impl FixtureValidator {
    /// Validate a fixture and return the first detected consistency error.
    pub fn validate(fixture: &ActionFixture) -> Result<(), FixtureValidationError> {
        Self::validate_schema(fixture)?;
        Self::validate_l1_blocks(fixture)?;
        Self::validate_l2_blocks(fixture)?;
        Self::validate_expected(fixture)?;
        Ok(())
    }

    /// Validate the manifest schema version.
    pub const fn validate_schema(fixture: &ActionFixture) -> Result<(), FixtureValidationError> {
        if fixture.manifest.schema_version != CURRENT_SCHEMA_VERSION {
            return Err(FixtureValidationError::UnsupportedSchema {
                got: fixture.manifest.schema_version,
                expected: CURRENT_SCHEMA_VERSION,
            });
        }
        Ok(())
    }

    /// Validate captured L1 blocks.
    pub fn validate_l1_blocks(fixture: &ActionFixture) -> Result<(), FixtureValidationError> {
        Self::validate_manifest_anchor(
            "l1_start",
            fixture.manifest.l1_start,
            fixture.l1_blocks.first(),
        )?;
        Self::validate_manifest_anchor(
            "l1_end",
            fixture.manifest.l1_end,
            fixture.l1_blocks.last(),
        )?;

        for block in &fixture.l1_blocks {
            Self::validate_transaction_count(
                "l1",
                block.header.number,
                block.transactions.len(),
                block.receipts.len(),
            )?;
            Self::validate_l1_transaction_bytes(block)?;
            Self::validate_unique_blobs(block)?;
        }

        for pair in fixture.l1_blocks.windows(2) {
            let parent = &pair[0];
            let child = &pair[1];
            Self::validate_parent_link(
                "l1",
                parent.id(),
                child.header.number,
                child.header.parent_hash,
            )?;
        }

        Ok(())
    }

    /// Validate captured L2 blocks.
    pub fn validate_l2_blocks(fixture: &ActionFixture) -> Result<(), FixtureValidationError> {
        Self::validate_manifest_anchor(
            "l2_start",
            fixture.manifest.l2_start,
            fixture.l2_blocks.first(),
        )?;
        Self::validate_manifest_anchor(
            "l2_end",
            fixture.manifest.l2_end,
            fixture.l2_blocks.last(),
        )?;

        for block in &fixture.l2_blocks {
            Self::validate_transaction_count(
                "l2",
                block.header.number,
                block.transactions.len(),
                block.receipts.len(),
            )?;
            Self::validate_l2_transaction_bytes(block)?;
        }

        for pair in fixture.l2_blocks.windows(2) {
            let parent = &pair[0];
            let child = &pair[1];
            Self::validate_parent_link(
                "l2",
                parent.id(),
                child.header.number,
                child.header.parent_hash,
            )?;
        }

        Ok(())
    }

    /// Validate expected outcome references.
    pub fn validate_expected(fixture: &ActionFixture) -> Result<(), FixtureValidationError> {
        if let Some(safe_head) = fixture.expected.safe_head {
            let found = fixture.l2_blocks.iter().any(|block| {
                block.header.number == safe_head.number && block.id().hash == safe_head.hash
            });
            if !found && !fixture.l2_blocks.is_empty() {
                return Err(FixtureValidationError::ExpectedSafeHeadMissing(safe_head));
            }
        }

        Ok(())
    }

    /// Validate a manifest anchor against a block, when the anchor is present.
    pub fn validate_manifest_anchor<B>(
        label: &'static str,
        expected: Option<BlockId>,
        block: Option<&B>,
    ) -> Result<(), FixtureValidationError>
    where
        B: FixtureBlockId,
    {
        let Some(expected) = expected else {
            return Ok(());
        };
        let actual = block
            .map(FixtureBlockId::id)
            .ok_or(FixtureValidationError::MissingAnchoredBlock { label, expected })?;
        if actual != expected {
            return Err(FixtureValidationError::AnchorMismatch { label, got: actual, expected });
        }
        Ok(())
    }

    /// Validate that each transaction has a matching receipt.
    pub const fn validate_transaction_count(
        chain: &'static str,
        block_number: u64,
        transactions: usize,
        receipts: usize,
    ) -> Result<(), FixtureValidationError> {
        if transactions != receipts {
            return Err(FixtureValidationError::TransactionReceiptCountMismatch {
                chain,
                block_number,
                transactions,
                receipts,
            });
        }
        Ok(())
    }

    /// Decode all L1 transaction bytes as Ethereum transaction envelopes.
    pub fn validate_l1_transaction_bytes(
        block: &FixtureL1Block,
    ) -> Result<(), FixtureValidationError> {
        for (index, raw) in block.transactions.iter().enumerate() {
            TxEnvelope::decode_2718_exact(raw.as_ref()).map_err(|error| {
                FixtureValidationError::TransactionDecode {
                    chain: "l1",
                    block_number: block.header.number,
                    transaction_index: index,
                    error: error.to_string(),
                }
            })?;
        }
        Ok(())
    }

    /// Decode all L2 transaction bytes as Base transaction envelopes when possible.
    ///
    /// The fixture crate currently validates the bytes are non-empty. L2 decode is
    /// intentionally left to execution-specific consumers because Base deposit
    /// envelopes live in `base-common-consensus`.
    pub fn validate_l2_transaction_bytes(
        block: &FixtureL2Block,
    ) -> Result<(), FixtureValidationError> {
        for (index, raw) in block.transactions.iter().enumerate() {
            if raw.is_empty() {
                return Err(FixtureValidationError::TransactionDecode {
                    chain: "l2",
                    block_number: block.header.number,
                    transaction_index: index,
                    error: "empty transaction bytes".to_owned(),
                });
            }
        }
        Ok(())
    }

    /// Validate that a captured L1 block does not contain duplicate blob hashes.
    pub fn validate_unique_blobs(block: &FixtureL1Block) -> Result<(), FixtureValidationError> {
        let mut seen = BTreeSet::new();
        for blob in &block.blobs {
            if !seen.insert(blob.versioned_hash) {
                return Err(FixtureValidationError::DuplicateBlobHash {
                    block_number: block.header.number,
                    blob_hash: blob.versioned_hash,
                });
            }
        }
        Ok(())
    }

    /// Validate a parent-child header link.
    pub fn validate_parent_link(
        chain: &'static str,
        parent: BlockId,
        child_number: u64,
        child_parent_hash: B256,
    ) -> Result<(), FixtureValidationError> {
        if child_number != parent.number + 1 {
            return Err(FixtureValidationError::NonContiguousBlocks {
                chain,
                parent_number: parent.number,
                child_number,
            });
        }
        if child_parent_hash != parent.hash {
            return Err(FixtureValidationError::ParentHashMismatch {
                chain,
                child_number,
                got: child_parent_hash,
                expected: parent.hash,
            });
        }
        Ok(())
    }
}

/// Trait used by validation to compare manifest anchors against block-like values.
pub trait FixtureBlockId {
    /// Return the block identity.
    fn id(&self) -> BlockId;
}

impl FixtureBlockId for FixtureL1Block {
    fn id(&self) -> BlockId {
        Self::id(self)
    }
}

impl FixtureBlockId for FixtureL2Block {
    fn id(&self) -> BlockId {
        Self::id(self)
    }
}

/// Fixture validation failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FixtureValidationError {
    /// The fixture schema version is not supported by this crate.
    #[error("unsupported fixture schema version {got}; expected {expected}")]
    UnsupportedSchema {
        /// Version found in the manifest.
        got: u32,
        /// Version supported by this crate.
        expected: u32,
    },
    /// A manifest anchor references a block, but that block list is empty.
    #[error("missing block for manifest anchor {label}: expected {expected:?}")]
    MissingAnchoredBlock {
        /// Manifest anchor label.
        label: &'static str,
        /// Expected anchored block identity.
        expected: BlockId,
    },
    /// A manifest anchor does not match the captured block.
    #[error("manifest anchor {label} mismatch: got {got:?}, expected {expected:?}")]
    AnchorMismatch {
        /// Manifest anchor label.
        label: &'static str,
        /// Captured block identity.
        got: BlockId,
        /// Manifest block identity.
        expected: BlockId,
    },
    /// The captured blocks are not contiguous.
    #[error("{chain} blocks are not contiguous: parent {parent_number}, child {child_number}")]
    NonContiguousBlocks {
        /// Chain label.
        chain: &'static str,
        /// Parent block number.
        parent_number: u64,
        /// Child block number.
        child_number: u64,
    },
    /// A child block does not reference the prior block hash.
    #[error("{chain} block {child_number} parent hash mismatch: got {got}, expected {expected}")]
    ParentHashMismatch {
        /// Chain label.
        chain: &'static str,
        /// Child block number.
        child_number: u64,
        /// Captured parent hash.
        got: B256,
        /// Expected parent hash.
        expected: B256,
    },
    /// Transaction and receipt list lengths differ.
    #[error("{chain} block {block_number} has {transactions} transactions but {receipts} receipts")]
    TransactionReceiptCountMismatch {
        /// Chain label.
        chain: &'static str,
        /// Block number.
        block_number: u64,
        /// Transaction count.
        transactions: usize,
        /// Receipt count.
        receipts: usize,
    },
    /// Transaction bytes failed to decode.
    #[error("{chain} block {block_number} tx {transaction_index} failed to decode: {error}")]
    TransactionDecode {
        /// Chain label.
        chain: &'static str,
        /// Block number.
        block_number: u64,
        /// Transaction index.
        transaction_index: usize,
        /// Decode error.
        error: String,
    },
    /// Duplicate blob hash in one L1 block.
    #[error("l1 block {block_number} contains duplicate blob hash {blob_hash}")]
    DuplicateBlobHash {
        /// Block number.
        block_number: u64,
        /// Duplicate blob hash.
        blob_hash: B256,
    },
    /// The expected safe head is not present in captured L2 data.
    #[error("expected safe head {0:?} is not present in captured l2 blocks")]
    ExpectedSafeHeadMissing(BlockId),
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Header;
    use alloy_primitives::b256;

    use crate::{
        ActionFixture, ExpectedOutcome, FixtureKind, FixtureL1Block, FixtureManifest,
        FixtureValidator,
    };

    #[test]
    fn rejects_unsupported_schema() {
        let mut manifest = FixtureManifest::new("window", "base-mainnet", FixtureKind::Derivation);
        manifest.schema_version = 99;
        let fixture = ActionFixture::new(manifest, vec![], vec![], ExpectedOutcome::default());
        assert!(FixtureValidator::validate(&fixture).is_err());
    }

    #[test]
    fn rejects_non_contiguous_l1_blocks() {
        let manifest = FixtureManifest::new("window", "base-mainnet", FixtureKind::Derivation);
        let first = FixtureL1Block {
            header: Header { number: 1, ..Default::default() },
            transactions: vec![],
            receipts: vec![],
            blobs: vec![],
        };
        let second = FixtureL1Block {
            header: Header {
                number: 3,
                parent_hash: b256!(
                    "0000000000000000000000000000000000000000000000000000000000000001"
                ),
                ..Default::default()
            },
            transactions: vec![],
            receipts: vec![],
            blobs: vec![],
        };
        let fixture =
            ActionFixture::new(manifest, vec![first, second], vec![], ExpectedOutcome::default());
        assert!(FixtureValidator::validate_l1_blocks(&fixture).is_err());
    }
}
