//! Deterministic Dowse state-prefetch benchmarks for canonical block replay.

use std::{collections::HashSet, fs, path::Path, sync::Arc, thread, time::Instant};

use alloy_consensus::{Transaction, transaction::SignerRecoverable};
use base_common_consensus::BaseBlock;
use base_execution_chainspec::BaseChainSpec;
use dowse_plan::{PlanLimits, PrefetchPlanner};
use dowse_types::HintTable;
use eyre::{Result as EyreResult, eyre};
use reth_execution_cache::ExecutionCache;
use reth_primitives_traits::Block as BlockT;
use reth_provider::{HeaderProvider, StateProvider, StateProviderFactory};

use crate::{
    DowseBlockBenchmarkResponse, DowseBlockReplayResponse, DowsePrefetchStats, meter_block,
    meter_block_with_optional_cache,
};

/// Static hint table and worker settings for block replay benchmarks.
#[derive(Clone, Debug)]
pub struct DowseBenchmarkConfig {
    hints: Arc<HintTable>,
    worker_count: usize,
    cache_size_bytes: usize,
}

impl DowseBenchmarkConfig {
    /// Loads a JSON hint table and validates benchmark resource settings.
    pub fn load(
        hints_path: impl AsRef<Path>,
        worker_count: usize,
        cache_size_bytes: usize,
    ) -> EyreResult<Self> {
        let path = hints_path.as_ref();
        let bytes = fs::read(path).map_err(|error| {
            eyre::eyre!("failed to read Dowse benchmark hints at {}: {error}", path.display())
        })?;
        let hints: HintTable = serde_json::from_slice(&bytes).map_err(|error| {
            eyre::eyre!("failed to parse Dowse benchmark hints at {}: {error}", path.display())
        })?;
        eyre::ensure!(hints.version == 1, "unsupported Dowse hint table version {}", hints.version);
        eyre::ensure!(worker_count > 0, "Dowse benchmark worker count must be greater than zero");
        eyre::ensure!(cache_size_bytes > 0, "Dowse benchmark cache size must be greater than zero");

        Ok(Self { hints: Arc::new(hints), worker_count, cache_size_bytes })
    }
}

/// Plans and prefetches canonical transactions, then replays the block with and without the cache.
pub fn benchmark_dowse_block<P>(
    provider: P,
    chain_spec: Arc<BaseChainSpec>,
    block: &BaseBlock,
    config: &DowseBenchmarkConfig,
    cached_first: bool,
) -> EyreResult<DowseBlockBenchmarkResponse>
where
    P: StateProviderFactory + HeaderProvider<Header = alloy_consensus::Header> + Clone + Sync,
{
    let raw = (!cached_first)
        .then(|| meter_block(provider.clone(), Arc::clone(&chain_spec), block))
        .transpose()?;
    let (cache, prefetch) = prefetch_dowse_block(provider.clone(), block, config)?;
    let cached = meter_block_with_optional_cache(
        provider.clone(),
        Arc::clone(&chain_spec),
        block,
        Some(cache),
    )?;
    let raw = match raw {
        Some(raw) => raw,
        None => meter_block(provider, chain_spec, block)?,
    };

    eyre::ensure!(
        raw.transactions.len() == cached.transactions.len()
            && raw
                .transactions
                .iter()
                .zip(&cached.transactions)
                .all(|(raw, cached)| raw.tx_hash == cached.tx_hash
                    && raw.gas_used == cached.gas_used),
        "raw and cache-backed Dowse replays produced different transaction results"
    );

    Ok(DowseBlockBenchmarkResponse { cached_first, prefetch, raw, cached })
}

/// Replays one canonical block exactly once, optionally using a cache populated from Dowse plans.
pub fn replay_dowse_block<P>(
    provider: P,
    chain_spec: Arc<BaseChainSpec>,
    block: &BaseBlock,
    config: &DowseBenchmarkConfig,
    dowse_cache_enabled: bool,
) -> EyreResult<DowseBlockReplayResponse>
where
    P: StateProviderFactory + HeaderProvider<Header = alloy_consensus::Header> + Clone + Sync,
{
    let (cache, prefetch) = if dowse_cache_enabled {
        let (cache, prefetch) = prefetch_dowse_block(provider.clone(), block, config)?;
        (Some(cache), Some(prefetch))
    } else {
        (None, None)
    };
    let replay = meter_block_with_optional_cache(provider, chain_spec, block, cache)?;

    Ok(DowseBlockReplayResponse { dowse_cache_enabled, prefetch, replay })
}

