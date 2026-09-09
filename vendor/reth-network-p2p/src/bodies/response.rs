use alloy_primitives::{BlockNumber, U256};
use base_common_types_chain::BlockHeader;
use reth_primitives_traits::{InMemorySize, SealedBlock, SealedHeader};
/// The block response
#[derive(PartialEq, Eq, Debug, Clone)]
pub enum BlockResponse {
    /// Full block response (with transactions or ommers)
    Full(SealedBlock),
    /// The empty block response
    Empty(SealedHeader),
}

impl BlockResponse {
    /// Return the block number
    pub fn block_number(&self) -> BlockNumber {
        match self {
            Self::Full(block) => block.number(),
            Self::Empty(header) => header.number(),
        }
    }

    /// Return the difficulty of the response header
    pub fn difficulty(&self) -> U256 {
        match self {
            Self::Full(block) => block.difficulty(),
            Self::Empty(header) => header.difficulty(),
        }
    }

    /// Return the reference to the response body
    pub fn into_body(self) -> Option<base_common_types_chain::BaseBlockBody> {
        match self {
            Self::Full(block) => Some(block.into_body()),
            Self::Empty(_) => None,
        }
    }

    /// Return the reference to the response body
    pub const fn body(&self) -> Option<&base_common_types_chain::BaseBlockBody> {
        match self {
            Self::Full(block) => Some(block.body()),
            Self::Empty(_) => None,
        }
    }
}

impl InMemorySize for BlockResponse {
    #[inline]
    fn size(&self) -> usize {
        match self {
            Self::Full(block) => SealedBlock::size(block),
            Self::Empty(header) => SealedHeader::size(header),
        }
    }
}
