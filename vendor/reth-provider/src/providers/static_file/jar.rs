use std::{
    fmt::Debug,
    ops::{Deref, RangeBounds, RangeInclusive},
    sync::Arc,
};

use alloy_eips::BlockHashOrNumber;
use alloy_primitives::{Address, B256, BlockHash, BlockNumber, TxHash, TxNumber};
use base_common_types_chain::{
    BaseReceipt, BaseTxEnvelope, ChainInfo, transaction::TransactionMeta,
};
use base_execution_state_api::range_size_hint;
use base_execution_state_types::ChangesetOffset;
use base_execution_state_types::{ProviderError, ProviderResult};
use reth_db::static_file::{
    BlockHashMask, HeaderMask, HeaderWithHashMask, ReceiptMask, StaticFileCursor, TransactionMask,
    TransactionSenderMask,
};
use reth_primitives_traits::SealedHeader;

use super::{
    LoadedJarRef,
    metrics::{StaticFileProviderMetrics, StaticFileProviderOperation},
};
use crate::{
    BlockHashReader, BlockNumReader, HeaderProvider, ReceiptProvider, TransactionsProvider,
    to_range,
};
/// Provider over a specific `NippyJar` and range.
#[derive(Debug)]
pub struct StaticFileJarProvider<'a> {
    /// Main static file segment
    jar: LoadedJarRef<'a>,
    /// Another kind of static file segment to help query data from the main one.
    auxiliary_jar: Option<Box<Self>>,
    /// Metrics for the static files.
    metrics: Option<Arc<StaticFileProviderMetrics>>,
}

impl<'a> Deref for StaticFileJarProvider<'a> {
    type Target = LoadedJarRef<'a>;
    fn deref(&self) -> &Self::Target {
        &self.jar
    }
}

impl<'a> From<LoadedJarRef<'a>> for StaticFileJarProvider<'a> {
    fn from(value: LoadedJarRef<'a>) -> Self {
        StaticFileJarProvider { jar: value, auxiliary_jar: None, metrics: None }
    }
}

impl<'a> StaticFileJarProvider<'a> {
    /// Provides a cursor for more granular data access.
    pub fn cursor<'b>(&'b self) -> ProviderResult<StaticFileCursor<'a>>
    where
        'b: 'a,
    {
        let result = StaticFileCursor::new(self.value(), self.mmap_handle())?;

        if let Some(metrics) = &self.metrics {
            metrics.record_segment_operation(
                self.segment(),
                StaticFileProviderOperation::InitCursor,
                None,
            );
        }

        Ok(result)
    }

    /// Adds a new auxiliary static file to help query data from the main one
    pub fn with_auxiliary(mut self, auxiliary_jar: Self) -> Self {
        self.auxiliary_jar = Some(Box::new(auxiliary_jar));
        self
    }

    /// Enables metrics on the provider.
    pub fn with_metrics(mut self, metrics: Arc<StaticFileProviderMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Returns the total size of the data and offsets files (from the in-memory mmap).
    pub fn size(&self) -> usize {
        self.jar.value().size()
    }

    /// Reads a changeset offset from the sidecar file for a given block.
    ///
    /// Returns `None` if:
    /// - The segment is not change-based
    /// - The block is not in the block range
    /// - The sidecar file doesn't exist
    pub fn read_changeset_offset(
        &self,
        block_number: BlockNumber,
    ) -> ProviderResult<Option<ChangesetOffset>> {
        let header = self.user_header();
        if !header.segment().is_change_based() {
            return Ok(None);
        }

        let Some(index) = header.changeset_offset_index(block_number) else {
            return Ok(None);
        };

        if let Some(reader) = self.jar.value().csoff_reader() {
            reader.get(index).map_err(ProviderError::other)
        } else {
            Ok(None)
        }
    }

    /// Reads all changeset offsets from the sidecar file.
    ///
    /// Returns `None` if:
    /// - The segment is not change-based
    /// - The sidecar file doesn't exist
    pub fn read_changeset_offsets(&self) -> ProviderResult<Option<Vec<ChangesetOffset>>> {
        let header = self.user_header();
        if !header.segment().is_change_based() {
            return Ok(None);
        }

        let len = header.changeset_offsets_len();
        if len == 0 {
            return Ok(Some(Vec::new()));
        }

        if let Some(reader) = self.jar.value().csoff_reader() {
            let offsets = reader.get_range(0, len).map_err(ProviderError::other)?;
            Ok(Some(offsets))
        } else {
            Ok(None)
        }
    }
}

