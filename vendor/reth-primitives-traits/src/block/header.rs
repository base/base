//! Block header data primitive.

use core::{fmt, hash::Hash};

use alloy_primitives::Sealable;
/// Re-exported alias
pub use base_common_consensus::BlockHeader as AlloyBlockHeader;

use crate::{InMemorySize, MaybeSerde};

/// Abstraction of a block header.
pub trait BlockHeader:
    Send
    + Sync
    + Unpin
    + Clone
    + Hash
    + Default
    + fmt::Debug
    + PartialEq
    + Eq
    + alloy_rlp::Encodable
    + alloy_rlp::Decodable
    + base_common_consensus::BlockHeader
    + Sealable
    + InMemorySize
    + MaybeSerde
    + AsRef<Self>
    + 'static
{
}

impl BlockHeader for base_common_consensus::Header {}
