//! Block type for Base chains.

use crate::BaseTxEnvelope;

/// A block type for Base chains.
pub type BaseBlock = alloy_consensus::Block<BaseTxEnvelope>;

/// Base block body containing signed Base transactions.
pub type BaseBlockBody = alloy_consensus::BlockBody<BaseTxEnvelope>;
