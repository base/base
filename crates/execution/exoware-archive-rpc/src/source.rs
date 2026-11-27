//! [`BlockSource`] and [`NodeState`] over a reth provider.

use alloy_consensus::{Header, constants::KECCAK_EMPTY};
use alloy_primitives::{Address, B256, Bytes, U256};
use async_trait::async_trait;
use base_common_consensus::{BaseBlock, BaseReceipt, BaseTxEnvelope};
use base_exoware_archive::{
    AccountChange, AccountInfo, ArchiveError, BlockBody, BlockSource, LocalAvailability, NodeState,
    StorageChange,
};
use reth_primitives_traits::Account;
use reth_prune_types::PruneSegment;
use reth_storage_api::{
    BlockIdReader, BlockNumReader, BlockReader, ChangeSetReader, HeaderProvider,
    PruneCheckpointReader, ReceiptProvider, StateProviderFactory, StorageChangeSetReader,
};

/// Provider capabilities the exporter and the reader's node fallback need.
pub trait ArchiveProvider:
    BlockNumReader
    + BlockIdReader
    + HeaderProvider<Header = Header>
    + BlockReader<Block = BaseBlock, Transaction = BaseTxEnvelope, Receipt = BaseReceipt>
    + ReceiptProvider<Receipt = BaseReceipt>
    + ChangeSetReader
    + StorageChangeSetReader
    + PruneCheckpointReader
    + StateProviderFactory
    + Clone
    + Send
    + Sync
    + 'static
{
}

impl<P> ArchiveProvider for P where
    P: BlockNumReader
        + BlockIdReader
        + HeaderProvider<Header = Header>
        + BlockReader<Block = BaseBlock, Transaction = BaseTxEnvelope, Receipt = BaseReceipt>
        + ReceiptProvider<Receipt = BaseReceipt>
        + ChangeSetReader
        + StorageChangeSetReader
        + PruneCheckpointReader
        + StateProviderFactory
        + Clone
        + Send
        + Sync
        + 'static
{
}

/// The node's provider as the exporter's [`BlockSource`] and the reader's [`NodeState`].
#[derive(Debug, Clone)]
pub struct ProviderSource<P> {
    /// Node provider.
    pub provider: P,
}

impl<P: ArchiveProvider> ProviderSource<P> {
    /// Wraps a provider.
    pub const fn new(provider: P) -> Self {
        Self { provider }
    }

    /// Maps a provider error.
    pub fn local(err: impl std::fmt::Display) -> ArchiveError {
        ArchiveError::Local { reason: err.to_string() }
    }

    /// Lowest block a prune segment still holds: one past its checkpoint, or 0.
    pub fn retained_from(&self, segment: PruneSegment) -> Result<u64, ArchiveError> {
        Ok(self
            .provider
            .get_prune_checkpoint(segment)
            .map_err(Self::local)?
            .and_then(|checkpoint| checkpoint.block_number)
            .map_or(0, |pruned| pruned.saturating_add(1)))
    }

    /// The archive's account fields of a reth account. An account without code has
    /// `KECCAK_EMPTY` as its code hash.
    pub fn account_info(account: Account) -> AccountInfo {
        AccountInfo {
            nonce: account.nonce,
            balance: account.balance,
            code_hash: account.bytecode_hash.unwrap_or(KECCAK_EMPTY),
        }
    }

    /// Runs a provider read on the blocking pool.
    pub async fn blocking<T: Send + 'static>(
        &self,
        read: impl FnOnce(&P) -> Result<T, ArchiveError> + Send + 'static,
    ) -> Result<T, ArchiveError> {
        let provider = self.provider.clone();
        tokio::task::spawn_blocking(move || read(&provider))
            .await
            .map_err(|err| Self::local(format!("provider task failed: {err}")))?
    }
}

impl<P: ArchiveProvider> BlockSource for ProviderSource<P> {
    fn finalized_block_number(&self) -> Result<Option<u64>, ArchiveError> {
        self.provider.finalized_block_number().map_err(Self::local)
    }

