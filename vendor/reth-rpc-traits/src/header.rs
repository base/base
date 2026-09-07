use alloy_primitives::U256;
use reth_primitives_traits::SealedHeader;

/// Conversion trait for obtaining RPC header from a consensus header.
pub trait FromConsensusHeader {
    /// Takes a consensus header and converts it into `self`.
    fn from_consensus_header(header: SealedHeader, block_size: usize) -> Self;
}

impl FromConsensusHeader for alloy_rpc_types_eth::Header {
    fn from_consensus_header(header: SealedHeader, block_size: usize) -> Self {
        Self::from_consensus(header.into(), None, Some(U256::from(block_size)))
    }
}
