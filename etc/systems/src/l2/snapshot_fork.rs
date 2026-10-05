//! Discovery of the canonical L1 block whose derivation covers a snapshot's unsafe tail.

use std::{
    fmt,
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};

use alloy_eips::BlockNumberOrTag;
use alloy_provider::{Provider, RootProvider};
use base_common_chains::L1_CONFIGS;
use base_common_network::Base;
use base_consensus_derive::{
    ActivationSignal, OriginProvider, Pipeline, PipelineError, PipelineErrorKind, ResetError,
    ResetSignal, SignalReceiver, StepResult,
};
use base_consensus_engine::AttributesMatch;
use base_consensus_providers::{
    AlloyChainProvider, AlloyL2ChainProvider, BeaconClient, OnlineBeaconClient, OnlineBlobProvider,
    OnlinePipeline,
};
use base_protocol::{BatchValidationProvider, BlockInfo, L2BlockInfo};
use eyre::{OptionExt, Result, WrapErr, bail, ensure, eyre};
use tokio::time::{Instant, sleep, timeout_at};
use url::Url;

use super::SnapshotInspection;

/// Upstream L1 execution and Beacon endpoints used for fork discovery.
///
/// Their URLs may embed credentials, so neither they nor provider errors that could echo them
/// appear in [`fmt::Debug`] output or discovery errors.
#[derive(Clone)]
pub struct SnapshotForkSource {
    /// L1 execution JSON-RPC URL.
    pub execution: Url,
    /// L1 Beacon API URL.
    pub beacon: Url,
}

impl fmt::Debug for SnapshotForkSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnapshotForkSource").finish_non_exhaustive()
    }
}

impl SnapshotForkSource {
    /// Environment variable holding the upstream L1 execution JSON-RPC URL.
    pub const EXECUTION_ENV: &'static str = "SNAPSHOT_UPSTREAM_EXECUTION";
    /// Environment variable holding the upstream L1 Beacon API URL.
    pub const BEACON_ENV: &'static str = "SNAPSHOT_UPSTREAM_BEACON";
    /// Replacement for provider error text that could contain an upstream URL.
    const REDACTED_ERROR: &'static str = "upstream provider request failed";

    /// Reads both endpoints from [`Self::EXECUTION_ENV`] and [`Self::BEACON_ENV`] only.
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            execution: Self::env_url(Self::EXECUTION_ENV)?,
            beacon: Self::env_url(Self::BEACON_ENV)?,
        })
    }

    /// Parses an endpoint environment variable without echoing its value on failure.
    pub fn env_url(name: &str) -> Result<Url> {
        let value =
            std::env::var(name).map_err(|_| eyre!("{name} must be set to an upstream L1 URL"))?;
        Url::parse(&value).map_err(|_| eyre!("{name} is not a valid URL"))
    }

    /// Returns `error` as text unless it could echo an upstream URL.
    pub fn redact(&self, error: impl fmt::Display) -> String {
        let text = error.to_string();
        let leaks = text.contains("://")
            || [&self.execution, &self.beacon]
                .into_iter()
                .any(|url| url.host_str().is_some_and(|host| text.contains(host)));
        if leaks { Self::REDACTED_ERROR.to_string() } else { text }
    }
}

/// Finds the L1 block whose derivation covers a snapshot's unsafe tail with the production
/// [`OnlinePipeline`].
///
/// The snapshot safe head is trusted. Discovery resets the pipeline there, derives payload
/// attributes only for `(safe, latest]`, and requires each to match the snapshot block with
/// [`AttributesMatch::check`]; nothing is executed or written. The result proves the unsafe tail's
/// provenance, not the validity of all history.
#[derive(Debug, Clone)]
pub struct SnapshotForkFinder {
    /// Execution JSON-RPC URL of the snapshot node serving the L2 blocks.
    pub rpc_url: Url,
    /// Upstream L1 endpoints.
    pub source: SnapshotForkSource,
    /// Deadline for the whole discovery.
    pub timeout: Duration,
}

impl SnapshotForkFinder {
    /// L1 and L2 provider cache size for one bounded discovery run.
    const PROVIDER_CACHE_SIZE: usize = 1024;
    /// Delay before retrying after a transient provider error.
    const TRANSIENT_RETRY_DELAY: Duration = Duration::from_millis(500);
    /// Confirmation depth that turns the pipeline's L1 head into an inclusive read limit of
    /// `head - 1`.
    const L1_LIMIT_CONFIRMATIONS: u64 = 1;

