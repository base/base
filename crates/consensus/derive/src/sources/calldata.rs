//! `CallData` Source

use alloc::{boxed::Box, collections::VecDeque};

use alloy_consensus::{Transaction, TxEip4844Variant, TxEnvelope, transaction::SignerRecoverable};
use alloy_primitives::{Address, Bytes};
use async_trait::async_trait;
use base_protocol::BlockInfo;

use crate::{ChainProvider, DataAvailabilityProvider, PipelineError, PipelineResult};

/// A data iterator that reads from calldata.
#[derive(Debug, Clone)]
pub struct CalldataSource<CP>
where
    CP: ChainProvider + Send,
{
    /// The chain provider to use for the calldata source.
    pub chain_provider: CP,
    /// The batch inbox address.
    pub batch_inbox_address: Address,
    /// Current calldata.
    pub calldata: VecDeque<Bytes>,
    /// Whether the calldata source is open.
    pub open: bool,
}

impl<CP: ChainProvider + Send> CalldataSource<CP> {
    /// Creates a new calldata source.
    pub const fn new(chain_provider: CP, batch_inbox_address: Address) -> Self {
        Self { chain_provider, batch_inbox_address, calldata: VecDeque::new(), open: false }
    }

    /// Loads the calldata into the source if it is not open.
    async fn load_calldata(
        &mut self,
        block_ref: &BlockInfo,
        batcher_address: Address,
    ) -> Result<(), CP::Error> {
        if self.open {
            return Ok(());
        }

        let (_, txs) =
            self.chain_provider.block_info_and_transactions_by_hash(block_ref.hash).await?;

        self.calldata = txs
            .iter()
            .filter_map(|tx| {
                let (tx_kind, data) = match tx {
                    TxEnvelope::Legacy(tx) => (tx.tx().to(), tx.tx().input()),
                    TxEnvelope::Eip2930(tx) => (tx.tx().to(), tx.tx().input()),
                    TxEnvelope::Eip1559(tx) => (tx.tx().to(), tx.tx().input()),
                    // EIP-4844 (type 3) transactions are valid batcher transaction types per the
                    // derivation spec. In the pre-Ecotone window an honest batcher may submit an
                    // EIP-4844 tx carrying calldata to the batch inbox; blobs within the same
                    // transaction are ignored here and handled by BlobSource. Include calldata
                    // from type-3 transactions.
                    TxEnvelope::Eip4844(tx) => match tx.tx() {
                        TxEip4844Variant::TxEip4844(inner) => (Some(inner.to), &inner.input),
                        TxEip4844Variant::TxEip4844WithSidecar(inner) => {
                            let inner = inner.tx();
                            (Some(inner.to), &inner.input)
                        }
                    },
                    _ => return None,
                };
                let to = tx_kind?;

                if to != self.batch_inbox_address {
                    return None;
                }
                if tx.recover_signer().ok()? != batcher_address {
                    return None;
                }
                Some(data.to_vec().into())
            })
            .collect::<VecDeque<_>>();

        #[cfg(feature = "metrics")]
        metrics::gauge!(
            crate::metrics::Metrics::PIPELINE_DATA_AVAILABILITY_PROVIDER,
            "source" => "calldata",
        )
        .increment(self.calldata.len() as f64);

        self.open = true;

        Ok(())
    }
}

#[async_trait]
impl<CP: ChainProvider + Send> DataAvailabilityProvider for CalldataSource<CP> {
    type Item = Bytes;

    async fn next(
        &mut self,
        block_ref: &BlockInfo,
        batcher_address: Address,
    ) -> PipelineResult<Self::Item> {
        self.load_calldata(block_ref, batcher_address).await.map_err(Into::into)?;
        self.calldata.pop_front().ok_or(PipelineError::Eof.temp())
    }

    fn clear(&mut self) {
        self.calldata.clear();
        self.open = false;
    }
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use alloy_consensus::{Signed, TxEip2930, TxEip4844, TxEip4844Variant, TxEip7702, TxLegacy};
    use alloy_primitives::{Address, Signature, TxKind, address};

    use super::*;
    use crate::{errors::PipelineErrorKind, test_utils::TestChainProvider};

