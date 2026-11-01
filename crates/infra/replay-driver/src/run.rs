use std::{
    collections::{HashMap, HashSet},
    time::{Duration, UNIX_EPOCH},
};

use alloy_consensus::Block;
use alloy_provider::Provider;
use base_clock::GlobalClock;
use base_common_chains::Upgrades;
use base_common_consensus::BaseTxEnvelope;
use base_common_network::{Base, BaseEngineApi};
use eyre::{OptionExt, Result, WrapErr, ensure};
use tokio_util::sync::CancellationToken;

use crate::{
    BlockDiff, BuilderIngressInjector, CanonicalBlock, DiffReport, InjectionSchedule, Injector,
    ReplayDataset, ShadowDriver,
};

/// Runs one export against an already initialized canonical parent and replay-clock builder.
#[derive(Debug)]
pub struct ReplayRunner<'a, P, E, I, C> {
    /// Source of full canonical blocks and receipts.
    pub canonical: P,
    /// Builder Engine API. Candidates are never imported.
    pub engine: E,
    /// Builder ingress delivery adapter.
    pub injector: I,
    /// Fork schedule matching the exported chain.
    pub chain: &'a C,
    /// Per-phase timeout and candidate retrieval policy.
    pub driver: ShadowDriver,
}

impl<P, E, I, C> ReplayRunner<'_, P, E, I, C>
where
    P: Provider<Base>,
    E: BaseEngineApi + Sync,
    I: Injector,
    C: Upgrades,
{
    /// Maximum lead for a whole-second build, including large parent timestamp gaps.
    pub const BUILD_LEAD_MS: u64 = 2_000;

    /// Acknowledge warmup, drive inclusive export heights sequentially, and join scheduled ingress.
    /// The first exported block is retained as warmup and excluded from report aggregates.
    /// Errors stop both futures; cancellation can leave an engine request in flight.
    pub async fn run(&self, dataset: ReplayDataset) -> Result<DiffReport> {
        let dataset = ReplayDataset::new(dataset.manifest, dataset.records)?;
        let window = &dataset.manifest.window;
        let chain_id =
            tokio::time::timeout(self.driver.timeout, self.canonical.get_chain_id()).await??;
        ensure!(chain_id == window.chain_id, "canonical chain ID differs from export");
        let parent = self.block(window.from_block - 1).await?;
        let mut prepared = Vec::new();
        for number in window.from_block..=window.to_block {
            let block = self.block(number).await?;
            let receipts = tokio::time::timeout(
                self.driver.timeout,
                self.canonical.get_block_receipts(block.hash_slow().into()),
            )
            .await??;
            let baseline = CanonicalBlock::from_block(&block, receipts.as_deref());
            prepared.push((block, baseline));
        }
        let first = &prepared[0].0;
        ensure!(
            first.header.timestamp.checked_mul(1000) == Some(window.from_timestamp_ms),
            "export start timestamp differs from canonical block"
        );
        ensure!(first.header.parent_hash == parent.hash_slow(), "canonical parent mismatch");
        let now = GlobalClock::get().now();
        ensure!(
            InjectionSchedule::delay(window.from_timestamp_ms, now)? > Duration::ZERO,
            "first builder deadline already expired"
        );
        let cutoff = parent
            .header
            .timestamp
            .checked_mul(1000)
            .ok_or_eyre("parent timestamp overflow")?
            .max(window.from_timestamp_ms.saturating_sub(Self::BUILD_LEAD_MS));
        let mut recorded =
            dataset.records.iter().map(|record| record.tx_hash).collect::<Option<HashSet<_>>>();
        let records: HashMap<_, _> = dataset
            .records
            .iter()
            .filter_map(|record| record.tx_hash.map(|hash| (hash, record.clone())))
            .collect();
        let mut schedule = InjectionSchedule::new(dataset.records);
        schedule.timeout = self.driver.timeout;
        schedule.warmup(&self.injector, cutoff).await?;
        let drive = async {
            let mut diffs = Vec::new();
            let mut parent_hash = parent.hash_slow();
            let mut parent_timestamp = parent.header.timestamp;
            for (number, (block, baseline)) in (window.from_block..=window.to_block).zip(prepared) {
                ensure!(
                    block.header.number == number && block.header.parent_hash == parent_hash,
                    "canonical chain changed during replay"
                );
                ensure!(
                    block.header.timestamp > parent_timestamp,
                    "replay requires increasing block timestamps"
                );
                if number == window.to_block {
                    ensure!(
                        block.header.timestamp.checked_mul(1000) == Some(window.to_timestamp_ms),
                        "export end timestamp differs from canonical block"
                    );
                }
                if let Some(recorded) = &mut recorded {
                    recorded.extend(
                        block
                            .body
                            .transactions
                            .iter()
                            .filter(|tx| tx.is_deposit())
                            .map(|tx| tx.tx_hash()),
                    );
                }
                let deadline = block
                    .header
                    .timestamp
                    .checked_mul(1000)
                    .ok_or_eyre("block timestamp overflow")?;
                let build_start = parent_timestamp
                    .checked_mul(1000)
                    .ok_or_eyre("parent timestamp overflow")?
                    .max(deadline.saturating_sub(Self::BUILD_LEAD_MS));
                tokio::time::sleep(InjectionSchedule::delay(
                    build_start,
                    GlobalClock::get().now(),
                )?)
                .await;
                let driver = ShadowDriver {
                    build_deadline: Some(UNIX_EPOCH + Duration::from_millis(deadline)),
                    ..self.driver
                };
                let candidate = driver.replay_block(&self.engine, &block, self.chain).await?;
                let canonical: HashSet<_> =
                    block.body.transactions.iter().map(|tx| tx.tx_hash()).collect();
                let mut restored = 0;
                let mut obsolete = 0;
                tokio::time::timeout(self.driver.timeout, async {
                    for hash in &candidate.transaction_hashes {
                        if canonical.contains(hash) {
                            continue;
                        }
                        let record = records.get(hash).ok_or_else(|| {
                            eyre::eyre!("candidate-only transaction {hash} has no recorded ingress")
                        })?;
                        match self.injector.inject(record.clone()).await {
                            Ok(()) => restored += 1,
                            Err(error) if BuilderIngressInjector::is_obsolete(&error, *hash) => {
                                obsolete += 1;
                                tracing::warn!(block_number = number, tx_hash = %hash, error = %error, "skipped obsolete pool restoration");
                            }
                            Err(error) => return Err(error).wrap_err_with(|| {
                                format!("restoring transaction {hash} after block {number}")
                            }),
                        }
                    }
                    Ok::<_, eyre::Report>(())
                })
                .await
                .wrap_err_with(|| format!("pool restoration timed out after block {number}"))??;
                tracing::info!(
                    block_number = number,
                    restored,
                    obsolete,
                    "restored candidate-only transactions"
                );
                diffs.push(BlockDiff::compare(&candidate, &baseline, recorded.as_ref())?);
                parent_hash = block.hash_slow();
                parent_timestamp = block.header.timestamp;
            }
            Ok::<_, eyre::Report>(DiffReport::new(window.from_block - 1, diffs)?)
        };
        let cancel = CancellationToken::new();
        let (report, ()) = tokio::try_join!(drive, schedule.run(&self.injector, &cancel))?;
        Ok(report)
    }

    /// Fetch a complete canonical block with a bounded RPC request.
    pub async fn block(&self, number: u64) -> Result<Block<BaseTxEnvelope>> {
        let block = tokio::time::timeout(
            self.driver.timeout,
            self.canonical.get_block_by_number(number.into()).full(),
        )
        .await??
        .ok_or_else(|| eyre::eyre!("canonical block {number} unavailable"))?;
        Ok(block
            .map_header(|header| header.inner)
            .into_consensus()
            .map_transactions(|tx| tx.inner.into_inner()))
    }
}