    /// Returns the canonical, finalized L1 block at which the latest snapshot block's batch is
    /// derived: the maximum L1 source among the attributes derived for the unsafe tail.
    ///
    /// When safe equals latest, discovery starts from latest's parent so it still locates the
    /// latest batch; that parent must retain its body and cannot be the rollup genesis. L1 reads
    /// stop at the earlier of upstream finalized and latest's L1 origin plus the sequencer
    /// window. Mismatching attributes, reset or critical pipeline errors, an exhausted L1 range,
    /// and the deadline are all fatal.
    pub async fn find(&self, inspection: &SnapshotInspection) -> Result<BlockInfo> {
        let deadline = Instant::now() + self.timeout;
        let config = Arc::new(inspection.rollup_config.clone());
        let genesis = config.genesis;
        let latest = inspection.latest.block_info;
        let timed_out =
            |stage: &str| eyre!("fork discovery timed out after {:?} {stage}", self.timeout);

        let l1_config = L1_CONFIGS.get(&config.l1_chain_id).cloned().ok_or_else(|| {
            eyre!("no built-in L1 chain config for L1 chain ID {}", config.l1_chain_id)
        })?;
        let mut l1 =
            AlloyChainProvider::new_http(self.source.execution.clone(), Self::PROVIDER_CACHE_SIZE);
        let l1_chain_id = timeout_at(deadline, l1.chain_id())
            .await
            .map_err(|_| timed_out("reading the upstream L1 chain ID"))?
            .map_err(|_| eyre!("failed to read the upstream L1 chain ID"))?;
        ensure!(
            l1_chain_id == config.l1_chain_id,
            "upstream L1 chain ID {l1_chain_id} does not match rollup L1 chain ID {}",
            config.l1_chain_id
        );
        let finalized =
            timeout_at(deadline, l1.inner.get_block_by_number(BlockNumberOrTag::Finalized))
                .await
                .map_err(|_| timed_out("reading the upstream finalized L1 block"))?
                .map_err(|_| eyre!("failed to read the upstream finalized L1 block"))?
                .ok_or_eyre("upstream L1 has no finalized block")?
                .header
                .number;
        ensure!(
            finalized >= latest.l1_origin.number,
            "upstream finalized L1 block {finalized} is behind latest snapshot L1 origin {}",
            latest.l1_origin.number
        );
        let l1_limit =
            latest.l1_origin.number.saturating_add(config.seq_window_size).min(finalized);

        let snapshot = RootProvider::<Base>::new_http(self.rpc_url.clone());
        let mut l2 = AlloyL2ChainProvider::new(
            snapshot.clone(),
            Arc::clone(&config),
            Self::PROVIDER_CACHE_SIZE,
        );
        let start = if inspection.safe.block_info == latest {
            ensure!(
                latest.block_info.number > genesis.l2.number + 1,
                "cannot locate the batch for L2 block {}: its parent is the rollup genesis, which \
                 has no L1-info deposit",
                latest.block_info.number
            );
            timeout_at(deadline, l2.l2_block_info_by_hash(latest.block_info.parent_hash))
                .await
                .map_err(|_| timed_out("reading the latest block's parent"))?
                .wrap_err_with(|| {
                    format!(
                        "snapshot parent of L2 block {} is missing or pruned",
                        latest.block_info.number
                    )
                })?
        } else {
            inspection.safe.block_info
        };

        let beacon = OnlineBeaconClient::new_http(self.source.beacon.to_string());
        // Construct from fallible reads instead of `init`, which panics with provider error text.
        let genesis_time = timeout_at(deadline, beacon.genesis_time())
            .await
            .map_err(|_| timed_out("reading upstream Beacon genesis"))?
            .map_err(|_| eyre!("failed to read upstream Beacon genesis time"))?
            .data
            .genesis_time;
        let slot_interval = timeout_at(deadline, beacon.slot_interval())
            .await
            .map_err(|_| timed_out("reading upstream Beacon slot duration"))?
            .map_err(|_| eyre!("failed to read upstream Beacon slot duration"))?
            .data
            .seconds_per_slot;
        ensure!(slot_interval > 0, "upstream Beacon slot duration must be positive");
        let blobs = OnlineBlobProvider { beacon_client: beacon, genesis_time, slot_interval };

        let mut pipeline = OnlinePipeline::new_polled(
            Arc::clone(&config),
            Arc::new(l1_config),
            blobs,
            l1.clone(),
            l2,
            Arc::new(AtomicU64::new(l1_limit + 1)),
            Self::L1_LIMIT_CONFIRMATIONS,
        );
        let mut last_transient = None;
        loop {
            let reset =
                timeout_at(deadline, pipeline.signal(ResetSignal { l2_safe_head: start }.signal()))
                    .await
                    .map_err(|_| {
                        timed_out(&format!(
                            "resetting derivation at L2 block {} (last transient error: {})",
                            start.block_info.number,
                            last_transient.as_deref().unwrap_or("none")
                        ))
                    })?;
            match reset {
                Ok(()) => break,
                Err(PipelineErrorKind::Temporary(error)) => {
                    let error = self.source.redact(error);
                    timeout_at(deadline, sleep(Self::TRANSIENT_RETRY_DELAY)).await.map_err(
                        |_| timed_out(&format!("retrying the derivation reset after: {error}")),
                    )?;
                    last_transient = Some(error);
                }
                Err(error) => bail!(
                    "failed to reset derivation at L2 block {}: {}",
                    start.block_info.number,
                    self.source.redact(error)
                ),
            }
        }

        let mut cursor = start;
        let mut origin = pipeline.origin().map_or(0, |origin| origin.number);
        let mut fork: Option<BlockInfo> = None;
        while cursor.block_info.number < latest.block_info.number {
            let next = cursor.block_info.number + 1;
            // Steps that complete without awaiting I/O never yield to `timeout_at`.
            if Instant::now() >= deadline {
                return Err(timed_out(&format!(
                    "deriving L2 block {next} at L1 origin {origin} (last transient error: {})",
                    last_transient.as_deref().unwrap_or("none")
                )));
            }

            if let Some(attributes) = pipeline.next() {
                let block = timeout_at(deadline, snapshot.get_block_by_number(next.into()).full())
                    .await
                    .map_err(|_| timed_out(&format!("reading snapshot L2 block {next}")))?
                    .wrap_err_with(|| format!("failed to read snapshot L2 block {next}"))?
                    .ok_or_else(|| eyre!("snapshot node has no L2 block {next}"))?
                    .map_header(|header| header.into_inner());
                if let AttributesMatch::Mismatch(mismatch) =
                    AttributesMatch::check(&config, &attributes, &block)
                {
                    bail!(
                        "attributes derived for L2 block {next} do not match the snapshot block: \
                         {mismatch:?}"
                    );
                }
                let derived_from = attributes
                    .derived_from
                    .ok_or_else(|| eyre!("attributes for L2 block {next} have no L1 source"))?;
                if fork.is_none_or(|fork| derived_from.number > fork.number) {
                    fork = Some(derived_from);
                }
                let block =
                    block.into_consensus().map_transactions(|tx| tx.inner.inner.into_inner());
                cursor = L2BlockInfo::from_block_and_genesis(&block, &genesis)
                    .wrap_err_with(|| format!("failed to decode snapshot L2 block {next}"))?;
                continue;
            }

            let step = timeout_at(deadline, pipeline.step(cursor)).await.map_err(|_| {
                timed_out(&format!(
                    "deriving L2 block {next} at L1 origin {origin} (last transient error: {})",
                    last_transient.as_deref().unwrap_or("none")
                ))
            })?;
            let (error, advancing_origin) = match step {
                StepResult::PreparedAttributes => continue,
                StepResult::AdvancedOrigin => {
                    origin = pipeline.origin().ok_or_eyre("derivation lost its L1 origin")?.number;
                    continue;
                }
                StepResult::OriginAdvanceErr(error) => (error, true),
                StepResult::StepFailed(error) => (error, false),
            };
            match error {
                PipelineErrorKind::Temporary(PipelineError::NotEnoughData) => {}
                // The confirmation-depth gate refuses every block past the limit.
                PipelineErrorKind::Temporary(_) if advancing_origin && origin >= l1_limit => bail!(
                    "no batch derives L2 block {next} through L1 block {l1_limit} (latest L1 \
                     origin {} plus sequencer window {}, upstream finalized {finalized})",
                    latest.l1_origin.number,
                    config.seq_window_size
                ),
                PipelineErrorKind::Temporary(error) => {
                    let error = self.source.redact(error);
                    timeout_at(deadline, sleep(Self::TRANSIENT_RETRY_DELAY)).await.map_err(
                        |_| {
                            timed_out(&format!(
                                "retrying derivation of L2 block {next} after: {error}"
                            ))
                        },
                    )?;
                    last_transient = Some(error);
                }
                // Crossing Holocene activation is a deterministic in-place transition, not a reset.
                PipelineErrorKind::Reset(ResetError::HoloceneActivation) => {
                    timeout_at(
                        deadline,
                        pipeline.signal(ActivationSignal { l2_safe_head: cursor }.signal()),
                    )
                    .await
                    .map_err(|_| timed_out("activating Holocene derivation"))?
                    .map_err(|error| {
                        eyre!(
                            "failed to activate Holocene derivation: {}",
                            self.source.redact(error)
                        )
                    })?;
                }
                error => bail!(
                    "derivation of L2 block {next} failed at L1 origin {origin}: {}",
                    self.source.redact(error)
                ),
            }
        }
        ensure!(
            cursor.block_info.hash == latest.block_info.hash,
            "snapshot L2 block {} changed during fork discovery",
            latest.block_info.number
        );

        let fork = fork.ok_or_eyre("fork discovery derived no attributes")?;
        let header = timeout_at(deadline, l1.inner.get_block_by_number(fork.number.into()))
            .await
            .map_err(|_| timed_out(&format!("reading upstream L1 block {}", fork.number)))?
            .map_err(|_| eyre!("failed to read upstream L1 block {}", fork.number))?
            .ok_or_else(|| eyre!("upstream L1 has no block {}", fork.number))?
            .header
            .into_consensus();
        let canonical =
            BlockInfo::new(header.hash_slow(), header.number, header.parent_hash, header.timestamp);
        ensure!(
            canonical.hash == fork.hash,
            "L1 block {} that derives the latest snapshot block is no longer canonical",
            fork.number
        );
        Ok(canonical)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use alloy_consensus::{
        Block, BlockBody, EMPTY_ROOT_HASH, Header, SignableTransaction, TxEip1559, TxEnvelope,
        transaction::Recovered,
    };
    use alloy_eips::BlockNumHash;
    use alloy_primitives::{Address, B256, TxKind};
    use alloy_rpc_types_eth::BlockTransactions;
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use axum::{Router, routing::get};
    use base_batcher_encoder::{
        BatchEncoder, BatchPipeline, DaType, EncoderConfig, FrameEncoder, SubmissionPayload,
    };
    use base_common_chains::L1_CONFIGS;
    use base_common_consensus::{BaseTxEnvelope, Predeploys};
    use base_common_genesis::{ChainGenesis, RollupConfig, SystemConfig, UpgradeConfig};
    use base_common_rpc_types::{BaseBlockResponse, BaseHeaderResponse, Transaction};
    use base_consensus_providers::{APIConfigResponse, APIGenesisResponse};
    use base_protocol::{BlockInfo, L1BlockInfoTx};
    use jsonrpsee::{
        RpcModule,
        server::{ServerBuilder, ServerHandle},
        types::ErrorObjectOwned,
    };
    use serde_json::Value;
    use tokio::{net::TcpListener, task::JoinHandle};
    use url::Url;

    use super::{SnapshotForkFinder, SnapshotForkSource};
    use crate::SnapshotInspection;

    const L1_CHAIN_ID: u64 = 1;
    const L2_CHAIN_ID: u64 = 8453;
    /// Post-Prague mainnet time, so the L1 config selects a real blob fee schedule.
    const L1_GENESIS_TIME: u64 = 1_750_000_000;
    /// L1 and L2 both produce a block every two seconds in the fixture.
    const BLOCK_TIME: u64 = 2;
    const L1_BLOCKS: u64 = 31;
    const GAS_LIMIT: u64 = 30_000_000;
    const SEQ_WINDOW_SIZE: u64 = 10;
    const BATCH_INBOX: Address = Address::repeat_byte(0x1b);
    /// L2 block `n` has L1 origin `n + 1`, so the latest block's origin is L1 block 10.
    const LATEST: u64 = 9;
    /// L1 block carrying the latest block's batch, five blocks after its origin.
    const BATCH_L1_BLOCK: u64 = 15;

    /// A Fjord-era chain whose L2 blocks are exactly what the derivation pipeline produces.
    struct Fixture {
        rollup_config: RollupConfig,
        l1: Vec<Block<TxEnvelope>>,
        l2: Vec<Block<BaseTxEnvelope>>,
        /// Finalized, safe, and latest L2 block numbers.
        labels: [u64; 3],
        l1_chain_id: u64,
        l1_finalized: u64,
        /// L2 blocks served without bodies; genesis is always pruned.
        pruned_l2: Vec<u64>,
        /// Upstream L1 receipt requests that fail before succeeding.
        failing_receipts: usize,
        stall_receipts: bool,
    }

    impl Fixture {
        fn new() -> Self {
            let batcher = Self::batcher();
            let mut l1 = Vec::new();
            for number in 0..BATCH_L1_BLOCK {
                l1.push(Self::l1_block(l1.last(), number, Vec::new()));
            }

            let l2_genesis = Block::<BaseTxEnvelope> {
                header: Header {
                    timestamp: l1[1].header.timestamp,
                    gas_limit: GAS_LIMIT,
                    ..Default::default()
                },
                body: BlockBody::default(),
            };
            let system_config = SystemConfig {
                batcher_address: batcher.address(),
                gas_limit: GAS_LIMIT,
                ..Default::default()
            };
            let rollup_config = RollupConfig {
                genesis: ChainGenesis {
                    l1: BlockNumHash { number: 1, hash: l1[1].header.hash_slow() },
                    l2: BlockNumHash { number: 0, hash: l2_genesis.header.hash_slow() },
                    l2_time: l2_genesis.header.timestamp,
                    system_config: Some(system_config),
                },
                block_time: BLOCK_TIME,
                max_sequencer_drift: 600,
                seq_window_size: SEQ_WINDOW_SIZE,
                channel_timeout: 4,
                l1_chain_id: L1_CHAIN_ID,
                l2_chain_id: L2_CHAIN_ID.into(),
                batch_inbox_address: BATCH_INBOX,
                upgrades: UpgradeConfig {
                    regolith_time: Some(0),
                    canyon_time: Some(0),
                    delta_time: Some(0),
                    ecotone_time: Some(0),
                    fjord_time: Some(0),
                    ..Default::default()
                },
                ..Default::default()
            };

            let mut l2 = vec![l2_genesis];
            for number in 1..=LATEST {
                let parent = &l2[number as usize - 1].header;
                let origin = &l1[number as usize + 1].header;
                let timestamp = parent.timestamp + BLOCK_TIME;
                let (_, l1_info) = L1BlockInfoTx::try_new_with_deposit_tx(
                    &rollup_config,
                    &L1_CONFIGS[&L1_CHAIN_ID],
                    &system_config,
                    0,
                    origin,
                    parent.timestamp,
                    timestamp,
                )
                .unwrap();
                let block = Block {
                    header: Header {
                        parent_hash: parent.hash_slow(),
                        number,
                        timestamp,
                        gas_limit: GAS_LIMIT,
                        beneficiary: Predeploys::SEQUENCER_FEE_VAULT,
                        mix_hash: origin.mix_hash,
                        base_fee_per_gas: Some(1),
                        withdrawals_root: Some(EMPTY_ROOT_HASH),
                        parent_beacon_block_root: origin.parent_beacon_block_root,
                        ..Default::default()
                    },
                    body: BlockBody {
                        transactions: vec![BaseTxEnvelope::Deposit(l1_info)],
                        ommers: Vec::new(),
                        withdrawals: Some(Default::default()),
                    },
                };
                l2.push(block);
            }

            let batch = Self::batch_transactions(&rollup_config, &batcher, &l2[LATEST as usize]);
            l1.push(Self::l1_block(l1.last(), BATCH_L1_BLOCK, batch));
            for number in BATCH_L1_BLOCK + 1..L1_BLOCKS {
                l1.push(Self::l1_block(l1.last(), number, Vec::new()));
            }

            Self {
                rollup_config,
                l1,
                l2,
                labels: [7, 8, LATEST],
                l1_chain_id: L1_CHAIN_ID,
                l1_finalized: L1_BLOCKS - 1,
                pruned_l2: vec![0],
                failing_receipts: 0,
                stall_receipts: false,
            }
        }

        fn batcher() -> PrivateKeySigner {
            PrivateKeySigner::from_bytes(&B256::repeat_byte(0x11)).unwrap()
        }

        fn l1_block(
            parent: Option<&Block<TxEnvelope>>,
            number: u64,
            transactions: Vec<TxEnvelope>,
        ) -> Block<TxEnvelope> {
            Block {
                header: Header {
                    parent_hash: parent.map_or(B256::ZERO, |parent| parent.header.hash_slow()),
                    number,
                    timestamp: L1_GENESIS_TIME + BLOCK_TIME * number,
                    gas_limit: GAS_LIMIT,
                    mix_hash: B256::with_last_byte(number as u8 + 1),
                    base_fee_per_gas: Some(7),
                    withdrawals_root: Some(EMPTY_ROOT_HASH),
                    blob_gas_used: Some(0),
                    excess_blob_gas: Some(0),
                    parent_beacon_block_root: Some(B256::repeat_byte(number as u8 + 1)),
                    ..Default::default()
                },
                body: BlockBody {
                    transactions,
                    ommers: Vec::new(),
                    withdrawals: Some(Default::default()),
                },
            }
        }

        /// Encodes `block` with the production batcher encoder into signed calldata transactions.
        fn batch_transactions(
            rollup_config: &RollupConfig,
            batcher: &PrivateKeySigner,
            block: &Block<BaseTxEnvelope>,
        ) -> Vec<TxEnvelope> {
            let mut encoder = BatchEncoder::new(
                Arc::new(rollup_config.clone()),
                EncoderConfig { da_type: DaType::Calldata, ..Default::default() },
            )
            .unwrap();
            encoder.add_block(block.clone()).unwrap();
            encoder
                .encode_and_drain()
                .unwrap()
                .iter()
                .enumerate()
                .map(|(nonce, submission)| {
                    let SubmissionPayload::Calldata(frame) = submission.payload() else {
                        panic!("expected a calldata submission")
                    };
                    let tx = TxEip1559 {
                        chain_id: L1_CHAIN_ID,
                        nonce: nonce as u64,
                        gas_limit: 1_000_000,
                        max_fee_per_gas: 10,
                        max_priority_fee_per_gas: 1,
                        to: TxKind::Call(BATCH_INBOX),
                        input: FrameEncoder::to_calldata(frame),
                        ..Default::default()
                    };
                    let signature = batcher.sign_hash_sync(&tx.signature_hash()).unwrap();
                    TxEnvelope::Eip1559(tx.into_signed(signature))
                })
                .collect()
        }

        fn l1_hash(&self, number: u64) -> B256 {
            self.l1[number as usize].header.hash_slow()
        }

        async fn find(self, timeout: Duration) -> eyre::Result<BlockInfo> {
            let (l2_url, l2_handle) = self.serve_l2().await;
            let (l1_url, l1_handle) = self.serve_l1().await;
            let (beacon_url, beacon_task) = serve_beacon().await;
            let inspection = SnapshotInspection::read(
                l2_url.clone(),
                Arc::new(self.rollup_config.clone()),
                L2_CHAIN_ID,
            )
            .await
            .expect("snapshot without a genesis body is inspectable");
            let finder = SnapshotForkFinder {
                rpc_url: l2_url,
                source: SnapshotForkSource { execution: l1_url, beacon: beacon_url },
                timeout,
            };
            let result = tokio::time::timeout(timeout * 2, finder.find(&inspection))
                .await
                .expect("discovery honors its own deadline");
            l2_handle.stop().unwrap();
            l1_handle.stop().unwrap();
            beacon_task.abort();
            result
        }

        async fn serve_l2(&self) -> (Url, ServerHandle) {
            let mut by_tag = HashMap::new();
            let mut by_hash = HashMap::new();
            for block in &self.l2 {
                let number = block.header.number;
                if number == 0 {
                    continue;
                }
                let mut block = block.clone();
                if self.pruned_l2.contains(&number) {
                    block.body.transactions.clear();
                }
                let json = l2_rpc_block(block.clone());
                by_hash.insert(block.header.hash_slow(), json.clone());
                by_tag.insert(format!("{number:#x}"), json);
            }
            for (label, number) in ["finalized", "safe", "latest"].into_iter().zip(self.labels) {
                by_tag.insert(label.to_string(), by_tag[&format!("{number:#x}")].clone());
            }

            let mut module = RpcModule::new((by_tag, by_hash));
            module
                .register_method("eth_chainId", |_, _, _| {
                    Ok::<_, ErrorObjectOwned>(format!("{L2_CHAIN_ID:#x}"))
                })
                .unwrap();
            module
                .register_method("eth_getBlockByNumber", |params, blocks, _| {
                    let (tag, _full): (String, bool) = params.parse()?;
                    Ok::<_, ErrorObjectOwned>(blocks.0.get(&tag).cloned().unwrap_or(Value::Null))
                })
                .unwrap();
            module
                .register_method("eth_getBlockByHash", |params, blocks, _| {
                    let (hash, _full): (B256, bool) = params.parse()?;
                    Ok::<_, ErrorObjectOwned>(blocks.1.get(&hash).cloned().unwrap_or(Value::Null))
                })
                .unwrap();
            serve(module).await
        }

        async fn serve_l1(&self) -> (Url, ServerHandle) {
            let blocks: Vec<Value> = self.l1.iter().map(l1_rpc_block).collect();
            let by_hash: HashMap<B256, Value> = self
                .l1
                .iter()
                .zip(&blocks)
                .map(|(b, json)| (b.header.hash_slow(), json.clone()))
                .collect();
            let node = L1Node {
                chain_id: self.l1_chain_id,
                finalized: blocks[self.l1_finalized as usize].clone(),
                blocks,
                by_hash,
                failing_receipts: AtomicUsize::new(self.failing_receipts),
                stall_receipts: self.stall_receipts,
            };

            let mut module = RpcModule::new(node);
            module
                .register_method("eth_chainId", |_, node, _| {
                    Ok::<_, ErrorObjectOwned>(format!("{:#x}", node.chain_id))
                })
                .unwrap();
            module
                .register_method("eth_getBlockByNumber", |params, node, _| {
                    let (tag, _full): (String, bool) = params.parse()?;
                    let block = if tag == "finalized" {
                        Some(node.finalized.clone())
                    } else {
                        u64::from_str_radix(tag.trim_start_matches("0x"), 16)
                            .ok()
                            .and_then(|number| node.blocks.get(number as usize).cloned())
                    };
                    Ok::<_, ErrorObjectOwned>(block.unwrap_or(Value::Null))
                })
                .unwrap();
            module
                .register_method("eth_getBlockByHash", |params, node, _| {
                    let (hash, _full): (B256, bool) = params.parse()?;
                    Ok::<_, ErrorObjectOwned>(
                        node.by_hash.get(&hash).cloned().unwrap_or(Value::Null),
                    )
                })
                .unwrap();
            module
                .register_async_method("eth_getBlockReceipts", |params, node, _| async move {
                    let (id,): (Value,) = params.parse()?;
                    if node.stall_receipts {
                        tokio::time::sleep(Duration::from_secs(3600)).await;
                    }
                    if node
                        .failing_receipts
                        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                        .is_ok()
                    {
                        return Err(ErrorObjectOwned::owned(
                            -32000,
                            "temporarily unavailable",
                            None::<()>,
                        ));
                    }
                    let hash = id.as_str().or_else(|| id["blockHash"].as_str()).unwrap_or_default();
                    let known =
                        hash.parse().is_ok_and(|hash: B256| node.by_hash.contains_key(&hash));
                    Ok(if known { Value::Array(Vec::new()) } else { Value::Null })
                })
                .unwrap();
            serve(module).await
        }
    }

    struct L1Node {
        chain_id: u64,
        blocks: Vec<Value>,
        by_hash: HashMap<B256, Value>,
        finalized: Value,
        failing_receipts: AtomicUsize,
        stall_receipts: bool,
    }

    async fn serve<T: Send + Sync + 'static>(module: RpcModule<T>) -> (Url, ServerHandle) {
        let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        (format!("http://{address}").parse().unwrap(), server.start(module))
    }

