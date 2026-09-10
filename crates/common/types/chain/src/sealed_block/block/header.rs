//! Block header data primitive.

use core::{fmt, hash::Hash};

use alloy_primitives::Sealable;

use crate::sealed_block::{InMemorySize, MaybeSerde};

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
    + base_common_types_chain::BlockHeader
    + Sealable
    + InMemorySize
    + MaybeSerde
    + AsRef<Self>
    + 'static
{
}

impl BlockHeader for base_common_types_chain::Header {}