    fn availability(&self) -> Result<LocalAvailability, ArchiveError> {
        let earliest = self.provider.earliest_block_number().map_err(Self::local)?;
        Ok(LocalAvailability {
            headers: 0,
            bodies: earliest.max(self.retained_from(PruneSegment::Bodies)?),
            receipts: earliest.max(self.retained_from(PruneSegment::Receipts)?),
            senders: earliest.max(self.retained_from(PruneSegment::SenderRecovery)?),
            account_changes: self.retained_from(PruneSegment::AccountHistory)?,
            storage_changes: self.retained_from(PruneSegment::StorageHistory)?,
        })
    }

    fn header(&self, number: u64) -> Result<Option<Header>, ArchiveError> {
        self.provider.header_by_number(number).map_err(Self::local)
    }

    fn body(&self, number: u64) -> Result<Option<BlockBody>, ArchiveError> {
        Ok(self.provider.block_by_number(number).map_err(Self::local)?.map(|block| BlockBody {
            transactions: block.body.transactions,
            withdrawals: block.body.withdrawals,
        }))
    }

    fn receipts(&self, number: u64) -> Result<Option<Vec<BaseReceipt>>, ArchiveError> {
        self.provider.receipts_by_block(number.into()).map_err(Self::local)
    }

    fn senders(&self, number: u64) -> Result<Option<Vec<Address>>, ArchiveError> {
        let Some(indices) = self.provider.block_body_indices(number).map_err(Self::local)? else {
            return Ok(None);
        };
        if indices.tx_count == 0 {
            return Ok(Some(Vec::new()));
        }
        // A pruned or never-written sender table reads short; the loader then recovers.
        Ok(self.provider.senders_by_tx_range(indices.first_tx_num..indices.next_tx_num()).ok())
    }

    fn account_changes(&self, number: u64) -> Result<Vec<AccountChange>, ArchiveError> {
        Ok(self
            .provider
            .account_block_changeset(number)
            .map_err(Self::local)?
            .into_iter()
            .map(|change| AccountChange {
                address: change.address,
                before: change.info.map(Self::account_info),
            })
            .collect())
    }

    fn storage_changes(&self, number: u64) -> Result<Vec<StorageChange>, ArchiveError> {
        Ok(self
            .provider
            .storage_block_changeset(number)
            .map_err(Self::local)?
            .into_iter()
            .map(|change| StorageChange {
                address: change.address,
                slot: change.key,
                before: change.value,
            })
            .collect())
    }
}

#[async_trait]
impl<P: ArchiveProvider> NodeState for ProviderSource<P> {
    async fn canonical_hash(&self, number: u64) -> Result<Option<B256>, ArchiveError> {
        self.blocking(move |p| p.block_hash(number).map_err(Self::local)).await
    }

    async fn account_at(
        &self,
        block: u64,
        address: Address,
    ) -> Result<Option<AccountInfo>, ArchiveError> {
        self.blocking(move |p| {
            let state = p.history_by_block_number(block).map_err(Self::local)?;
            Ok(state.basic_account(&address).map_err(Self::local)?.map(Self::account_info))
        })
        .await
    }

    async fn storage_at(
        &self,
        block: u64,
        address: Address,
        slot: B256,
    ) -> Result<U256, ArchiveError> {
        self.blocking(move |p| {
            let state = p.history_by_block_number(block).map_err(Self::local)?;
            Ok(state.storage(address, slot).map_err(Self::local)?.unwrap_or_default())
        })
        .await
    }

    async fn bytecode(&self, code_hash: B256) -> Result<Option<Bytes>, ArchiveError> {
        self.blocking(move |p| {
            let state = p.latest().map_err(Self::local)?;
            Ok(state
                .bytecode_by_hash(&code_hash)
                .map_err(Self::local)?
                .map(|code| code.original_bytes()))
        })
        .await
    }
}