    pub(crate) fn test_legacy_tx(to: Address) -> TxEnvelope {
        let sig = Signature::test_signature();
        TxEnvelope::Legacy(Signed::new_unchecked(
            TxLegacy { to: TxKind::Call(to), ..Default::default() },
            sig,
            Default::default(),
        ))
    }

    pub(crate) fn test_eip2930_tx(to: Address) -> TxEnvelope {
        let sig = Signature::test_signature();
        TxEnvelope::Eip2930(Signed::new_unchecked(
            TxEip2930 { to: TxKind::Call(to), ..Default::default() },
            sig,
            Default::default(),
        ))
    }

    pub(crate) fn test_eip7702_tx(to: Address) -> TxEnvelope {
        let sig = Signature::test_signature();
        TxEnvelope::Eip7702(Signed::new_unchecked(
            TxEip7702 { to, ..Default::default() },
            sig,
            Default::default(),
        ))
    }

    pub(crate) fn test_blob_tx(to: Address) -> TxEnvelope {
        let sig = Signature::test_signature();
        TxEnvelope::Eip4844(Signed::new_unchecked(
            TxEip4844Variant::TxEip4844(TxEip4844 { to, ..Default::default() }),
            sig,
            Default::default(),
        ))
    }

    pub(crate) fn default_test_calldata_source() -> CalldataSource<TestChainProvider> {
        CalldataSource::new(TestChainProvider::default(), Default::default())
    }

    #[tokio::test]
    async fn test_clear_calldata() {
        let mut source = default_test_calldata_source();
        source.open = true;
        source.calldata.push_back(Bytes::default());
        source.clear();
        assert!(source.calldata.is_empty());
        assert!(!source.open);
    }

    #[tokio::test]
    async fn test_load_calldata_open() {
        let mut source = default_test_calldata_source();
        source.open = true;
        assert!(source.load_calldata(&BlockInfo::default(), Address::ZERO).await.is_ok());
    }

    #[tokio::test]
    async fn test_load_calldata_provider_err() {
        let mut source = default_test_calldata_source();
        assert!(source.load_calldata(&BlockInfo::default(), Address::ZERO).await.is_err());
    }

    #[tokio::test]
    async fn test_load_calldata_chain_provider_empty_txs() {
        let mut source = default_test_calldata_source();
        let block_info = BlockInfo::default();
        source.chain_provider.insert_block_with_transactions(0, block_info, Vec::new());
        assert!(!source.open); // Source is not open by default.
        assert!(source.load_calldata(&BlockInfo::default(), Address::ZERO).await.is_ok());
        assert!(source.calldata.is_empty());
        assert!(source.open);
    }

    #[tokio::test]
    async fn test_load_calldata_wrong_batch_inbox_address() {
        let batch_inbox_address = address!("0123456789012345678901234567890123456789");
        let mut source = default_test_calldata_source();
        let block_info = BlockInfo::default();
        let tx = test_legacy_tx(batch_inbox_address);
        source.chain_provider.insert_block_with_transactions(0, block_info, vec![tx]);
        assert!(!source.open); // Source is not open by default.
        assert!(source.load_calldata(&BlockInfo::default(), Address::ZERO).await.is_ok());
        assert!(source.calldata.is_empty());
        assert!(source.open);
    }

    #[tokio::test]
    async fn test_load_calldata_wrong_signer() {
        let batch_inbox_address = address!("0123456789012345678901234567890123456789");
        let mut source = default_test_calldata_source();
        source.batch_inbox_address = batch_inbox_address;
        let block_info = BlockInfo::default();
        let tx = test_legacy_tx(batch_inbox_address);
        source.chain_provider.insert_block_with_transactions(0, block_info, vec![tx]);
        assert!(!source.open); // Source is not open by default.
        assert!(source.load_calldata(&BlockInfo::default(), Address::ZERO).await.is_ok());
        assert!(source.calldata.is_empty());
        assert!(source.open);
    }

    #[tokio::test]
    async fn test_load_calldata_valid_legacy_tx() {
        let batch_inbox_address = address!("0123456789012345678901234567890123456789");
        let mut source = default_test_calldata_source();
        source.batch_inbox_address = batch_inbox_address;
        let tx = test_legacy_tx(batch_inbox_address);
        let block_info = BlockInfo::default();
        source.chain_provider.insert_block_with_transactions(0, block_info, vec![tx.clone()]);
        assert!(!source.open); // Source is not open by default.
        assert!(
            source.load_calldata(&BlockInfo::default(), tx.recover_signer().unwrap()).await.is_ok()
        );
        assert!(!source.calldata.is_empty()); // Calldata is NOT empty.
        assert!(source.open);
    }

