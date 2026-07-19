use std::{
    collections::BTreeSet,
    fmt::Debug,
    sync::{Arc, RwLock},
    time::Instant,
};

use alloy_consensus::{Header, Sealed};
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use base_flashblocks::{FlashblocksAPI, FlashblocksState, PendingBlocks};
use base_mev_trader::{
    BundleVisitor, PayloadVisitor, PendingSnapshotView, PortError, SnapshotHandle,
    SnapshotHandleFactory, TraderSnapshotPort, TransactionVisitor, VisitControl, VisitSummary,
};
use reth_provider::{HeaderProvider, StateProviderBox, StateProviderFactory};

#[derive(Debug)]
struct PendingSnapshotViewAdapter {
    pending: Arc<PendingBlocks>,
}

impl PendingSnapshotView for PendingSnapshotViewAdapter {
    fn parent_hash(&self) -> B256 {
        self.pending.parent_hash()
    }

    fn latest_block_number(&self) -> u64 {
        self.pending.latest_block_number()
    }

    fn canonical_block_number(&self) -> u64 {
        match self.pending.canonical_block_number() {
            BlockNumberOrTag::Number(number) => number,
            _ => 0,
        }
    }

    fn latest_flashblock_index(&self) -> u64 {
        self.pending.latest_flashblock_index()
    }

    fn latest_header(&self) -> Sealed<Header> {
        self.pending.latest_header()
    }

    fn latest_block_transaction_count(&self) -> usize {
        self.pending.latest_block_transaction_count()
    }

    fn has_transaction_hash(&self, transaction_hash: B256) -> bool {
        self.pending.has_transaction_hash(&transaction_hash)
    }

    fn transaction_position(&self, block_number: u64, transaction_hash: B256) -> Option<usize> {
        self.pending.transaction_position(block_number, &transaction_hash)
    }

    fn visit_latest_block_payloads(
        &self,
        visitor: &mut dyn PayloadVisitor,
    ) -> Result<VisitSummary, PortError> {
        let mut visited = 0u32;
        for flashblock in self.pending.latest_block_flashblocks_iter() {
            visited = visited.checked_add(1).ok_or(PortError::LimitExceeded)?;
            if visitor.visit(flashblock.payload_id, flashblock.index)? == VisitControl::Stop {
                return Ok(VisitSummary { visited, complete: false });
            }
        }
        Ok(VisitSummary { visited, complete: true })
    }

    fn visit_transactions_for_block(
        &self,
        block_number: u64,
        start: usize,
        limit: usize,
        visitor: &mut dyn TransactionVisitor,
    ) -> Result<VisitSummary, PortError> {
        let mut transactions = self.pending.get_transactions_for_block(block_number).skip(start);
        let mut visited = 0u32;
        for position in start..start.saturating_add(limit) {
            let Some(transaction) = transactions.next() else {
                return Ok(VisitSummary { visited, complete: true });
            };
            visited = visited.checked_add(1).ok_or(PortError::LimitExceeded)?;
            if visitor.visit(position, transaction.inner.inner.inner())? == VisitControl::Stop {
                return Ok(VisitSummary { visited, complete: false });
            }
        }
        Ok(VisitSummary { visited, complete: transactions.next().is_none() })
    }

    fn visit_bundle(&self, visitor: &mut dyn BundleVisitor) -> Result<VisitSummary, PortError> {
        let bundle = self.pending.get_bundle_state();
        let mut visited = 0u32;
        let mut code_hashes = BTreeSet::new();
        for (address, account) in bundle.state() {
            visited = visited.checked_add(1).ok_or(PortError::LimitExceeded)?;
            if visitor.visit_account(*address, account)? == VisitControl::Stop {
                return Ok(VisitSummary { visited, complete: false });
            }
            if let Some(info) = account.account_info()
                && !info.code_hash.is_zero()
            {
                code_hashes.insert(info.code_hash);
            }
        }
        for code_hash in code_hashes {
            let Some(bytecode) = bundle.bytecode(&code_hash) else { continue };
            visited = visited.checked_add(1).ok_or(PortError::LimitExceeded)?;
            if visitor.visit_contract(code_hash, &bytecode)? == VisitControl::Stop {
                return Ok(VisitSummary { visited, complete: false });
            }
        }
        Ok(VisitSummary { visited, complete: true })
    }
}

#[derive(Debug)]
struct PendingSnapshotRecord {
    view: Arc<dyn PendingSnapshotView + Send + Sync>,
    pending: Arc<PendingBlocks>,
    received_at: Instant,
}

