//! Block header data primitive.

use core::{fmt, hash::Hash};

/// Re-exported alias
pub use alloy_consensus::BlockHeader as AlloyBlockHeader;
use alloy_primitives::Sealable;

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
    + alloy_consensus::BlockHeader
    + Sealable
    + InMemorySize
    + MaybeSerde
    + AsRef<Self>
    + 'static
{
}

impl BlockHeader for alloy_consensus::Header {}
