//! Block type for Base chains.

use crate::OpTxEnvelope;

/// A block type for Base chains.
pub type OpBlock = alloy_consensus::Block<OpTxEnvelope>;