    async fn serve_beacon() -> (Url, JoinHandle<()>) {
        let genesis = serde_json::to_string(&APIGenesisResponse::new(L1_GENESIS_TIME)).unwrap();
        let spec = serde_json::to_string(&APIConfigResponse::new(12)).unwrap();
        let app = Router::new()
            .route("/eth/v1/beacon/genesis", get(|| async move { genesis }))
            .route("/eth/v1/config/spec", get(|| async move { spec }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap()).parse().unwrap();
        (url, tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }))
    }

    fn l1_rpc_block(block: &Block<TxEnvelope>) -> Value {
        let hash = block.header.hash_slow();
        let transactions = block
            .body
            .transactions
            .iter()
            .enumerate()
            .map(|(index, transaction)| alloy_rpc_types_eth::Transaction {
                inner: Recovered::new_unchecked(transaction.clone(), Fixture::batcher().address()),
                block_hash: Some(hash),
                block_number: Some(block.header.number),
                block_timestamp: Some(block.header.timestamp),
                transaction_index: Some(index as u64),
                effective_gas_price: Some(7),
            })
            .collect();
        serde_json::to_value(alloy_rpc_types_eth::Block {
            header: alloy_rpc_types_eth::Header {
                hash,
                inner: block.header.clone(),
                total_difficulty: None,
                size: None,
            },
            uncles: Vec::new(),
            transactions: BlockTransactions::Full(transactions),
            withdrawals: block.body.withdrawals.clone(),
        })
        .unwrap()
    }

    fn l2_rpc_block(block: Block<BaseTxEnvelope>) -> Value {
        let hash = block.header.hash_slow();
        let number = block.header.number;
        let timestamp = block.header.timestamp;
        let transactions = block
            .body
            .transactions
            .into_iter()
            .enumerate()
            .map(|(index, transaction)| Transaction {
                inner: alloy_rpc_types_eth::Transaction {
                    inner: Recovered::new_unchecked(transaction, Address::ZERO),
                    block_hash: Some(hash),
                    block_number: Some(number),
                    block_timestamp: Some(timestamp),
                    transaction_index: Some(index as u64),
                    effective_gas_price: Some(0),
                },
                block_timestamp_ms: None,
                deposit_nonce: None,
                deposit_receipt_version: None,
            })
            .collect();
        serde_json::to_value(BaseBlockResponse {
            header: BaseHeaderResponse::new(alloy_rpc_types_eth::Header {
                hash,
                inner: block.header,
                total_difficulty: None,
                size: None,
            }),
            uncles: Vec::new(),
            transactions: BlockTransactions::Full(transactions),
            withdrawals: block.body.withdrawals,
        })
        .unwrap()
    }

    const TIMEOUT: Duration = Duration::from_secs(20);

    #[tokio::test]
    async fn selects_inclusion_block_rather_than_l1_origin() {
        let fixture = Fixture::new();
        assert_eq!(fixture.l2[LATEST as usize].header.mix_hash, fixture.l1[10].header.mix_hash);
        let expected = BlockInfo::new(
            fixture.l1_hash(BATCH_L1_BLOCK),
            BATCH_L1_BLOCK,
            fixture.l1_hash(BATCH_L1_BLOCK - 1),
            fixture.l1[BATCH_L1_BLOCK as usize].header.timestamp,
        );

        let fork = fixture.find(TIMEOUT).await.unwrap();
        assert_eq!(fork, expected);
        let json = serde_json::to_value(fork).unwrap();
        assert_eq!(json["number"], BATCH_L1_BLOCK);
        assert_eq!(json["parentHash"], serde_json::to_value(expected.parent_hash).unwrap());
    }

    #[tokio::test]
    async fn locates_latest_batch_from_parent_when_safe_is_latest() {
        let mut fixture = Fixture::new();
        fixture.labels = [7, LATEST, LATEST];
        let expected = fixture.l1_hash(BATCH_L1_BLOCK);

        let fork = fixture.find(TIMEOUT).await.unwrap();
        assert_eq!((fork.number, fork.hash), (BATCH_L1_BLOCK, expected));
    }

    #[tokio::test]
    async fn retries_transient_upstream_errors() {
        let mut fixture = Fixture::new();
        fixture.failing_receipts = 2;

        let fork = fixture.find(TIMEOUT).await.unwrap();
        assert_eq!(fork.number, BATCH_L1_BLOCK);
    }

    #[tokio::test]
    async fn refuses_attributes_that_do_not_match_the_snapshot() {
        let mut fixture = Fixture::new();
        fixture.l2[LATEST as usize].header.beneficiary = Address::repeat_byte(0xfe);

        let error = fixture.find(TIMEOUT).await.unwrap_err().to_string();
        assert!(error.contains("do not match the snapshot block"), "{error}");
        assert!(error.contains("FeeRecipient"), "{error}");
    }

    #[tokio::test]
    async fn fails_when_batch_lies_beyond_upstream_finalized() {
        let mut fixture = Fixture::new();
        fixture.l1_finalized = BATCH_L1_BLOCK - 1;

        let error = fixture.find(TIMEOUT).await.unwrap_err().to_string();
        assert!(
            error.contains(&format!("no batch derives L2 block {LATEST} through L1 block 14")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn rejects_wrong_upstream_l1_chain() {
        let mut fixture = Fixture::new();
        fixture.l1_chain_id = 11_155_111;

        let error = fixture.find(TIMEOUT).await.unwrap_err().to_string();
        assert!(error.contains("does not match rollup L1 chain ID 1"), "{error}");
    }

    #[tokio::test]
    async fn rejects_safe_latest_tail_rooted_at_genesis() {
        let mut fixture = Fixture::new();
        fixture.labels = [1, 1, 1];

        let error = fixture.find(TIMEOUT).await.unwrap_err().to_string();
        assert!(error.contains("its parent is the rollup genesis"), "{error}");
    }

    #[tokio::test]
    async fn times_out_when_lookback_body_is_pruned() {
        let mut fixture = Fixture::new();
        fixture.pruned_l2.push(5);

        let error = fixture.find(Duration::from_secs(2)).await.unwrap_err().to_string();
        assert!(error.contains("timed out"), "{error}");
        assert!(error.contains("L2 block info construction failed"), "{error}");
    }

    #[tokio::test]
    async fn times_out_on_stalled_upstream_without_echoing_it() {
        let mut fixture = Fixture::new();
        fixture.stall_receipts = true;

        let error = fixture.find(Duration::from_secs(1)).await.unwrap_err();
        let report = format!("{error:?}");
        assert!(report.contains("timed out"), "{report}");
        assert!(!report.contains("127.0.0.1"), "{report}");
    }

    #[test]
    fn redacts_provider_errors_that_could_echo_upstream_urls() {
        let source = SnapshotForkSource {
            execution: "https://user:secret@l1.example/key".parse().unwrap(),
            beacon: "https://beacon.example/token".parse().unwrap(),
        };
        assert_eq!(
            source.redact("Transport error: error sending request for url (https://x/)"),
            SnapshotForkSource::REDACTED_ERROR
        );
        assert_eq!(
            source.redact("dns error: failed to lookup beacon.example"),
            SnapshotForkSource::REDACTED_ERROR
        );
        assert_eq!(source.redact("Not enough data"), "Not enough data");
        assert_eq!(format!("{source:?}"), "SnapshotForkSource { .. }");
    }
}
