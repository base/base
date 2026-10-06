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
    ActivationSignal, ChainProvider, OriginProvider, Pipeline, PipelineError, PipelineErrorKind,
    ResetError, ResetSignal, SignalReceiver, StepResult,
};
use base_consensus_engine::AttributesMatch;
use base_consensus_providers::{
    AlloyChainProvider, AlloyL2ChainProvider, BeaconClient, OnlineBeaconClient, OnlineBlobProvider,
    OnlinePipeline,
};
use base_protocol::{BatchValidationProvider, BlockInfo, L2BlockInfo};
use eyre::{OptionExt, Result, WrapErr, bail, ensure, eyre};
use tokio::{
    task::yield_now,
    time::{Instant, sleep, timeout_at},
};
use tracing::debug;
use url::Url;

use super::SnapshotInspection;

/// Upstream L1 execution and Beacon endpoints used for fork discovery.
///
/// Their URLs may embed credentials, including in query strings, so neither they nor provider
/// errors that could echo them appear in [`fmt::Debug`] output or errors.
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
}

/// Upstream L1 metadata that bounds fork discovery for one snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotForkMetadata {
    /// Upstream finalized L1 block number.
    pub finalized: u64,
    /// Last L1 block discovery may read: the earlier of [`Self::finalized`] and the latest
    /// snapshot block's L1 origin plus the sequencer window, after which its batch is invalid.
    pub l1_limit: u64,
    /// Beacon genesis time, in seconds since the Unix epoch.
    pub genesis_time: u64,
    /// Beacon slot duration, in seconds.
    pub slot_interval: u64,
}

impl SnapshotForkMetadata {
    /// Reads and validates `source`'s metadata for `inspection`, giving up at `deadline`.
    ///
    /// Takes a deadline rather than a duration so a whole discovery shares one budget. Fails when
    /// the execution chain ID is not the rollup's L1 chain ID, when upstream finality is missing or
    /// behind the latest snapshot block's L1 origin, or when Beacon metadata is unreadable or has
    /// a zero slot duration. Errors never contain upstream URLs.
    pub async fn read(
        source: &SnapshotForkSource,
        inspection: &SnapshotInspection,
        deadline: Instant,
    ) -> Result<Self> {
        let mut stage = "reading the upstream L1 chain ID";
        let read = async {
            let config = &inspection.rollup_config;
            let latest_origin = inspection.latest.block_info.l1_origin.number;
            let l1: RootProvider = RootProvider::new_http(source.execution.clone());
            let chain_id = l1
                .get_chain_id()
                .await
                .map_err(|_| eyre!("failed to read the upstream L1 chain ID"))?;
            ensure!(
                chain_id == config.l1_chain_id,
                "upstream L1 chain ID {chain_id} does not match rollup L1 chain ID {}",
                config.l1_chain_id
            );

            stage = "reading the upstream finalized L1 block";
            let finalized = l1
                .get_block_by_number(BlockNumberOrTag::Finalized)
                .await
                .map_err(|_| eyre!("failed to read the upstream finalized L1 block"))?
                .ok_or_eyre("upstream L1 has no finalized block")?
                .header
                .number;
            ensure!(
                finalized >= latest_origin,
                "upstream finalized L1 block {finalized} is behind latest snapshot L1 origin \
                 {latest_origin}"
            );

            let beacon = OnlineBeaconClient::new_http(source.beacon.to_string());
            stage = "reading upstream Beacon genesis";
            let genesis_time = beacon
                .genesis_time()
                .await
                .map_err(|_| eyre!("failed to read upstream Beacon genesis time"))?
                .data
                .genesis_time;
            stage = "reading upstream Beacon slot duration";
            let slot_interval = beacon
                .slot_interval()
                .await
                .map_err(|_| eyre!("failed to read upstream Beacon slot duration"))?
                .data
                .seconds_per_slot;
            ensure!(slot_interval > 0, "upstream Beacon slot duration must be positive");

            Ok(Self {
                finalized,
                l1_limit: latest_origin.saturating_add(config.seq_window_size).min(finalized),
                genesis_time,
                slot_interval,
            })
        };
        timeout_at(deadline, read).await.map_err(|_| eyre!("timed out {stage}"))?
    }
}

/// Finds the L1 block whose derivation covers a snapshot's unsafe tail with the production
/// [`OnlinePipeline`].
///
/// The snapshot safe head is trusted. Discovery resets the pipeline there, or at latest's parent
/// when safe equals latest, derives payload attributes only for the blocks after that reset point
/// through latest, and requires each to match the snapshot block with
/// [`AttributesMatch::check`]; nothing is executed or written. The result proves the unsafe tail's
/// provenance, not the validity of all history.
#[derive(Debug, Clone)]
pub struct SnapshotForkFinder {
    /// Execution JSON-RPC URL of the snapshot node serving the L2 blocks.
    pub rpc_url: Url,
    /// Upstream L1 endpoints.
    pub source: SnapshotForkSource,
    /// Budget for the whole discovery, including upstream metadata reads.
    pub timeout: Duration,
}

