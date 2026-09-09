//! Block abstraction.
//!
//! This module provides the core block types and transformations:
//!
//! ```rust
//! # use reth_primitives_traits::{Block, SealedBlock, RecoveredBlock};
//! # fn example(block: base_common_types_chain::BaseBlock) -> Result<(), Box<dyn std::error::Error>> {
//! // Basic block flow
//!
//! // Seal (compute hash)
//! let sealed: SealedBlock = block.seal();
//!
//! // Recover senders
//! let recovered: RecoveredBlock = sealed.try_recover()?;
//!
//! // Access components
//! let senders = recovered.senders();
//! let hash = recovered.hash();
//! # Ok(())
//! # }
//! ```

pub(crate) mod sealed;
use base_common_types_chain::{BaseBlock, BaseBlockBody, BaseTxEnvelope, Header};
pub use sealed::{SealedBlock, SealedBlockWith};

pub(crate) mod sealed_or_recovered;
pub use sealed_or_recovered::SealedOrRecoveredBlock;

pub(crate) mod recovered;
pub use recovered::RecoveredBlock;

pub mod body;
pub mod error;
pub mod header;

use alloc::{fmt, vec::Vec};

use alloy_primitives::{Address, B256};
use alloy_rlp::{Decodable, Encodable};

use crate::{
    BlockBody, InMemorySize, MaybeSerde, SealedHeader, block::error::BlockRecoveryError,
    transaction::signed::RecoveryError,
};

/// Helper trait to access [`BlockBody::Transaction`] given a [`Block`].
pub type BlockTx = BaseTxEnvelope;

/// Abstraction of block data type.
///
/// This type defines the structure of a block in the blockchain.
/// A [`Block`] is composed of a header and a body.
/// It is expected that a block can always be completely reconstructed from its header and body
pub trait Block:
    Send
    + Sync
    + Unpin
    + Clone
    + Default
    + fmt::Debug
    + PartialEq
    + Eq
    + InMemorySize
    + MaybeSerde
    + Encodable
    + Decodable
    + Into<BaseBlock>
{
    /// Create new block instance.
    fn new(header: Header, body: BaseBlockBody) -> Self;

    /// Create new a sealed block instance from a sealed header and the block body.
    #[inline]
    fn new_sealed(header: SealedHeader, body: BaseBlockBody) -> SealedBlock {
        SealedBlock::from_sealed_parts(header, body)
    }

    /// Seal the block with a known hash.
    ///
    /// WARNING: This method does not perform validation whether the hash is correct.
    #[inline]
    fn seal_unchecked(self, hash: B256) -> SealedBlock {
        SealedBlock::new_unchecked(self.into(), hash)
    }

    /// Creates the [`SealedBlock`] from the block's parts without calculating the hash upfront.
    #[inline]
    fn seal(self) -> SealedBlock {
        SealedBlock::new_unhashed(self.into())
    }

    /// Calculate the header hash and seal the block so that it can't be changed.
    fn seal_slow(self) -> SealedBlock {
        SealedBlock::seal_slow(self.into())
    }

    /// Decodes the block from RLP and seals it.
    ///
    /// Implementations can override this to compute the block hash while decoding.
    fn decode_sealed(buf: &mut &[u8]) -> alloy_rlp::Result<SealedBlock>
    where
        Self: Sized,
    {
        let block = Self::decode(buf)?;
        Ok(SealedBlock::seal_slow(block.into()))
    }

    /// Returns reference to block header.
    fn header(&self) -> &Header;

    /// Returns reference to block body.
    fn body(&self) -> &BaseBlockBody;

    /// Splits the block into its header and body.
    fn split(self) -> (Header, BaseBlockBody);

    /// Returns a tuple of references to the block's header and body.
    #[inline]
    fn split_ref(&self) -> (&Header, &BaseBlockBody) {
        (self.header(), self.body())
    }

    /// Consumes the block and returns the header.
    #[inline]
    fn into_header(self) -> Header {
        self.split().0
    }

    /// Consumes the block and returns the body.
    #[inline]
    fn into_body(self) -> BaseBlockBody {
        self.split().1
    }

    /// Encodes the block with the given header and body.
    fn rlp_encode(header: &Header, body: &BaseBlockBody, out: &mut dyn alloy_rlp::bytes::BufMut) {
        // TODO: https://github.com/paradigmxyz/reth/issues/18002
        Self::new(header.clone(), body.clone()).encode(out)
    }

    /// Returns the rlp length of the block with the given header and body.
    fn rlp_length(header: &Header, body: &BaseBlockBody) -> usize;

    /// Expensive operation that recovers transaction signer.
    fn recover_signers(&self) -> Result<Vec<Address>, RecoveryError> {
        self.body().recover_signers()
    }

    /// Transform the block into a [`RecoveredBlock`] using the given senders.
    ///
    /// If the number of senders does not match the number of transactions in the block, this falls
    /// back to manually recovery, but _without ensuring that the signature has a low `s` value_.
    ///
    /// Returns the block as error if a signature is invalid.
    fn try_into_recovered_unchecked(
        self,
        senders: Vec<Address>,
    ) -> Result<RecoveredBlock, BlockRecoveryError<Self>> {
        let senders = if self.body().transactions.len() == senders.len() {
            senders
        } else {
            // Fall back to recovery if lengths don't match
            let Ok(senders) = self.body().recover_signers_unchecked() else {
                return Err(BlockRecoveryError::new(self));
            };
            senders
        };
        Ok(RecoveredBlock::new_unhashed(self.into(), senders))
    }

    /// Transform the block into a [`RecoveredBlock`] using the given signers.
    ///
    /// Note: This method assumes the signers are correct and does not validate them.
    #[inline]
    fn into_recovered_with_signers(self, signers: Vec<Address>) -> RecoveredBlock {
        RecoveredBlock::new_unhashed(self.into(), signers)
    }

    /// **Expensive**. Transform into a [`RecoveredBlock`] by recovering senders in the contained
    /// transactions.
    ///
    /// Returns the block as error if a signature is invalid.
    fn try_into_recovered(self) -> Result<RecoveredBlock, BlockRecoveryError<Self>> {
        let Ok(signers) = self.body().recover_signers() else {
            return Err(BlockRecoveryError::new(self));
        };
        Ok(RecoveredBlock::new_unhashed(self.into(), signers))
    }

    /// A Convenience function to convert this type into the regular ethereum block that
    /// consists of:
    ///
    /// - Header
    ///
    /// And the ethereum block body [`base_common_types_chain::BlockBody`], see also
    /// [`BlockBody::into_ethereum_body`].
    /// - Transactions
    /// - Withdrawals
    /// - Ommers
    ///
    /// Note: This conversion can be incomplete. It is not expected that this `Block` is the same as
    /// [`base_common_types_chain::Block`] only that it can be converted into it which is useful for
    /// the `eth_` RPC namespace (e.g. RPC block).
    #[inline]
    fn into_ethereum_block(self) -> base_common_types_chain::Block<BaseTxEnvelope, Header> {
        let (header, body) = self.split();
        base_common_types_chain::Block::new(header, body.into_ethereum_body())
    }
}

