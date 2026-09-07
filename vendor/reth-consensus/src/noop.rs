//! A consensus implementation that does nothing.
//!
//! This module provides `NoopConsensus`, a consensus implementation that performs no validation
//! and always returns `Ok(())` for all validation methods. Useful for testing and scenarios
//! where consensus validation is not required.
//!
//! # Examples
//!
//! ```rust
//! use reth_consensus::noop::NoopConsensus;
//! use std::sync::Arc;
//!
//! let consensus = NoopConsensus::default();
//! let consensus_arc = NoopConsensus::arc();
//! ```
//!
//! # Warning
//!
//! **Not for production use** - provides no security guarantees or consensus validation.

use alloc::sync::Arc;

use alloy_primitives::B256;
use base_common_consensus::BaseReceipt;
use reth_execution_types::BlockExecutionResult;
use reth_primitives_traits::{RecoveredBlock, SealedBlock, SealedHeader};

use crate::{Consensus, ConsensusError, FullConsensus, HeaderValidator, ReceiptRootBloom};

/// A Consensus implementation that does nothing.
///
/// Always returns `Ok(())` for all validation methods. Suitable for testing and scenarios
/// where consensus validation is not required.
#[derive(Debug, Copy, Clone, Default)]
#[non_exhaustive]
pub struct NoopConsensus;

impl NoopConsensus {
    /// Creates an Arc instance of Self.
    pub fn arc() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

impl HeaderValidator for NoopConsensus {
    /// Validates a header (no-op implementation).
    fn validate_header(&self, _header: &SealedHeader) -> Result<(), ConsensusError> {
        Ok(())
    }

    /// Validates a header against its parent (no-op implementation).
    fn validate_header_against_parent(
        &self,
        _header: &SealedHeader,
        _parent: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        Ok(())
    }
}

impl Consensus for NoopConsensus {
    /// Validates body against header (no-op implementation).
    fn validate_body_against_header(
        &self,
        _body: &base_common_consensus::BaseBlockBody,
        _header: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        Ok(())
    }

    /// Validates block before execution (no-op implementation).
    fn validate_block_pre_execution(&self, _block: &SealedBlock) -> Result<(), ConsensusError> {
        Ok(())
    }
}

impl FullConsensus for NoopConsensus {
    /// Validates block after execution (no-op implementation).
    fn validate_block_post_execution(
        &self,
        _block: &RecoveredBlock,
        _result: &BlockExecutionResult<BaseReceipt>,
        _receipt_root_bloom: Option<ReceiptRootBloom>,
        _block_access_list_hash: Option<B256>,
    ) -> Result<(), ConsensusError> {
        Ok(())
    }
}