impl HeaderProvider for StaticFileJarProvider<'_> {
    fn header(
        &self,
        block_hash: BlockHash,
    ) -> ProviderResult<Option<base_common_types_chain::Header>> {
        Ok(self
            .cursor()?
            .get_two::<HeaderWithHashMask<base_common_types_chain::Header>>((&block_hash).into())?
            .filter(|(_, hash)| hash == &block_hash)
            .map(|(header, _)| header))
    }

    fn header_by_number(
        &self,
        num: BlockNumber,
    ) -> ProviderResult<Option<base_common_types_chain::Header>> {
        self.cursor()?.get_one::<HeaderMask<base_common_types_chain::Header>>(num.into())
    }

    fn headers_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<base_common_types_chain::Header>> {
        let mut cursor = self.cursor()?;
        let mut headers = Vec::with_capacity(range_size_hint(&range).unwrap_or(1024));

        for num in to_range(range) {
            if let Some(header) =
                cursor.get_one::<HeaderMask<base_common_types_chain::Header>>(num.into())?
            {
                headers.push(header);
            }
        }

        Ok(headers)
    }

    fn sealed_header(&self, number: BlockNumber) -> ProviderResult<Option<SealedHeader>> {
        Ok(self
            .cursor()?
            .get_two::<HeaderWithHashMask<base_common_types_chain::Header>>(number.into())?
            .map(|(header, hash)| SealedHeader::new(header, hash)))
    }

    fn sealed_headers_while(
        &self,
        range: impl RangeBounds<BlockNumber>,
        mut predicate: impl FnMut(&SealedHeader) -> bool,
    ) -> ProviderResult<Vec<SealedHeader>> {
        let mut cursor = self.cursor()?;
        let mut headers = Vec::with_capacity(range_size_hint(&range).unwrap_or(1024));

        for number in to_range(range) {
            if let Some((header, hash)) = cursor
                .get_two::<HeaderWithHashMask<base_common_types_chain::Header>>(number.into())?
            {
                let sealed = SealedHeader::new(header, hash);
                if !predicate(&sealed) {
                    break;
                }
                headers.push(sealed);
            }
        }
        Ok(headers)
    }
}

impl BlockHashReader for StaticFileJarProvider<'_> {
    fn block_hash(&self, number: u64) -> ProviderResult<Option<B256>> {
        self.cursor()?.get_one::<BlockHashMask>(number.into())
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        let mut cursor = self.cursor()?;
        let mut hashes = Vec::with_capacity((end - start) as usize);

        for number in start..end {
            if let Some(hash) = cursor.get_one::<BlockHashMask>(number.into())? {
                hashes.push(hash)
            }
        }
        Ok(hashes)
    }
}

impl BlockNumReader for StaticFileJarProvider<'_> {
    fn chain_info(&self) -> ProviderResult<ChainInfo> {
        // Information on live database
        Err(ProviderError::UnsupportedProvider)
    }

    fn best_block_number(&self) -> ProviderResult<BlockNumber> {
        // Information on live database
        Err(ProviderError::UnsupportedProvider)
    }

    fn last_block_number(&self) -> ProviderResult<BlockNumber> {
        // Information on live database
        Err(ProviderError::UnsupportedProvider)
    }

    fn block_number(&self, hash: B256) -> ProviderResult<Option<BlockNumber>> {
        let mut cursor = self.cursor()?;

        Ok(cursor
            .get_one::<BlockHashMask>((&hash).into())?
            .and_then(|res| (res == hash).then(|| cursor.number()).flatten()))
    }
}

