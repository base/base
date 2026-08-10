//! Prepared L1 origin state shared by selection and attributes building.

use std::sync::Arc;

use alloy_consensus::{Header, Receipt};
use alloy_primitives::B256;
use base_protocol::BlockInfo;

/// An L1 origin with the header and receipts needed to build payload attributes.
#[derive(Debug, Clone)]
pub struct PreparedL1Origin {
    /// The verified origin block hash.
    pub hash: B256,
    /// The full L1 block header.
    pub header: Header,
    /// The L1 block receipts.
    pub receipts: Arc<Vec<Receipt>>,
}

impl PreparedL1Origin {
    /// Returns the lightweight block information for this origin.
    pub const fn block_info(&self) -> BlockInfo {
        BlockInfo {
            hash: self.hash,
            number: self.header.number,
            parent_hash: self.header.parent_hash,
            timestamp: self.header.timestamp,
        }
    }
}
