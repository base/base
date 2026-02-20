//! Block type for OP chains.

use crate::OpTxEnvelope;

/// A block type for OP chains.
pub type OpBlock = alloy_consensus::Block<OpTxEnvelope>;
