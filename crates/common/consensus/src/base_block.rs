//! Block type for Base chains.

use crate::BaseTxEnvelope;

/// A block type for Base chains.
pub type BaseBlock = base_common_consensus::Block<BaseTxEnvelope>;

/// Base block body containing signed Base transactions.
pub type BaseBlockBody = base_common_consensus::BlockBody<BaseTxEnvelope>;
