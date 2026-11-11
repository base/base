//! Witness generation for Succinct ZK proving backends.

use std::{error::Error as StdError, fmt, num::NonZeroU64, sync::Arc};

use alloy_primitives::{Address, B256};
use base_l1_head::{L1HeadCalculator, L1HeadError};
use base_proof_host::Metrics;
use base_proof_zk_utils::boot::BootInfoStruct;
use base_proof_zk_witness::{fetcher::OPSuccinctDataFetcher, host::SuccinctHost};
use base_prover_service_protocol::{ZkProofRequest, ZkRangeError};
use sp1_sdk::{SP1ProofWithPublicValues, SP1Stdin, SP1VerifyingKey};
use thiserror::Error;
use tracing::{Instrument, debug, error, info, info_span};

use super::utils::{get_agg_proof_stdin, get_sp1_stdin};

/// Resolved inputs for range witness generation.
#[derive(Debug, Clone, Copy)]
struct WitnessParams {
    /// L2 block immediately before the range.
    pub start_block: u64,
    /// Last L2 block in the range, inclusive.
    pub end_block: u64,
    /// Source for the L1 head hash used by the Succinct host.
    pub l1_head: L1HeadSource,
    /// L2 block number whose timestamp determines the activated upgrade schedule.
    pub schedule_l2_block_number: Option<u64>,
    /// Number of L2 blocks between two intermediate output roots of the proven game.
    pub intermediate_block_interval: NonZeroU64,
}

/// Source used to select the L1 head hash for range witness generation.
#[derive(Debug, Clone, Copy)]
enum L1HeadSource {
    /// Use this exact L1 head hash.
    Pinned(B256),
    /// Calculate the L1 head from the range end and sequence window.
    SequenceWindow { sequence_window: u64 },
}

impl L1HeadSource {
    const fn variant_name(&self) -> &'static str {
        match self {
            Self::Pinned(_) => "Pinned",
            Self::SequenceWindow { .. } => "SequenceWindow",
        }
    }
}

