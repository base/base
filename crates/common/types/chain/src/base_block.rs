//! Block type for Base chains.

use crate::BaseTxEnvelope;

/// A block type for Base chains.
pub type BaseBlock = base_common_types_chain::Block<BaseTxEnvelope>;

/// Base block body containing signed Base transactions.
pub type BaseBlockBody = base_common_types_chain::BlockBody<BaseTxEnvelope>;

#[cfg(feature = "k256")]
impl BaseBlock {
    /// Seals the block using a caller-supplied hash without validating it.
    pub fn seal_unchecked(self, hash: alloy_primitives::B256) -> crate::SealedBlock {
        crate::SealedBlock::new_unchecked(self, hash)
    }

    /// Seals the block and computes its header hash lazily.
    pub fn seal(self) -> crate::SealedBlock {
        crate::SealedBlock::new_unhashed(self)
    }

    /// Computes the header hash and seals the block.
    pub fn seal_slow(self) -> crate::SealedBlock {
        crate::SealedBlock::seal_slow(self)
    }

    /// Returns the block header.
    pub const fn header(&self) -> &crate::Header {
        &self.header
    }

    /// Returns the block body.
    pub const fn body(&self) -> &BaseBlockBody {
        &self.body
    }

    /// Splits the block into its header and body.
    pub fn split(self) -> (crate::Header, BaseBlockBody) {
        (self.header, self.body)
    }

    /// Borrows the header and body separately.
    pub const fn split_ref(&self) -> (&crate::Header, &BaseBlockBody) {
        (&self.header, &self.body)
    }

    /// Recovers transaction signers while enforcing the low-s signature rule.
    pub fn recover_signers(
        &self,
    ) -> Result<alloc::vec::Vec<alloy_primitives::Address>, crate::RecoveryError> {
        crate::recover_signers(&self.body.transactions)
    }

    /// Attaches senders, recovering without the low-s rule if their count does not match.
    pub fn try_into_recovered_unchecked(
        self,
        senders: alloc::vec::Vec<alloy_primitives::Address>,
    ) -> Result<crate::RecoveredBlock, crate::BlockRecoveryError<Self>> {
        let senders = if self.body.transactions.len() == senders.len() {
            senders
        } else {
            let Ok(senders) = crate::recover_signers_unchecked(&self.body.transactions) else {
                return Err(crate::BlockRecoveryError::new(self));
            };
            senders
        };
        Ok(crate::RecoveredBlock::new_unhashed(self, senders))
    }

    /// Attaches caller-supplied signers without validating them.
    pub fn into_recovered_with_signers(
        self,
        signers: alloc::vec::Vec<alloy_primitives::Address>,
    ) -> crate::RecoveredBlock {
        crate::RecoveredBlock::new_unhashed(self, signers)
    }

    /// Recovers all transaction signers, returning the original block on failure.
    pub fn try_into_recovered(
        self,
    ) -> Result<crate::RecoveredBlock, crate::BlockRecoveryError<Self>> {
        let Ok(signers) = self.recover_signers() else {
            return Err(crate::BlockRecoveryError::new(self));
        };
        Ok(crate::RecoveredBlock::new_unhashed(self, signers))
    }
}