    #[tokio::test]
    async fn test_load_calldata_valid_eip2930_tx() {
        let batch_inbox_address = address!("0123456789012345678901234567890123456789");
        let mut source = default_test_calldata_source();
        source.batch_inbox_address = batch_inbox_address;
        let tx = test_eip2930_tx(batch_inbox_address);
        let block_info = BlockInfo::default();
        source.chain_provider.insert_block_with_transactions(0, block_info, vec![tx.clone()]);
        assert!(!source.open); // Source is not open by default.
        assert!(
            source.load_calldata(&BlockInfo::default(), tx.recover_signer().unwrap()).await.is_ok()
        );
        assert!(!source.calldata.is_empty()); // Calldata is NOT empty.
        assert!(source.open);
    }

    /// Regression test: EIP-4844 (type 3) batcher transactions must NOT be silently
    /// dropped. Per the derivation spec (derivation.md:504) type 3 is a valid batcher tx type
    /// with no pre-Ecotone exclusion.
    #[tokio::test]
    async fn test_load_calldata_blob_tx_calldata_included() {
        let batch_inbox_address = address!("0123456789012345678901234567890123456789");
        let mut source = default_test_calldata_source();
        source.batch_inbox_address = batch_inbox_address;
        let tx = test_blob_tx(batch_inbox_address);
        let block_info = BlockInfo::default();
        source.chain_provider.insert_block_with_transactions(0, block_info, vec![tx.clone()]);
        assert!(!source.open); // Source is not open by default.
        assert!(
            source.load_calldata(&BlockInfo::default(), tx.recover_signer().unwrap()).await.is_ok()
        );
        assert!(!source.calldata.is_empty());
        assert!(source.open);
    }

    #[tokio::test]
    async fn test_load_calldata_eip7702_tx_ignored() {
        let batch_inbox_address = address!("0123456789012345678901234567890123456789");
        let mut source = default_test_calldata_source();
        source.batch_inbox_address = batch_inbox_address;
        let tx = test_eip7702_tx(batch_inbox_address);
        let block_info = BlockInfo::default();
        source.chain_provider.insert_block_with_transactions(0, block_info, vec![tx.clone()]);
        assert!(!source.open); // Source is not open by default.
        assert!(
            source.load_calldata(&BlockInfo::default(), tx.recover_signer().unwrap()).await.is_ok()
        );
        assert!(source.calldata.is_empty());
        assert!(source.open);
    }

    #[tokio::test]
    async fn test_next_err_loading_calldata() {
        let mut source = default_test_calldata_source();
        assert!(matches!(
            source.next(&BlockInfo::default(), Address::ZERO).await,
            Err(PipelineErrorKind::Temporary(_))
        ));
    }

    /// After a `SystemConfig` batcher address update (modeled as changing the
    /// `batcher_address` passed to `load_calldata`), transactions signed by the
    /// OLD batcher are rejected while transactions signed by the NEW batcher
    /// are accepted.
    #[tokio::test]
    async fn test_calldata_source_rejects_old_batcher_after_config_update() {
        let batch_inbox_address = address!("0123456789012345678901234567890123456789");
        let tx = test_legacy_tx(batch_inbox_address);
        let original_batcher = tx.recover_signer().unwrap();

        let mut source = default_test_calldata_source();
        source.batch_inbox_address = batch_inbox_address;
        let block_info = BlockInfo::default();
        source
            .chain_provider
            .insert_block_with_transactions(0, block_info, vec![tx.clone()]);

        // With the original batcher address, calldata is accepted.
        assert!(source.load_calldata(&block_info, original_batcher).await.is_ok());
        assert!(!source.calldata.is_empty());

        // Simulate batcher rotation: clear source state and use a new batcher address.
        source.clear();
        let rotated_batcher = address!("00000000000000000000000000000000DeaDBeef");
        assert!(source.load_calldata(&block_info, rotated_batcher).await.is_ok());
        // The same transaction is now rejected because the signer does not match.
        assert!(source.calldata.is_empty());
    }
}