/// Errors raised while generating Succinct witness stdin.
#[derive(Debug, Error)]
pub enum WitnessError {
    /// The requested block range violates the protocol constraints.
    #[error("invalid ZK proof range: {0}")]
    InvalidRange(#[source] ZkRangeError),
    /// Fetching host arguments with a caller-pinned L1 head failed.
    #[error("failed to fetch Succinct host args with caller-provided l1_head")]
    PinnedHostFetch {
        /// Underlying Succinct host error.
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
    /// Sequence-window L1-head calculation failed.
    #[error("failed to calculate sequence-window l1_head")]
    SequenceWindowL1Head {
        /// Sequence-window L1-head calculation error.
        #[source]
        source: L1HeadError,
    },
    /// Fetching host arguments with a sequence-window L1 head failed.
    #[error("failed to fetch Succinct host args with sequence-window l1_head")]
    SequenceWindowHostFetch {
        /// Underlying Succinct host error.
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
    /// Running the Succinct host failed.
    #[error("failed to run Succinct host")]
    HostRun {
        /// Underlying Succinct host error.
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
    /// Converting the generated witness into SP1 stdin failed.
    #[error("failed to build SP1 stdin from Succinct witness")]
    Stdin {
        /// Underlying witness conversion error.
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
    /// Fetching the latest L1 checkpoint head for aggregation failed.
    #[error("failed to fetch latest L1 checkpoint head for aggregation")]
    AggregationL1Head {
        /// Underlying header fetch error.
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
    /// Fetching L1 header preimages for aggregation failed.
    #[error("failed to fetch L1 header preimages for aggregation")]
    AggregationHeaders {
        /// Underlying header fetch error.
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
    /// Building aggregation stdin failed.
    #[error("failed to build aggregation stdin")]
    AggregationStdin {
        /// Underlying aggregation stdin error.
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
}

/// Provider wrapping the Succinct host for witness generation.
#[derive(Clone)]
pub struct OpSuccinctWitnessProvider {
    host: Arc<SuccinctHost>,
    default_sequence_window: u64,
    l1_node_url: String,
    base_consensus_url: String,
}

impl fmt::Debug for OpSuccinctWitnessProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpSuccinctWitnessProvider").finish_non_exhaustive()
    }
}

impl OpSuccinctWitnessProvider {
    fn resolve(
        request: &ZkProofRequest,
        default_sequence_window: u64,
    ) -> Result<WitnessParams, WitnessError> {
        Ok(WitnessParams {
            start_block: request.start_block_number,
            end_block: request.validate_range().map_err(WitnessError::InvalidRange)?,
            l1_head: request.l1_head.map_or_else(
                || L1HeadSource::SequenceWindow {
                    sequence_window: request.sequence_window.unwrap_or(default_sequence_window),
                },
                L1HeadSource::Pinned,
            ),
            schedule_l2_block_number: request.schedule_l2_block_number,
            intermediate_block_interval: request.intermediate_block_interval,
        })
    }

    /// Create a new provider with an initialized host.
    pub fn new(
        fetcher: Arc<OPSuccinctDataFetcher>,
        default_sequence_window: u64,
        l1_node_url: String,
        base_consensus_url: String,
    ) -> Self {
        info!("initializing Succinct witness provider with Ethereum DA");
        let host = Arc::new(SuccinctHost::new(fetcher));
        Self { host, default_sequence_window, l1_node_url, base_consensus_url }
    }

    /// Generate a range witness, using a pinned L1 head or calculating it from the sequence window.
    ///
    /// Log the backend selected by the request.
    pub async fn generate_range_witness(
        &self,
        request: &ZkProofRequest,
    ) -> Result<SP1Stdin, WitnessError> {
        let params = Self::resolve(request, self.default_sequence_window);
        let resolved = params.as_ref().ok().copied();
        let end_block = resolved.map(|params| params.end_block);
        let sequence_window = resolved.and_then(|params| match params.l1_head {
            L1HeadSource::Pinned(_) => None,
            L1HeadSource::SequenceWindow { sequence_window } => Some(sequence_window),
        });
        let l1_head_source = resolved.map(|params| params.l1_head.variant_name());
        let span = info_span!(
            "range_witness",
            backend = %request.zk_backend,
            start_block = request.start_block_number,
            end_block,
            number_of_blocks = request.number_of_blocks_to_prove,
            sequence_window,
            l1_head_source,
            intermediate_block_interval = %request.intermediate_block_interval,
        );
        async {
            info!(
                backend = %request.zk_backend,
                start_block = request.start_block_number,
                end_block,
                number_of_blocks = request.number_of_blocks_to_prove,
                sequence_window,
                l1_head_source,
                intermediate_block_interval = %request.intermediate_block_interval,
                "starting range witness generation"
            );
            let result = async {
                let params = params?;
                let timer = base_metrics::timed!(Metrics::witness_build_duration_seconds(
                    Metrics::PROVER_SP1
                ));
                let stdin = self.generate_witness_inner(params).await?;
                drop(timer);
                #[cfg(feature = "metrics")]
                Metrics::witness_size_bytes(Metrics::PROVER_SP1)
                    .record(stdin.buffer.iter().map(Vec::len).sum::<usize>() as f64);
                info!("range witness generation completed");
                Ok(stdin)
            }
            .await;
            if let Err(error) = &result {
                error!(
                    error = %error,
                    backend = %request.zk_backend,
                    start_block = request.start_block_number,
                    end_block,
                    number_of_blocks = request.number_of_blocks_to_prove,
                    sequence_window,
                    l1_head_source,
                    intermediate_block_interval = %request.intermediate_block_interval,
                    "range witness generation failed"
                );
            }
            result
        }
        .instrument(span)
        .await
    }

    /// Generate aggregation stdin from a completed compressed range proof.
    pub async fn generate_aggregation_witness(
        &self,
        mut range_proof: SP1ProofWithPublicValues,
        range_vk: &SP1VerifyingKey,
        prover_address: Address,
    ) -> Result<SP1Stdin, WitnessError> {
        let boot_info: BootInfoStruct = range_proof.public_values.read();
        let boot_infos = vec![boot_info];
        let proofs = vec![range_proof.proof];

        let header =
            self.host.fetcher.get_latest_l1_head_in_batch(&boot_infos).await.map_err(|source| {
                WitnessError::AggregationL1Head { source: source.into_boxed_dyn_error() }
            })?;
        let l1_head_hash = header.hash_slow();

        let headers =
            self.host.fetcher.get_header_preimages(&boot_infos, l1_head_hash).await.map_err(
                |source| WitnessError::AggregationHeaders { source: source.into_boxed_dyn_error() },
            )?;

        info!(
            l1_head_hash = %l1_head_hash,
            num_headers = headers.len(),
            "fetched L1 headers for aggregation proof"
        );

        get_agg_proof_stdin(proofs, boot_infos, headers, range_vk, l1_head_hash, prover_address)
            .map_err(|source| WitnessError::AggregationStdin {
                source: source.into_boxed_dyn_error(),
            })
    }

    async fn generate_witness_inner(
        &self,
        params: WitnessParams,
    ) -> Result<SP1Stdin, WitnessError> {
        let start_block = params.start_block;
        let end_block = params.end_block;

        let host_args = match params.l1_head {
            L1HeadSource::Pinned(hash) => {
                info!(hash = %hash, "using pinned L1 head");
                self.host
                    .fetch(
                        start_block,
                        end_block,
                        Some(hash),
                        false,
                        params.schedule_l2_block_number,
                        params.intermediate_block_interval,
                    )
                    .await
                    .map_err(|source| WitnessError::PinnedHostFetch {
                        source: source.into_boxed_dyn_error(),
                    })?
            }
            L1HeadSource::SequenceWindow { sequence_window } => {
                let (l1_head_block_num, l1_head_hash) =
                    L1HeadCalculator::calculate_l1_head_from_urls(
                        &self.l1_node_url,
                        &self.base_consensus_url,
                        end_block,
                        sequence_window,
                    )
                    .await
                    .map_err(|source| WitnessError::SequenceWindowL1Head { source })?;
                info!(
                    l1_head_block = l1_head_block_num,
                    l1_head_hash = %l1_head_hash,
                    "L1 head calculated from sequence window"
                );
                self.host
                    .fetch(
                        start_block,
                        end_block,
                        Some(l1_head_hash),
                        false,
                        params.schedule_l2_block_number,
                        params.intermediate_block_interval,
                    )
                    .await
                    .map_err(|source| WitnessError::SequenceWindowHostFetch {
                        source: source.into_boxed_dyn_error(),
                    })?
            }
        };

        debug!(start_block = start_block, end_block = end_block, "host args fetched");

        let witness =
            self.host.run(&host_args).await.map_err(|source| WitnessError::HostRun {
                source: source.into_boxed_dyn_error(),
            })?;
        get_sp1_stdin(witness)
            .map_err(|source| WitnessError::Stdin { source: source.into_boxed_dyn_error() })
    }
}

#[cfg(test)]
mod tests {
    use std::{io, sync::Mutex};

    use alloy_provider::ProviderBuilder;
    use base_proof_zk_witness::fetcher::RPCConfig;
    use base_prover_service_protocol::{ZkBackend, ZkVm};
    use serde_json::{Value, json};
    use tracing::Level;

    use super::*;

    fn request() -> ZkProofRequest {
        ZkProofRequest {
            start_block_number: 100,
            number_of_blocks_to_prove: 20,
            sequence_window: None,
            l1_head: None,
            schedule_l2_block_number: Some(90),
            intermediate_block_interval: NonZeroU64::new(5).unwrap(),
            zk_vm: ZkVm::Sp1,
            zk_backend: ZkBackend::Cluster,
        }
    }

    #[derive(Clone, Default)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    impl io::Write for LogCapture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl LogCapture {
        fn events(&self) -> Vec<Value> {
            std::str::from_utf8(&self.0.lock().unwrap())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    fn provider() -> OpSuccinctWitnessProvider {
        let url: url::Url = "http://127.0.0.1:1".parse().unwrap();
        let fetcher = OPSuccinctDataFetcher {
            rpc_config: RPCConfig {
                l1_rpc: url.clone(),
                l1_beacon_rpc: None,
                l2_rpc: url.clone(),
                l2_node_rpc: url.clone(),
                l1_config_dir: None,
                l2_config_dir: None,
            },
            l1_provider: Arc::new(ProviderBuilder::default().connect_http(url.clone())),
            l2_provider: Arc::new(ProviderBuilder::default().connect_http(url.clone())),
            rollup_config: None,
            rollup_config_path: None,
            l1_config_path: None,
        };
        OpSuccinctWitnessProvider::new(Arc::new(fetcher), 50, url.to_string(), url.to_string())
    }

    #[tokio::test]
    async fn range_witness_failure_keeps_request_fields_at_warn_level() {
        let provider = provider();
        let capture = LogCapture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_max_level(Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let request = ZkProofRequest { start_block_number: u64::MAX - 9, ..request() };

        let result = provider.generate_range_witness(&request).await;

        assert!(matches!(
            result,
            Err(WitnessError::InvalidRange(ZkRangeError::EndBlockOverflow { .. }))
        ));
        let events = capture.events();
        assert_eq!(events.len(), 1, "the provider must emit exactly one failure event");
        assert_eq!(events[0]["level"], "ERROR");
        let fields = &events[0]["fields"];
        assert_eq!(fields["message"], "range witness generation failed");
        assert_eq!(fields["backend"], request.zk_backend.as_str());
        assert_eq!(fields["start_block"], json!(18446744073709551606_u64));
        assert_eq!(fields["number_of_blocks"], 20);
        assert_eq!(fields["intermediate_block_interval"], "5");
        assert_eq!(
            fields["error"],
            "invalid ZK proof range: start block 18446744073709551606 plus block count 20 overflows u64"
        );
    }

    #[tokio::test]
    async fn range_witness_start_and_failure_are_logged_once_with_request_fields() {
        let provider = provider();
        let capture = LogCapture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_max_level(Level::INFO)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let request = ZkProofRequest { start_block_number: u64::MAX - 9, ..request() };

        let result = provider.generate_range_witness(&request).await;

        assert!(matches!(
            result,
            Err(WitnessError::InvalidRange(ZkRangeError::EndBlockOverflow { .. }))
        ));
        let events = capture.events();
        assert_eq!(events.len(), 2, "the provider must emit one start and one failure event");
        assert_eq!(events[0]["level"], "INFO");
        assert_eq!(
            events[0]["fields"],
            json!({
                "message": "starting range witness generation",
                "backend": request.zk_backend.as_str(),
                "start_block": 18446744073709551606_u64,
                "number_of_blocks": 20,
                "intermediate_block_interval": "5",
            })
        );
        assert_eq!(events[1]["level"], "ERROR");
        assert_eq!(events[1]["fields"]["message"], "range witness generation failed");
    }

    #[test]
    fn resolve_preserves_pinned_l1_head() {
        let request = ZkProofRequest { l1_head: Some(B256::repeat_byte(0x42)), ..request() };
        let params = OpSuccinctWitnessProvider::resolve(&request, 50).unwrap();
        assert!(
            matches!(params.l1_head, L1HeadSource::Pinned(hash) if hash == B256::repeat_byte(0x42))
        );
    }

    #[test]
    fn resolve_calculates_l1_head_from_range_end_with_default_sequence_window() {
        let params = OpSuccinctWitnessProvider::resolve(&request(), 50).unwrap();
        assert_eq!(params.start_block, 100);
        assert_eq!(params.end_block, 120);
        assert!(matches!(params.l1_head, L1HeadSource::SequenceWindow { sequence_window: 50, .. }));
    }

    #[test]
    fn resolve_uses_request_sequence_window() {
        let request = ZkProofRequest { sequence_window: Some(12), ..request() };
        let params = OpSuccinctWitnessProvider::resolve(&request, 50).unwrap();
        assert!(matches!(params.l1_head, L1HeadSource::SequenceWindow { sequence_window: 12, .. }));
    }

    #[test]
    fn resolve_preserves_schedule_block_and_intermediate_block_interval() {
        let params = OpSuccinctWitnessProvider::resolve(&request(), 50).unwrap();
        assert_eq!(params.schedule_l2_block_number, Some(90));
        assert_eq!(params.intermediate_block_interval.get(), 5);
        let request = ZkProofRequest { schedule_l2_block_number: None, ..request() };
        assert_eq!(
            OpSuccinctWitnessProvider::resolve(&request, 50).unwrap().schedule_l2_block_number,
            None
        );
    }

    #[test]
    fn resolve_rejects_empty_range() {
        let request = ZkProofRequest { number_of_blocks_to_prove: 0, ..request() };
        let error = OpSuccinctWitnessProvider::resolve(&request, 50).unwrap_err();
        assert!(matches!(error, WitnessError::InvalidRange(_)));
        assert_eq!(
            error.source().unwrap().to_string(),
            "number_of_blocks_to_prove must be greater than zero"
        );
    }

    #[test]
    fn resolve_rejects_misaligned_range() {
        let request = ZkProofRequest { number_of_blocks_to_prove: 9, ..request() };
        let error = OpSuccinctWitnessProvider::resolve(&request, 50).unwrap_err();
        assert!(matches!(error, WitnessError::InvalidRange(_)));
        assert_eq!(
            error.source().unwrap().to_string(),
            "block count 9 must be a multiple of intermediate block interval 5"
        );
    }

    #[test]
    fn resolve_overflow_keeps_range_error_source() {
        let request = ZkProofRequest { start_block_number: u64::MAX - 19, ..request() };
        let error = OpSuccinctWitnessProvider::resolve(&request, 50).unwrap_err();
        assert!(matches!(error, WitnessError::InvalidRange(_)));
        assert_eq!(
            error.source().unwrap().to_string(),
            "start block 18446744073709551596 plus block count 20 overflows u64"
        );
    }

    #[test]
    fn resolve_rejects_end_block_overflow() {
        let request = ZkProofRequest { start_block_number: u64::MAX - 9, ..request() };
        assert!(matches!(
            OpSuccinctWitnessProvider::resolve(&request, 50),
            Err(WitnessError::InvalidRange(ZkRangeError::EndBlockOverflow { .. }))
        ));
        let request = ZkProofRequest { start_block_number: u64::MAX - 20, ..request };
        assert_eq!(OpSuccinctWitnessProvider::resolve(&request, 50).unwrap().end_block, u64::MAX);
    }
}