fn prefetch_dowse_block<P>(
    provider: P,
    block: &BaseBlock,
    config: &DowseBenchmarkConfig,
) -> EyreResult<(ExecutionCache, DowsePrefetchStats)>
where
    P: StateProviderFactory + Clone + Sync,
{
    let planning_start = Instant::now();
    let planner = PrefetchPlanner::new(&config.hints, PlanLimits::new(32, 256));
    let mut accounts = HashSet::new();
    let mut storage = HashSet::new();
    let mut planned_transactions = 0;

    for transaction in block.body().transactions() {
        let Some(target) = transaction.to() else {
            continue;
        };
        let sender = transaction
            .recover_signer()
            .map_err(|error| eyre!("failed to recover transaction signer: {error}"))?;
        let Some(plan) = planner.plan(target, sender, transaction.input()) else {
            continue;
        };
        planned_transactions += 1;
        accounts.extend(plan.accounts);
        storage.extend(plan.storage);
    }

    let planning_time_us = planning_start.elapsed().as_micros();
    let account_targets = accounts.len();
    let storage_targets = storage.len();
    let mut accounts: Vec<_> = accounts.into_iter().collect();
    accounts.sort_unstable();
    let mut storage: Vec<_> = storage.into_iter().collect();
    storage.sort_unstable_by_key(|target| (target.address, target.slot));
    let cache = ExecutionCache::new(config.cache_size_bytes);

    let prefetch_start = Instant::now();
    let total_targets = accounts.len() + storage.len();
    let workers = config.worker_count.min(total_targets);
    let bytecode_targets = if total_targets == 0 {
        0
    } else {
        thread::scope(|scope| -> EyreResult<usize> {
            let mut handles = Vec::with_capacity(workers);
            let accounts = &accounts;
            let storage = &storage;
            for worker in 0..workers {
                let provider = provider.clone();
                let cache = cache.clone();
                let parent_hash = block.header().parent_hash;
                handles.push(scope.spawn(move || -> EyreResult<usize> {
                    let state_provider = provider.state_by_block_hash(parent_hash)?;
                    let mut bytecode_targets = 0;

                    for address in accounts.iter().skip(worker).step_by(workers) {
                        let account = state_provider.basic_account(address)?;
                        cache.insert_account(*address, account);
                        if let Some(code_hash) = account.and_then(|info| info.bytecode_hash) {
                            let code = state_provider.bytecode_by_hash(&code_hash)?;
                            cache.insert_code(code_hash, code);
                            bytecode_targets += 1;
                        }
                    }
                    let storage_offset = (worker + workers - accounts.len() % workers) % workers;
                    for target in storage.iter().skip(storage_offset).step_by(workers) {
                        let value = state_provider.storage(target.address, target.slot)?;
                        cache.insert_storage(target.address, target.slot, value);
                    }

                    Ok(bytecode_targets)
                }));
            }

            handles.into_iter().try_fold(0, |total, handle| {
                let count =
                    handle.join().map_err(|_| eyre!("Dowse benchmark worker panicked"))??;
                Ok(total + count)
            })
        })?
    };
    let prefetch_time_us = prefetch_start.elapsed().as_micros();

    Ok((
        cache,
        DowsePrefetchStats {
            planning_time_us,
            prefetch_time_us,
            planned_transactions,
            account_targets,
            storage_targets,
            bytecode_targets,
            workers,
        },
    ))
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{BlockHeader, Header};
    use alloy_primitives::{Address, B256, U256};
    use alloy_sol_types::SolCall;
    use base_common_consensus::{BaseBlockBody, BaseTransactionSigned};
    use base_node_runner::test_utils::TestHarness;
    use base_test_utils::{Account, SimpleStorage};
    use dowse_types::{PrefetchItem, SlotExpression};
    use reth_transaction_pool::test_utils::TransactionBuilder;

    use super::*;

    #[tokio::test]
    async fn replays_identical_block_with_prefetched_storage() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let (deployment, target, _) =
            Account::Deployer.create_deployment_tx(SimpleStorage::BYTECODE.clone(), 0)?;
        harness.build_block_from_transactions(vec![deployment]).await?;
        let signed = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(target)
            .gas_limit(100_000)
            .max_fee_per_gas(1_000_000_000)
            .max_priority_fee_per_gas(1)
            .input(SimpleStorage::setValueCall { v: U256::from(42) }.abi_encode())
            .into_eip1559();
        let transaction = BaseTransactionSigned::Eip1559(
            signed.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let latest = harness.latest_block();
        let block = BaseBlock::new(
            Header {
                parent_hash: latest.hash(),
                number: latest.number() + 1,
                timestamp: latest.timestamp() + 2,
                gas_limit: 30_000_000,
                beneficiary: Address::random(),
                base_fee_per_gas: Some(1),
                parent_beacon_block_root: Some(B256::ZERO),
                ..Default::default()
            },
            BaseBlockBody { transactions: vec![transaction], ommers: vec![], withdrawals: None },
        );
        let mut hints = HintTable::new();
        hints.insert(
            target,
            B256::repeat_byte(0x11),
            None,
            vec![PrefetchItem::Storage { slot: SlotExpression::Concrete { value: B256::ZERO } }],
        );
        let config = DowseBenchmarkConfig {
            hints: Arc::new(hints),
            worker_count: 2,
            cache_size_bytes: 1_000_000,
        };

        for cached_first in [false, true] {
            let response = benchmark_dowse_block(
                harness.blockchain_provider(),
                harness.chain_spec(),
                &block,
                &config,
                cached_first,
            )?;

            assert_eq!(response.cached_first, cached_first);
            assert_eq!(response.prefetch.planned_transactions, 1);
            assert_eq!(response.prefetch.storage_targets, 1);
            assert_eq!(
                response.raw.transactions[0].gas_used,
                response.cached.transactions[0].gas_used
            );
            assert!(response.raw.state_provider.storage_fetches > 0);
            assert_eq!(response.cached.state_provider.storage_fetches, 0);
        }

        let raw = replay_dowse_block(
            harness.blockchain_provider(),
            harness.chain_spec(),
            &block,
            &config,
            false,
        )?;
        let cached = replay_dowse_block(
            harness.blockchain_provider(),
            harness.chain_spec(),
            &block,
            &config,
            true,
        )?;
        assert!(!raw.dowse_cache_enabled);
        assert!(raw.prefetch.is_none());
        assert!(cached.dowse_cache_enabled);
        assert_eq!(cached.prefetch.as_ref().unwrap().planned_transactions, 1);
        assert_eq!(raw.replay.transactions[0].gas_used, cached.replay.transactions[0].gas_used);
        assert!(raw.replay.state_provider.storage_fetches > 0);
        assert_eq!(cached.replay.state_provider.storage_fetches, 0);
        Ok(())
    }
}