impl TransactionsProvider for StaticFileJarProvider<'_> {
    type Transaction = BaseTxEnvelope;

    fn transaction_id(&self, hash: TxHash) -> ProviderResult<Option<TxNumber>> {
        let mut cursor = self.cursor()?;

        Ok(cursor
            .get_one::<TransactionMask<Self::Transaction>>((&hash).into())?
            .and_then(|res| (*res.tx_hash() == hash).then(|| cursor.number()).flatten()))
    }

    fn transaction_by_id(&self, num: TxNumber) -> ProviderResult<Option<Self::Transaction>> {
        self.cursor()?.get_one::<TransactionMask<Self::Transaction>>(num.into())
    }

    fn transaction_by_id_unhashed(
        &self,
        num: TxNumber,
    ) -> ProviderResult<Option<Self::Transaction>> {
        self.cursor()?.get_one::<TransactionMask<Self::Transaction>>(num.into())
    }

    fn transaction_by_hash(&self, hash: TxHash) -> ProviderResult<Option<Self::Transaction>> {
        self.cursor()?.get_one::<TransactionMask<Self::Transaction>>((&hash).into())
    }

    fn transaction_by_hash_with_meta(
        &self,
        _hash: TxHash,
    ) -> ProviderResult<Option<(Self::Transaction, TransactionMeta)>> {
        // Information required on indexing table [`tables::TransactionBlocks`]
        Err(ProviderError::UnsupportedProvider)
    }

    fn transactions_by_block(
        &self,
        _block_id: BlockHashOrNumber,
    ) -> ProviderResult<Option<Vec<Self::Transaction>>> {
        // Related to indexing tables. Live database should get the tx_range and call static file
        // provider with `transactions_by_tx_range` instead.
        Err(ProviderError::UnsupportedProvider)
    }

    fn transactions_by_block_range(
        &self,
        _range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<Vec<Self::Transaction>>> {
        // Related to indexing tables. Live database should get the tx_range and call static file
        // provider with `transactions_by_tx_range` instead.
        Err(ProviderError::UnsupportedProvider)
    }

    fn transactions_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Self::Transaction>> {
        let mut cursor = self.cursor()?;
        let mut txs = Vec::with_capacity(range_size_hint(&range).unwrap_or(1024));

        for num in to_range(range) {
            if let Some(tx) = cursor.get_one::<TransactionMask<Self::Transaction>>(num.into())? {
                txs.push(tx)
            }
        }
        Ok(txs)
    }

    fn senders_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Address>> {
        let mut cursor = self.cursor()?;
        let mut senders = Vec::with_capacity(range_size_hint(&range).unwrap_or(1024));

        for num in to_range(range) {
            if let Some(tx) = cursor.get_one::<TransactionSenderMask>(num.into())? {
                senders.push(tx)
            }
        }
        Ok(senders)
    }

    fn transaction_sender(&self, id: TxNumber) -> ProviderResult<Option<Address>> {
        self.cursor()?.get_one::<TransactionSenderMask>(id.into())
    }
}

impl ReceiptProvider for StaticFileJarProvider<'_> {
    type Receipt = BaseReceipt;

    fn receipt(&self, num: TxNumber) -> ProviderResult<Option<Self::Receipt>> {
        self.cursor()?.get_one::<ReceiptMask<Self::Receipt>>(num.into())
    }

    fn receipt_by_hash(&self, hash: TxHash) -> ProviderResult<Option<Self::Receipt>> {
        if let Some(tx_static_file) = &self.auxiliary_jar
            && let Some(num) = tx_static_file.transaction_id(hash)?
        {
            return self.receipt(num);
        }
        Ok(None)
    }

    fn receipts_by_block(
        &self,
        _block: BlockHashOrNumber,
    ) -> ProviderResult<Option<Vec<Self::Receipt>>> {
        // Related to indexing tables. StaticFile should get the tx_range and call static file
        // provider with `receipt()` instead for each
        Err(ProviderError::UnsupportedProvider)
    }

    fn receipts_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Self::Receipt>> {
        let mut cursor = self.cursor()?;
        let mut receipts = Vec::with_capacity(range_size_hint(&range).unwrap_or(1024));

        for num in to_range(range) {
            if let Some(tx) = cursor.get_one::<ReceiptMask<Self::Receipt>>(num.into())? {
                receipts.push(tx)
            }
        }
        Ok(receipts)
    }

    fn receipts_by_block_range(
        &self,
        _block_range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<Vec<Self::Receipt>>> {
        // Related to indexing tables. StaticFile should get the tx_range and call static file
        // provider with `receipt()` instead for each
        Err(ProviderError::UnsupportedProvider)
    }
}