impl SnapshotForkFinder {
    /// L1 and L2 provider cache size for one bounded discovery run.
    pub const PROVIDER_CACHE_SIZE: usize = 1024;
    /// Confirmation depth that turns the pipeline's L1 head into an inclusive read limit of
    /// `head - 1`.
    pub const L1_LIMIT_CONFIRMATIONS: u64 = 1;
    /// Delay before retrying after a temporary reset or derivation error.
    pub const TRANSIENT_RETRY_DELAY: Duration = Duration::from_millis(500);
    /// Provider errors labeled temporary that recur for the same block data, such as a pruned body.
    ///
    /// Providers erase their typed errors into [`PipelineError::Provider`] text, so these exact
    /// messages are the only way to tell them from transport failures.
    pub const DETERMINISTIC_PROVIDER_ERRORS: [&'static str; 3] = [
        "L2 block info construction failed",
        "system config conversion failed",
        "Failed to convert RPC receipts into consensus receipts",
    ];
    /// Stands in for provider-supplied error text, which can echo an upstream URL, credential or
    /// response body.
    pub const PROVIDER_ERROR: &'static str = "upstream provider request failed";

    /// Describes a derivation error by its kind and typed cause, replacing provider-supplied text
    /// other than [`Self::DETERMINISTIC_PROVIDER_ERRORS`] with [`Self::PROVIDER_ERROR`]. That text
    /// is logged only at debug level.
    ///
    /// Providers erase their errors into [`PipelineError::Provider`]; every other variant is typed
    /// and built from chain data.
    pub fn diagnostic(error: &PipelineErrorKind) -> String {
        let kind = match error {
            PipelineErrorKind::Temporary(PipelineError::Provider(text))
                if !Self::DETERMINISTIC_PROVIDER_ERRORS.contains(&text.as_str()) =>
            {
                debug!(error = %text, "temporary provider error withheld from diagnostics");
                "Temporary"
            }
            PipelineErrorKind::Critical(PipelineError::Provider(text)) => {
                debug!(error = %text, "critical provider error withheld from diagnostics");
                "Critical"
            }
            error => return error.to_string(),
        };
        format!("{kind} error: {}", Self::PROVIDER_ERROR)
    }

    /// Returns whether `error` is temporary and not one of
    /// [`Self::DETERMINISTIC_PROVIDER_ERRORS`], so retrying it may succeed.
    pub fn is_retryable(error: &PipelineErrorKind) -> bool {
        match error {
            PipelineErrorKind::Temporary(PipelineError::Provider(text)) => {
                !Self::DETERMINISTIC_PROVIDER_ERRORS.contains(&text.as_str())
            }
            PipelineErrorKind::Temporary(_) => true,
            _ => false,
        }
    }

