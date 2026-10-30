use alloy_primitives::Bytes;
use base_common_consensus::Eip8130Signed;
use base_common_flz::flz_compress_len;

use crate::BaseTransaction;

/// Trait for Base transaction environments. Allows to recover the transaction encoded bytes if
/// they're available.
pub trait BaseTxEnv {
    /// Returns the encoded bytes of the transaction.
    fn encoded_bytes(&self) -> Option<&Bytes>;

    /// Returns the `FastLZ` compressed size of the encoded bytes, computing and caching it on
    /// first use so later L1 cost calculation for this transaction reuses it. `None` when the
    /// encoded bytes are unavailable.
    fn cache_encoded_bytes_fastlz_size(&mut self) -> Option<u32> {
        self.encoded_bytes().map(|encoded| flz_compress_len(encoded))
    }

    /// Records `fastlz_size`, already computed by the caller, as the `FastLZ` compressed size of
    /// the encoded bytes, so [`Self::cache_encoded_bytes_fastlz_size`] and the L1 cost charged at
    /// execution reuse it instead of compressing the bytes again. `fastlz_size` must equal
    /// `flz_compress_len` of the encoded bytes. The default implementation ignores it.
    fn set_encoded_bytes_fastlz_size(&mut self, _fastlz_size: u32) {}

    /// Returns the signed EIP-8130 envelope for an EIP-8130 transaction, or
    /// [`None`] for every other transaction type.
    ///
    /// This lives on [`BaseTxEnv`] (rather than the richer [`BaseTxTr`], which
    /// requires a revm `Transaction` supertrait) so block-gas accounting can
    /// reach the payer authentication blob through the transaction-env bound the
    /// executor and builders already carry, without spreading a `BaseTxTr` bound
    /// through every downstream generic.
    ///
    /// [`BaseTxTr`]: crate::BaseTxTr
    fn eip8130_signed(&self) -> Option<&Eip8130Signed>;
}

impl<T: revm::context::Transaction> BaseTxEnv for BaseTransaction<T> {
    fn encoded_bytes(&self) -> Option<&Bytes> {
        self.enveloped_tx.as_ref()
    }

    fn cache_encoded_bytes_fastlz_size(&mut self) -> Option<u32> {
        let encoded = self.enveloped_tx.as_ref()?;
        Some(*self.enveloped_tx_fastlz_size.get_or_insert_with(|| flz_compress_len(encoded)))
    }

    fn set_encoded_bytes_fastlz_size(&mut self, fastlz_size: u32) {
        debug_assert!(
            self.enveloped_tx
                .as_ref()
                .is_some_and(|encoded| flz_compress_len(encoded) == fastlz_size),
            "stale FastLZ size"
        );
        self.enveloped_tx_fastlz_size = Some(fastlz_size);
    }

    fn eip8130_signed(&self) -> Option<&Eip8130Signed> {
        self.eip8130.as_ref().map(|parts| &parts.signed)
    }
}