#[derive(Debug)]
pub(crate) struct CliTraderSnapshotPort<Provider> {
    flashblocks: Arc<FlashblocksState>,
    provider: Provider,
    current_record: RwLock<Option<Arc<PendingSnapshotRecord>>>,
}

impl<Provider> CliTraderSnapshotPort<Provider> {
    pub(crate) fn new(flashblocks: Arc<FlashblocksState>, provider: Provider) -> Self {
        Self { flashblocks, provider, current_record: RwLock::new(None) }
    }

    pub(crate) fn record_pending_snapshot(
        &self,
        pending: Arc<PendingBlocks>,
        received_at: Instant,
    ) {
        let view: Arc<dyn PendingSnapshotView + Send + Sync> =
            Arc::new(PendingSnapshotViewAdapter { pending: Arc::clone(&pending) });
        let record = Arc::new(PendingSnapshotRecord { view, pending, received_at });
        *self.current_record.write().unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(record);
    }

    fn current_record(&self) -> Option<Arc<PendingSnapshotRecord>> {
        self.current_record.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    fn record_is_current(&self, record: &PendingSnapshotRecord) -> bool {
        let current = self.flashblocks.get_pending_blocks();
        current.as_ref().is_some_and(|pending| Arc::ptr_eq(pending, &record.pending))
    }
}

impl<Provider> TraderSnapshotPort for CliTraderSnapshotPort<Provider>
where
    Provider: StateProviderFactory
        + HeaderProvider<Header = Header>
        + Clone
        + Debug
        + Send
        + Sync
        + 'static,
{
    fn capture_latest(
        &self,
        factory: &SnapshotHandleFactory,
    ) -> Result<Option<SnapshotHandle>, PortError> {
        let Some(record) = self.current_record() else { return Ok(None) };
        if !self.record_is_current(&record) {
            return Ok(None);
        }
        factory.issue(Arc::clone(&record.view), record.received_at).map(Some)
    }

    fn is_current_authoritative(&self, handle: &SnapshotHandle) -> bool {
        let Some(record) = self.current_record() else { return false };
        self.record_is_current(&record) && handle.matches_capture(&record.view, record.received_at)
    }

    fn state_at_hash(&self, block_hash: B256) -> Result<StateProviderBox, PortError> {
        self.provider.state_by_block_hash(block_hash).map_err(|_| PortError::ProviderUnavailable)
    }

    fn sealed_header_at_hash(&self, block_hash: B256) -> Result<Sealed<Header>, PortError> {
        let header = self
            .provider
            .sealed_header_by_hash(block_hash)
            .map_err(|_| PortError::HeaderUnavailable)?
            .ok_or(PortError::HeaderUnavailable)?;
        Ok(Sealed::new_unchecked(header.clone_header(), header.hash()))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bloom, Bytes, U256};
    use base_common_flashblocks::{
        ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, Metadata,
    };
    use base_flashblocks::PendingBlocksBuilder;

    use super::*;

    fn pending_blocks() -> PendingBlocks {
        let parent_hash = B256::with_last_byte(1);
        let mut builder = PendingBlocksBuilder::default();
        builder.with_flashblocks([Flashblock {
            payload_id: Default::default(),
            index: 0,
            base: Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: B256::ZERO,
                parent_hash,
                fee_recipient: Address::ZERO,
                prev_randao: B256::ZERO,
                block_number: 100,
                gas_limit: 30_000_000,
                timestamp: 1,
                extra_data: Bytes::new(),
                base_fee_per_gas: U256::from(1),
            }),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::default(),
                gas_used: 0,
                block_hash: B256::with_last_byte(2),
                transactions: Vec::new(),
                withdrawals: Vec::new(),
                withdrawals_root: B256::ZERO,
                blob_gas_used: None,
            },
            metadata: Metadata::new(100),
        }]);
        builder.with_header(Sealed::new_unchecked(
            Header { parent_hash, number: 100, ..Default::default() },
            B256::with_last_byte(2),
        ));
        builder.build().expect("pending blocks")
    }

    #[test]
    fn adapter_requires_live_some_and_pointer_identical_capture() {
        let flashblocks = Arc::new(FlashblocksState::default());
        flashblocks.set_pending_blocks_for_testing(Some(pending_blocks()));
        let current = flashblocks.get_pending_blocks();
        let captured = Arc::clone(current.as_ref().expect("current pending"));
        drop(current);

        let port = CliTraderSnapshotPort::new(Arc::clone(&flashblocks), ());
        port.record_pending_snapshot(captured, Instant::now());
        let record = port.current_record().expect("captured record");
        assert!(port.record_is_current(&record));

        flashblocks.set_pending_blocks_for_testing(None);
        assert!(!port.record_is_current(&record));
    }
}