    /// Returns the canonical, finalized L1 block at which the latest snapshot block's batch is
    /// derived: the maximum L1 source among the attributes derived for the unsafe tail.
    ///
    /// When safe equals latest, discovery starts from latest's parent so it still locates the
    /// latest batch; that parent must retain its body and cannot be the rollup genesis. L1 reads
    /// stop at [`SnapshotForkMetadata::l1_limit`]. Retryable reset and derivation errors (see
    /// [`Self::is_retryable`]) are retried every [`Self::TRANSIENT_RETRY_DELAY`] until the deadline,
    /// which then reports the last one. Mismatching attributes, other pipeline errors, an exhausted
    /// L1 range, a snapshot latest block or L1 fork block that is no longer canonical, and the
    /// deadline are fatal. Errors contain no provider-supplied text beyond the fixed messages
    /// [`Self::diagnostic`] keeps.
    pub async fn find(&self, inspection: &SnapshotInspection) -> Result<BlockInfo> {
        let deadline = Instant::now() + self.timeout;
        let latest = inspection.latest.block_info;
        let from_parent = inspection.safe.block_info == latest;
        ensure!(
            !from_parent || latest.block_info.number > inspection.genesis.l2.number + 1,
            "cannot locate the batch for L2 block {}: its parent is the rollup genesis, which has \
             no L1-info deposit",
            latest.block_info.number
        );
        let metadata = SnapshotForkMetadata::read(&self.source, inspection, deadline).await?;

        let mut stage = String::new();
        let mut last_transient = None;
        let discovery = async {
            let config = Arc::new(inspection.rollup_config.clone());
            let genesis = config.genesis;
            let l1_config = L1_CONFIGS.get(&config.l1_chain_id).cloned().ok_or_else(|| {
                eyre!("no built-in L1 chain config for L1 chain ID {}", config.l1_chain_id)
            })?;
            let mut l1 = AlloyChainProvider::new_http(
                self.source.execution.clone(),
                Self::PROVIDER_CACHE_SIZE,
            );
            let snapshot = RootProvider::<Base>::new_http(self.rpc_url.clone());
            let mut l2 = AlloyL2ChainProvider::new(
                snapshot.clone(),
                Arc::clone(&config),
                Self::PROVIDER_CACHE_SIZE,
            );
            let start = if from_parent {
                stage = "reading the latest block's parent".into();
                l2.l2_block_info_by_hash(latest.block_info.parent_hash).await.wrap_err_with(
                    || {
                        format!(
                            "snapshot parent of L2 block {} is missing or pruned",
                            latest.block_info.number
                        )
                    },
                )?
            } else {
                inspection.safe.block_info
            };
            let blobs = OnlineBlobProvider {
                beacon_client: OnlineBeaconClient::new_http(self.source.beacon.to_string()),
                genesis_time: metadata.genesis_time,
                slot_interval: metadata.slot_interval,
            };
            let mut pipeline = OnlinePipeline::new_polled(
                Arc::clone(&config),
                Arc::new(l1_config),
                blobs,
                l1.clone(),
                l2,
                Arc::new(AtomicU64::new(metadata.l1_limit + 1)),
                Self::L1_LIMIT_CONFIRMATIONS,
            );
            loop {
                stage = format!("resetting derivation at L2 block {}", start.block_info.number);
                match pipeline.signal(ResetSignal { l2_safe_head: start }.signal()).await {
                    Ok(()) => break,
                    Err(error) if Self::is_retryable(&error) => {
                        stage = "retrying the derivation reset".into();
                        last_transient = Some(Self::diagnostic(&error));
                        sleep(Self::TRANSIENT_RETRY_DELAY).await;
                    }
                    Err(error) => bail!(
                        "failed to reset derivation at L2 block {}: {}",
                        start.block_info.number,
                        Self::diagnostic(&error)
                    ),
                }
            }

            let mut cursor = start;
            let mut origin = pipeline.origin().map_or(0, |origin| origin.number);
            let mut fork: Option<BlockInfo> = None;
            while cursor.block_info.number < latest.block_info.number {
                let next = cursor.block_info.number + 1;
                stage = format!("deriving L2 block {next} at L1 origin {origin}");
                // Cached pipeline steps may complete without I/O; yield so the deadline still fires.
                yield_now().await;

                if let Some(attributes) = pipeline.next() {
                    stage = format!("reading snapshot L2 block {next}");
                    let block = snapshot
                        .get_block_by_number(next.into())
                        .full()
                        .await
                        .wrap_err_with(|| format!("failed to read snapshot L2 block {next}"))?
                        .ok_or_else(|| eyre!("snapshot node has no L2 block {next}"))?
                        .map_header(|header| header.into_inner());
                    if let AttributesMatch::Mismatch(mismatch) =
                        AttributesMatch::check(&config, &attributes, &block)
                    {
                        bail!(
                            "attributes derived for L2 block {next} do not match the snapshot \
                             block: {mismatch:?}"
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

                let (error, advancing_origin) = match pipeline.step(cursor).await {
                    StepResult::PreparedAttributes => continue,
                    StepResult::AdvancedOrigin => {
                        origin =
                            pipeline.origin().ok_or_eyre("derivation lost its L1 origin")?.number;
                        continue;
                    }
                    StepResult::OriginAdvanceErr(error) => (error, true),
                    StepResult::StepFailed(error) => (error, false),
                };
                match error {
                    PipelineErrorKind::Temporary(PipelineError::NotEnoughData) => {}
                    // The confirmation-depth gate refuses every block past the limit.
                    PipelineErrorKind::Temporary(_)
                        if advancing_origin && origin >= metadata.l1_limit =>
                    {
                        bail!(
                            "no batch derives L2 block {next} through L1 block {} (latest L1 \
                             origin {} plus sequencer window {}, upstream finalized {})",
                            metadata.l1_limit,
                            latest.l1_origin.number,
                            config.seq_window_size,
                            metadata.finalized
                        )
                    }
                    error if Self::is_retryable(&error) => {
                        stage = format!("retrying derivation of L2 block {next}");
                        last_transient = Some(Self::diagnostic(&error));
                        sleep(Self::TRANSIENT_RETRY_DELAY).await;
                    }
                    // Crossing Holocene activation is a deterministic in-place transition, not a
                    // reset.
                    PipelineErrorKind::Reset(ResetError::HoloceneActivation) => {
                        stage = "activating Holocene derivation".into();
                        pipeline
                            .signal(ActivationSignal { l2_safe_head: cursor }.signal())
                            .await
                            .map_err(|error| {
                            eyre!(
                                "failed to activate Holocene derivation: {}",
                                Self::diagnostic(&error)
                            )
                        })?;
                        // Activation can leave the pipeline at a later origin than the last one
                        // reported, which the exhausted-range check must see.
                        origin =
                            pipeline.origin().ok_or_eyre("derivation lost its L1 origin")?.number;
                    }
                    error => bail!(
                        "derivation of L2 block {next} failed at L1 origin {origin}: {}",
                        Self::diagnostic(&error)
                    ),
                }
            }
            ensure!(
                cursor.block_info.hash == latest.block_info.hash,
                "snapshot L2 block {} changed during fork discovery",
                latest.block_info.number
            );

            let fork = fork.ok_or_eyre("fork discovery derived no attributes")?;
            stage = format!("reading upstream L1 block {}", fork.number);
            let canonical = l1
                .block_info_by_number(fork.number)
                .await
                .map_err(|_| eyre!("failed to read upstream L1 block {}", fork.number))?;
            ensure!(
                canonical.hash == fork.hash,
                "L1 block {} that derives the latest snapshot block is no longer canonical",
                fork.number
            );
            Ok(canonical)
        };
        timeout_at(deadline, discovery).await.map_err(|_| {
            eyre!(
                "fork discovery timed out after {:?} {stage} (last transient error: {})",
                self.timeout,
                last_transient.as_deref().unwrap_or("none")
            )
        })?
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
    use axum::{
        Router,
        extract::{RawQuery, State},
        http::StatusCode,
        routing::{get, post},
    };
    use base_batcher_encoder::{
        BatchEncoder, BatchPipeline, DaType, EncoderConfig, FrameEncoder, SubmissionPayload,
    };
    use base_common_chains::L1_CONFIGS;
    use base_common_consensus::{BaseTxEnvelope, Predeploys};
    use base_common_genesis::{ChainGenesis, RollupConfig, SystemConfig, UpgradeConfig};
    use base_consensus_derive::{PipelineError, ResetError};
    use base_protocol::{BlockInfo, L1BlockInfoTx};
    use serde_json::{Value, json};
    use tokio::{
        net::TcpListener,
        task::JoinHandle,
        time::{Instant, sleep},
    };

    use super::{SnapshotForkFinder, SnapshotForkMetadata, SnapshotForkSource};
    use crate::{SnapshotInspection, test_utils::SnapshotRpcFixture};

    /// Query-string credential every upstream request must carry.
    const CREDENTIAL: &str = "key=secret";
    const L1_CHAIN_ID: u64 = 1;
    /// Latest L1 origin of [`SnapshotRpcFixture`]'s default chain.
    const LATEST_ORIGIN: u64 = 11;
    const SEQ_WINDOW_SIZE: u64 = 10;
    /// Beacon genesis time.
    const GENESIS_TIME: u64 = 1_606_824_023;
    const DEADLINE: Duration = Duration::from_secs(10);
    /// Post-Prague mainnet time of L1 block 0, so the L1 config selects a real blob fee schedule.
    const L1_GENESIS_TIME: u64 = 1_750_000_000;
    /// L1 and L2 both produce a block every two seconds in [`Chain`].
    const BLOCK_TIME: u64 = 2;
    const L1_BLOCKS: u64 = 31;
    const GAS_LIMIT: u64 = 30_000_000;
    const BATCH_INBOX: Address = Address::repeat_byte(0x1b);
    /// L2 block `n` of [`Chain`] has L1 origin `n + 1`, so the latest block's origin is L1 block 10.
    const LATEST: u64 = 9;
    /// L1 block carrying the latest block's batch, five blocks after its origin.
    const BATCH_L1_BLOCK: u64 = 15;
    /// Finalized, safe, and latest L2 block numbers of a snapshot with an unsafe tail.
    const LABELS: [u64; 3] = [7, 8, LATEST];

    /// Upstream L1 execution JSON-RPC and Beacon API, served from one HTTP server.
    #[derive(Clone)]
    struct Upstream {
        chain_id: u64,
        finalized: Option<u64>,
        genesis: (StatusCode, String),
        spec: (StatusCode, String),
        spec_delay: Duration,
        /// L1 blocks served by number and hash.
        l1: Arc<Vec<Value>>,
        /// Block served by number in place of its canonical block after the first such read.
        reorged: Option<Arc<Value>>,
        reorged_reads: Arc<AtomicUsize>,
        /// Receipt requests that fail, echoing the credential, before succeeding.
        failing_receipts: Arc<AtomicUsize>,
        receipts_delay: Duration,
    }

    impl Default for Upstream {
        fn default() -> Self {
            Self {
                chain_id: L1_CHAIN_ID,
                finalized: Some(100),
                genesis: (
                    StatusCode::OK,
                    json!({"data": {"genesis_time": GENESIS_TIME.to_string()}}).to_string(),
                ),
                spec: Self::spec("12"),
                spec_delay: Duration::ZERO,
                l1: Arc::default(),
                reorged: None,
                reorged_reads: Arc::default(),
                failing_receipts: Arc::default(),
                receipts_delay: Duration::ZERO,
            }
        }
    }

    impl Upstream {
        /// A Beacon spec response with decimal `SECONDS_PER_SLOT`, as Beacon nodes serve it.
        fn spec(seconds_per_slot: &str) -> (StatusCode, String) {
            (StatusCode::OK, json!({"data": {"SECONDS_PER_SLOT": seconds_per_slot}}).to_string())
        }

        fn authorized(
            query: Option<String>,
            response: (StatusCode, String),
        ) -> (StatusCode, String) {
            if query.as_deref() == Some(CREDENTIAL) {
                response
            } else {
                (StatusCode::UNAUTHORIZED, String::new())
            }
        }

        async fn rpc(
            State(upstream): State<Self>,
            RawQuery(query): RawQuery,
            body: String,
        ) -> (StatusCode, String) {
            let request: Value = serde_json::from_str(&body).unwrap();
            let param = &request["params"][0];
            let result = match request["method"].as_str() {
                Some("eth_chainId") => json!(format!("{:#x}", upstream.chain_id)),
                Some("eth_getBlockByNumber") if param == "finalized" => {
                    upstream.finalized.map_or(Value::Null, |number| {
                        let mut block = alloy_rpc_types_eth::Block::<
                            alloy_rpc_types_eth::Transaction,
                        >::default();
                        block.header.inner.number = number;
                        serde_json::to_value(block).unwrap()
                    })
                }
                Some("eth_getBlockByNumber") => match &upstream.reorged {
                    Some(block)
                        if block["number"] == *param
                            && upstream.reorged_reads.fetch_add(1, Ordering::Relaxed) > 0 =>
                    {
                        (**block).clone()
                    }
                    _ => {
                        let number = u64::from_str_radix(
                            param.as_str().unwrap().trim_start_matches("0x"),
                            16,
                        )
                        .unwrap();
                        upstream.l1.get(number as usize).cloned().unwrap_or(Value::Null)
                    }
                },
                Some("eth_getBlockByHash") => upstream
                    .l1
                    .iter()
                    .find(|block| block["hash"] == *param)
                    .cloned()
                    .unwrap_or(Value::Null),
                Some("eth_getBlockReceipts") => {
                    sleep(upstream.receipts_delay).await;
                    if upstream
                        .failing_receipts
                        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                        .is_ok()
                    {
                        let error = json!({"code": -32000, "message": "invalid credential secret"});
                        let response =
                            json!({"jsonrpc": "2.0", "id": request["id"], "error": error});
                        return Self::authorized(query, (StatusCode::OK, response.to_string()));
                    }
                    json!([])
                }
                method => panic!("unexpected upstream request {method:?}"),
            };
            let response = json!({"jsonrpc": "2.0", "id": request["id"], "result": result});
            Self::authorized(query, (StatusCode::OK, response.to_string()))
        }

        /// Reads metadata for the default snapshot chain, whose rollup config accepts batches
        /// through L1 block `LATEST_ORIGIN + SEQ_WINDOW_SIZE`.
        async fn read(self, deadline: Duration) -> eyre::Result<SnapshotForkMetadata> {
            let fixture = SnapshotRpcFixture::default();
            let mut rollup_config = fixture.rollup_config();
            rollup_config.l1_chain_id = L1_CHAIN_ID;
            rollup_config.seq_window_size = SEQ_WINDOW_SIZE;
            let (l2_url, l2) = fixture.serve(SnapshotRpcFixture::CHAIN_ID).await;
            let inspection = SnapshotInspection::read(
                l2_url,
                Arc::new(rollup_config),
                SnapshotRpcFixture::CHAIN_ID,
            )
            .await
            .unwrap();
            l2.stop().unwrap();

            let (source, server) = self.serve().await;
            let result =
                SnapshotForkMetadata::read(&source, &inspection, Instant::now() + deadline).await;
            server.abort();
            redacted(result)
        }

        /// Serves this upstream behind [`CREDENTIAL`] until the returned task is aborted.
        async fn serve(self) -> (SnapshotForkSource, JoinHandle<()>) {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let source = SnapshotForkSource {
                execution: format!("{base}/rpc?{CREDENTIAL}").parse().unwrap(),
                beacon: format!("{base}/beacon/?{CREDENTIAL}").parse().unwrap(),
            };
            let app = Router::new()
                .route("/rpc", post(Self::rpc))
                .route(
                    "/beacon/eth/v1/beacon/genesis",
                    get(|State(upstream): State<Self>, RawQuery(query)| async move {
                        Self::authorized(query, upstream.genesis)
                    }),
                )
                .route(
                    "/beacon/eth/v1/config/spec",
                    get(|State(upstream): State<Self>, RawQuery(query)| async move {
                        sleep(upstream.spec_delay).await;
                        Self::authorized(query, upstream.spec)
                    }),
                )
                .with_state(self);
            (source, tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }))
        }
    }

    /// Asserts that a failure does not echo the upstream address or credential.
    fn redacted<T>(result: eyre::Result<T>) -> eyre::Result<T> {
        if let Err(error) = &result {
            let report = format!("{error:?}");
            assert!(!report.contains("127.0.0.1") && !report.contains("secret"), "{report}");
        }
        result
    }

    /// A Fjord-era chain whose L2 blocks are exactly what the derivation pipeline produces.
    struct Chain {
        rollup_config: RollupConfig,
        l1: Vec<Block<TxEnvelope>>,
        l2: Vec<Block<BaseTxEnvelope>>,
    }

    impl Chain {
        /// Builds the chain, activating Holocene at `holocene_time` when set.
        fn new(holocene_time: Option<u64>) -> Self {
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
                l2_chain_id: SnapshotRpcFixture::CHAIN_ID.into(),
                batch_inbox_address: BATCH_INBOX,
                upgrades: UpgradeConfig {
                    regolith_time: Some(0),
                    canyon_time: Some(0),
                    delta_time: Some(0),
                    ecotone_time: Some(0),
                    fjord_time: Some(0),
                    holocene_time,
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
            Self { rollup_config, l1, l2 }
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

        fn l1_info(&self, number: u64) -> BlockInfo {
            let header = &self.l1[number as usize].header;
            BlockInfo::new(header.hash_slow(), number, header.parent_hash, header.timestamp)
        }

        /// Serves the L2 blocks, without the genesis body, with the given finalized, safe, and
        /// latest labels.
        fn snapshot(&self, labels: [u64; 3]) -> SnapshotRpcFixture {
            let mut blocks: HashMap<_, _> = self.l2[1..]
                .iter()
                .map(|block| {
                    let tag = format!("{:#x}", block.header.number);
                    (tag, SnapshotRpcFixture::rpc_block(block.clone()))
                })
                .collect();
            for (label, number) in ["finalized", "safe", "latest"].into_iter().zip(labels) {
                blocks.insert(label.to_string(), blocks[&format!("{number:#x}")].clone());
            }
            SnapshotRpcFixture { genesis: self.l2[0].clone(), blocks }
        }

        /// Serves the L1 chain with every block finalized.
        fn upstream(&self) -> Upstream {
            Upstream {
                finalized: Some(L1_BLOCKS - 1),
                l1: Arc::new(self.l1.iter().map(l1_rpc_block).collect()),
                ..Default::default()
            }
        }

        async fn find(
            &self,
            snapshot: SnapshotRpcFixture,
            upstream: Upstream,
            timeout: Duration,
        ) -> eyre::Result<BlockInfo> {
            let (rpc_url, l2) = snapshot.serve(SnapshotRpcFixture::CHAIN_ID).await;
            let (source, server) = upstream.serve().await;
            let inspection = SnapshotInspection::read(
                rpc_url.clone(),
                Arc::new(self.rollup_config.clone()),
                SnapshotRpcFixture::CHAIN_ID,
            )
            .await
            .expect("snapshot without a genesis body is inspectable");
            let finder = SnapshotForkFinder { rpc_url, source, timeout };
            let result = tokio::time::timeout(timeout * 2, finder.find(&inspection))
                .await
                .expect("discovery honors its own deadline");
            l2.stop().unwrap();
            server.abort();
            redacted(result)
        }
    }

    fn l1_rpc_block(block: &Block<TxEnvelope>) -> Value {
        let hash = block.header.hash_slow();
        let transactions = block
            .body
            .transactions
            .iter()
            .enumerate()
            .map(|(index, transaction)| alloy_rpc_types_eth::Transaction {
                inner: Recovered::new_unchecked(transaction.clone(), Chain::batcher().address()),
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

    const TIMEOUT: Duration = Duration::from_secs(20);

    #[tokio::test]
    async fn reads_metadata_and_bounds_l1_range() {
        for (finalized, l1_limit) in
            [(100, LATEST_ORIGIN + SEQ_WINDOW_SIZE), (15, 15), (LATEST_ORIGIN, LATEST_ORIGIN)]
        {
            let upstream = Upstream { finalized: Some(finalized), ..Default::default() };
            let metadata = upstream.read(DEADLINE).await.unwrap();
            assert_eq!(
                metadata,
                SnapshotForkMetadata {
                    finalized,
                    l1_limit,
                    genesis_time: GENESIS_TIME,
                    slot_interval: 12
                }
            );
        }
    }

    #[tokio::test]
    async fn rejects_invalid_upstream_metadata() {
        let default = Upstream::default;
        for (upstream, message) in [
            (Upstream { chain_id: 11_155_111, ..default() }, "does not match rollup L1 chain ID 1"),
            (Upstream { finalized: None, ..default() }, "upstream L1 has no finalized block"),
            (
                Upstream { finalized: Some(LATEST_ORIGIN - 1), ..default() },
                "upstream finalized L1 block 10 is behind latest snapshot L1 origin 11",
            ),
            (
                Upstream { genesis: (StatusCode::OK, "not json".to_string()), ..default() },
                "failed to read upstream Beacon genesis time",
            ),
            (
                Upstream { spec: (StatusCode::INTERNAL_SERVER_ERROR, String::new()), ..default() },
                "failed to read upstream Beacon slot duration",
            ),
            (Upstream { spec: Upstream::spec("0"), ..default() }, "slot duration must be positive"),
        ] {
            let error = upstream.read(DEADLINE).await.unwrap_err();
            assert!(error.to_string().contains(message), "{error:?}");
        }
    }

    #[tokio::test]
    async fn times_out_at_deadline_on_stalled_upstream() {
        let upstream = Upstream { spec_delay: Duration::from_secs(3600), ..Default::default() };
        let error = upstream.read(Duration::from_millis(500)).await.unwrap_err();
        assert_eq!(error.to_string(), "timed out reading upstream Beacon slot duration");
    }

    #[tokio::test]
    async fn selects_inclusion_block_rather_than_l1_origin() {
        let chain = Chain::new(None);
        assert_eq!(chain.l2[LATEST as usize].header.mix_hash, chain.l1[10].header.mix_hash);

        let fork = chain.find(chain.snapshot(LABELS), chain.upstream(), TIMEOUT).await.unwrap();
        assert_eq!(fork, chain.l1_info(BATCH_L1_BLOCK));
        let json = serde_json::to_value(fork).unwrap();
        assert_eq!(json["number"], BATCH_L1_BLOCK);
        assert_eq!(json["parentHash"], serde_json::to_value(chain.l1_info(14).hash).unwrap());
    }

    #[tokio::test]
    async fn derives_across_holocene_activation() {
        // Holocene activates at L1 block 12, after the latest origin and before the batch.
        let chain = Chain::new(Some(L1_GENESIS_TIME + BLOCK_TIME * 12));

        let fork = chain.find(chain.snapshot(LABELS), chain.upstream(), TIMEOUT).await.unwrap();
        assert_eq!(fork, chain.l1_info(BATCH_L1_BLOCK));
    }

    #[tokio::test]
    async fn refuses_attributes_that_do_not_match_the_snapshot() {
        let mut chain = Chain::new(None);
        chain.l2[LATEST as usize].header.beneficiary = Address::repeat_byte(0xfe);

        let error = chain.find(chain.snapshot(LABELS), chain.upstream(), TIMEOUT).await;
        let error = error.unwrap_err().to_string();
        assert!(error.contains("do not match the snapshot block"), "{error}");
        assert!(error.contains("FeeRecipient"), "{error}");
    }

    #[tokio::test]
    async fn fails_when_batch_lies_beyond_upstream_finalized() {
        let chain = Chain::new(None);
        let upstream = Upstream { finalized: Some(BATCH_L1_BLOCK - 1), ..chain.upstream() };

        let error = chain.find(chain.snapshot(LABELS), upstream, TIMEOUT).await.unwrap_err();
        let expected = format!("no batch derives L2 block {LATEST} through L1 block 14");
        assert!(error.to_string().contains(&expected), "{error}");
    }

    #[tokio::test]
    async fn rejects_exhausted_range_at_holocene_activation() {
        let chain = Chain::new(Some(L1_GENESIS_TIME + BLOCK_TIME * (BATCH_L1_BLOCK - 1)));
        let upstream = Upstream { finalized: Some(BATCH_L1_BLOCK - 1), ..chain.upstream() };
        let error = chain.find(chain.snapshot(LABELS), upstream, Duration::from_secs(2)).await;
        let expected = format!("no batch derives L2 block {LATEST} through L1 block 14");
        assert!(error.unwrap_err().to_string().contains(&expected));
    }

    #[tokio::test]
    async fn rejects_fork_block_that_is_no_longer_canonical() {
        let chain = Chain::new(None);
        let mut reorged = chain.l1[BATCH_L1_BLOCK as usize].clone();
        reorged.header.mix_hash = B256::repeat_byte(0xee);
        let upstream =
            Upstream { reorged: Some(Arc::new(l1_rpc_block(&reorged))), ..chain.upstream() };

        let error = chain.find(chain.snapshot(LABELS), upstream, TIMEOUT).await.unwrap_err();
        assert!(error.to_string().contains("L1 block 15 that derives"), "{error}");
        assert!(error.to_string().contains("no longer canonical"), "{error}");
    }

    #[tokio::test]
    async fn rejects_snapshot_latest_that_changed() {
        let chain = Chain::new(None);
        let mut snapshot = chain.snapshot(LABELS);
        let mut replaced = chain.l2[LATEST as usize].clone();
        replaced.header.state_root = B256::repeat_byte(0xee);
        snapshot.blocks.insert("latest".to_string(), SnapshotRpcFixture::rpc_block(replaced));

        let error = chain.find(snapshot, chain.upstream(), TIMEOUT).await.unwrap_err();
        let expected = format!("snapshot L2 block {LATEST} changed during fork discovery");
        assert!(error.to_string().contains(&expected), "{error}");
    }

    #[tokio::test]
    async fn locates_latest_batch_from_parent_when_safe_is_latest() {
        let chain = Chain::new(None);

        let fork = chain.find(chain.snapshot([7, LATEST, LATEST]), chain.upstream(), TIMEOUT).await;
        assert_eq!(fork.unwrap(), chain.l1_info(BATCH_L1_BLOCK));
    }

    #[tokio::test]
    async fn rejects_safe_latest_tail_rooted_at_genesis() {
        let chain = Chain::new(None);

        let error = chain.find(chain.snapshot([1, 1, 1]), chain.upstream(), TIMEOUT).await;
        let error = error.unwrap_err().to_string();
        assert!(error.contains("its parent is the rollup genesis"), "{error}");
    }

    #[tokio::test]
    async fn rejects_missing_or_pruned_parent_when_safe_is_latest() {
        let chain = Chain::new(None);
        let mut pruned = chain.l2[LATEST as usize - 1].clone();
        pruned.body.transactions.clear();
        for parent in [None, Some(SnapshotRpcFixture::rpc_block(pruned))] {
            let mut snapshot = chain.snapshot([7, LATEST, LATEST]);
            match parent {
                Some(block) => snapshot.blocks.insert("0x8".to_string(), block),
                None => snapshot.blocks.remove("0x8"),
            };

            let error = chain.find(snapshot, chain.upstream(), TIMEOUT).await.unwrap_err();
            let expected = format!("snapshot parent of L2 block {LATEST} is missing or pruned");
            assert!(error.to_string().contains(&expected), "{error:?}");
        }
    }

    #[tokio::test]
    async fn retries_transient_upstream_errors() {
        let chain = Chain::new(None);
        let failing_receipts = Arc::new(AtomicUsize::new(2));
        let upstream = Upstream { failing_receipts, ..chain.upstream() };

        let fork = chain.find(chain.snapshot(LABELS), upstream, TIMEOUT).await.unwrap();
        assert_eq!(fork, chain.l1_info(BATCH_L1_BLOCK));
    }

    #[tokio::test]
    async fn transient_retries_share_the_discovery_deadline() {
        let chain = Chain::new(None);
        let failing_receipts = Arc::new(AtomicUsize::new(4));
        let upstream = Upstream { failing_receipts, ..chain.upstream() };

        // Each retry sleeps 500ms, less than this budget; giving each operation a fresh deadline
        // would eventually succeed instead of bounding the whole discovery.
        let error = chain.find(chain.snapshot(LABELS), upstream, Duration::from_millis(900)).await;
        let error = error.unwrap_err().to_string();
        assert!(error.contains("timed out after 900ms retrying derivation"), "{error}");
        let expected = format!(
            "last transient error: Temporary error: {}",
            SnapshotForkFinder::PROVIDER_ERROR
        );
        assert!(error.contains(&expected), "{error}");
    }

    #[tokio::test]
    async fn rejects_pruned_lookback_without_retrying() {
        let chain = Chain::new(None);
        let mut snapshot = chain.snapshot(LABELS);
        let mut pruned = chain.l2[5].clone();
        pruned.body.transactions.clear();
        snapshot.blocks.insert("0x5".to_string(), SnapshotRpcFixture::rpc_block(pruned));

        let error = chain.find(snapshot, chain.upstream(), Duration::from_secs(2)).await;
        let error = error.unwrap_err().to_string();
        assert!(error.contains("failed to reset derivation at L2 block"), "{error}");
        assert!(!error.contains("timed out"), "{error}");
        assert!(error.contains("L2 block info construction failed"), "{error}");
    }

    #[tokio::test]
    async fn metadata_reads_share_the_discovery_deadline() {
        let chain = Chain::new(None);
        // Derivation alone fits in the budget, but not after the slow metadata read.
        let upstream = Upstream {
            spec_delay: Duration::from_secs(1),
            receipts_delay: Duration::from_millis(100),
            ..chain.upstream()
        };

        let error = chain.find(chain.snapshot(LABELS), upstream, Duration::from_millis(1500)).await;
        let error = error.unwrap_err().to_string();
        assert!(error.starts_with("fork discovery timed out after 1.5s"), "{error}");
    }

    #[test]
    fn hides_endpoints_from_debug_and_env_errors() {
        let source = SnapshotForkSource {
            execution: "https://user:secret@l1.example/key".parse().unwrap(),
            beacon: "https://beacon.example/?token=secret".parse().unwrap(),
        };
        assert_eq!(format!("{source:?}"), "SnapshotForkSource { .. }");
        let error = SnapshotForkSource::env_url("SNAPSHOT_FORK_TEST_UNSET_URL").unwrap_err();
        assert_eq!(
            error.to_string(),
            "SNAPSHOT_FORK_TEST_UNSET_URL must be set to an upstream L1 URL"
        );
    }

    #[test]
    fn redacts_provider_errors_that_could_echo_upstream_urls() {
        for leak in [
            "error sending request for url (https://x/)",
            "dns error: failed to lookup beacon.example",
            "invalid credential secret",
            "unauthorized: token=secret",
        ] {
            for (error, kind) in [
                (PipelineError::Provider(leak.into()).temp(), "Temporary"),
                (PipelineError::Provider(leak.into()).crit(), "Critical"),
            ] {
                let expected = format!("{kind} error: {}", SnapshotForkFinder::PROVIDER_ERROR);
                assert_eq!(SnapshotForkFinder::diagnostic(&error), expected, "{leak}");
            }
        }
        for (error, expected) in [
            (PipelineError::NotEnoughData.temp(), "Temporary error: Not enough data"),
            (
                ResetError::BlobsUnavailable(7).reset(),
                "Pipeline reset: Blobs unavailable: beacon node returned 404 for slot 7",
            ),
        ] {
            assert_eq!(SnapshotForkFinder::diagnostic(&error), expected);
        }
    }
}