impl Block for BaseBlock {
    #[inline]
    fn new(header: Header, body: BaseBlockBody) -> Self {
        Self { header, body }
    }

    #[inline]
    fn header(&self) -> &Header {
        &self.header
    }

    #[inline]
    fn body(&self) -> &BaseBlockBody {
        &self.body
    }

    #[inline]
    fn split(self) -> (Header, BaseBlockBody) {
        (self.header, self.body)
    }

    fn rlp_length(header: &Header, body: &BaseBlockBody) -> usize {
        Self::rlp_length_for(header, body)
    }

    fn rlp_encode(header: &Header, body: &BaseBlockBody, out: &mut dyn alloy_rlp::bytes::BufMut) {
        Self::rlp_encode_from_parts(header, body, out)
    }

    fn decode_sealed(buf: &mut &[u8]) -> alloy_rlp::Result<SealedBlock> {
        Self::decode_sealed(buf).map(Into::into)
    }

    #[inline]
    fn into_ethereum_block(self) -> Self {
        self
    }
}

/// An extension trait for [`Block`]s that allows for mutable access to the block's internals.
///
/// This allows for modifying the block's header and body for testing purposes.
#[cfg(any(test, feature = "test-utils"))]
pub trait TestBlock: Block {
    /// Returns mutable reference to block body.
    fn body_mut(&mut self) -> &mut BaseBlockBody;

    /// Returns mutable reference to block header.
    fn header_mut(&mut self) -> &mut Header;

    /// Updates the block header.
    fn set_header(&mut self, header: Header);

    /// Updates the parent block hash.
    #[inline]
    fn set_parent_hash(&mut self, hash: alloy_primitives::BlockHash) {
        self.header_mut().parent_hash = hash;
    }

    /// Updates the block number.
    #[inline]
    fn set_block_number(&mut self, number: alloy_primitives::BlockNumber) {
        self.header_mut().number = number;
    }

    /// Updates the block timestamp.
    #[inline]
    fn set_timestamp(&mut self, timestamp: u64) {
        self.header_mut().timestamp = timestamp;
    }

    /// Updates the block state root.
    #[inline]
    fn set_state_root(&mut self, state_root: alloy_primitives::B256) {
        self.header_mut().state_root = state_root;
    }

    /// Updates the block difficulty.
    #[inline]
    fn set_difficulty(&mut self, difficulty: alloy_primitives::U256) {
        self.header_mut().difficulty = difficulty;
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl TestBlock for BaseBlock {
    #[inline]
    fn body_mut(&mut self) -> &mut BaseBlockBody {
        &mut self.body
    }

    #[inline]
    fn header_mut(&mut self) -> &mut Header {
        &mut self.header
    }

    #[inline]
    fn set_header(&mut self, header: Header) {
        self.header = header
    }
}
