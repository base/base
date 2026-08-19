//! L1 upgrade signal contract reader.

use core::time::Duration;

use alloy_json_rpc::{RequestPacket, ResponsePacket};
use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::{BlockId, BlockNumberOrTag, TransactionInput, TransactionRequest};
use alloy_sol_types::{SolCall, sol};
use alloy_transport::{TransportError, TransportErrorKind, TransportFut, utils::guess_local_url};
use backon::Retryable;
use base_common_genesis::BaseUpgrade;
use base_retry::RetryConfig;
use futures::future::try_join;
use reqwest::Client;
use tower::{ServiceExt, service_fn};
use tracing::warn;
use url::Url;

use crate::{
    UpgradeSignal, UpgradeSignalError, UpgradeSignalMetricLayer, UpgradeSignalMetrics,
    UpgradeSignalSchedule,
};

sol! {
    /// L1 `ProtocolVersions` upgrade schedule interface.
    ///
    /// The address can be a proxy. Nodes only depend on this read interface.
    interface IProtocolVersions {
        /// Returns the activation timestamp for every registered upgrade, ordered by ascending
        /// upgrade id (`0` = not scheduled).
        function getSchedule() external view returns (uint64[] memory);

        /// Returns the minimum protocol version clients must run (packed semver).
        function minimumProtocolVersion() external view returns (uint256);
    }
}

/// Reads upgrade signals from an L1 contract with Alloy.
#[derive(Debug, Clone)]
pub struct AlloyUpgradeSignalReader {
    /// L1 provider using a size-bounded HTTP transport.
    pub provider: RootProvider,
    /// L1 contract or proxy address.
    pub contract_address: Address,
    /// L1 block tag used to pin reads. Defaults to [`BlockNumberOrTag::Finalized`].
    pub block_tag: BlockNumberOrTag,
}

impl AlloyUpgradeSignalReader {
    /// Maximum JSON-RPC response body accepted from an upgrade signal endpoint.
    pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;

    /// Maximum number of schedule entries accepted from the L1 contract.
    ///
    /// This leaves substantial room for future registered upgrades while bounding ABI decoder
    /// allocation independently of the set this binary currently understands.
    pub const MAX_SCHEDULE_LENGTH: usize = 256;

    /// Creates a new Alloy-backed upgrade signal reader that reads at the finalized L1 head.
    pub fn new(
        l1_rpc: Url,
        contract_address: Address,
        request_timeout: Duration,
    ) -> Result<Self, UpgradeSignalError> {
        if !matches!(l1_rpc.scheme(), "http" | "https") {
            return Err(UpgradeSignalError::provider(
                "build upgrade signal HTTP client failed",
                "URL scheme must be http or https",
            ));
        }

        let client = Client::builder().timeout(request_timeout).build().map_err(|error| {
            UpgradeSignalError::provider("build upgrade signal HTTP client failed", error)
        })?;

        let is_local = guess_local_url(l1_rpc.as_str());
        // Alloy's built-in reqwest transport collects the entire response before decoding it, so
        // cap the stream here and feed the resulting transport back into Alloy's typed provider.
        let transport = service_fn(move |request: RequestPacket| {
            let client = client.clone();
            let l1_rpc = l1_rpc.clone();
            async move {
                let headers = request.headers();
                let mut response = client
                    .post(l1_rpc)
                    .json(&request)
                    .headers(headers)
                    .send()
                    .await
                    .map_err(TransportErrorKind::custom)?;
                let status = response.status();
                let capacity = response
                    .content_length()
                    .and_then(|length| usize::try_from(length).ok())
                    .unwrap_or_default()
                    .min(Self::MAX_RESPONSE_BYTES);
                let mut body = Vec::with_capacity(capacity);

                while let Some(chunk) =
                    response.chunk().await.map_err(TransportErrorKind::custom)?
                {
                    if body.len().saturating_add(chunk.len()) > Self::MAX_RESPONSE_BYTES {
                        return Err(TransportErrorKind::non_retryable_str(
                            "upgrade signal JSON-RPC response exceeds 256 KiB",
                        ));
                    }
                    body.extend_from_slice(&chunk);
                }

                if !status.is_success() {
                    if let Ok(response) = serde_json::from_slice::<ResponsePacket>(&body)
                        && response.is_error()
                    {
                        return Ok(response);
                    }
                    return Err(TransportErrorKind::http_error(
                        status.as_u16(),
                        String::from_utf8_lossy(&body).into_owned(),
                    ));
                }

                serde_json::from_slice(&body).map_err(|error| {
                    TransportError::deser_err(error, String::from_utf8_lossy(&body))
                })
            }
        })
        .map_future(|future| Box::pin(future) as TransportFut<'static>);
        let provider = RootProvider::new(RpcClient::new(transport, is_local));

        Ok(Self { provider, contract_address, block_tag: BlockNumberOrTag::Finalized })
    }

    /// Sets the L1 block tag used to pin reads.
    pub const fn with_block_tag(mut self, block_tag: BlockNumberOrTag) -> Self {
        self.block_tag = block_tag;
        self
    }

    /// Executes an `eth_call` against the upgrade signal contract at a specific L1 block.
    pub async fn call_at_block<C>(
        &self,
        call: C,
        block: BlockId,
        context: &'static str,
    ) -> Result<Bytes, UpgradeSignalError>
    where
        C: SolCall,
    {
        let request = TransactionRequest::default()
            .to(self.contract_address)
            .input(TransactionInput::new(Bytes::from(call.abi_encode())));

        self.provider
            .call(request)
            .block(block)
            .await
            .map_err(|error| UpgradeSignalError::provider(context, error))
    }

    /// Returns the L1 block number and concrete block ID for the configured block tag.
    ///
    /// Pinning reads to a concrete block hash ensures every contract call in a schedule read
    /// observes the same L1 state. The block tag (finalized by default) keeps the schedule
    /// reorg-stable.
    pub async fn pinned_l1_block_id(&self) -> Result<(u64, BlockId), UpgradeSignalError> {
        let block = self
            .provider
            .get_block_by_number(self.block_tag)
            .await
            .map_err(|error| UpgradeSignalError::provider("get L1 block failed", error))?
            .ok_or_else(|| {
                UpgradeSignalError::provider("get L1 block failed", "missing block for tag")
            })?;

        Ok((block.header.number, BlockId::hash(block.header.hash)))
    }

    /// Reads the contract's id-ordered activation timestamps and the global minimum protocol
    /// version using a previously observed L1 block ID.
    pub async fn read_contract_schedule_at_l1_block(
        &self,
        l1_block: BlockId,
    ) -> Result<(Vec<u64>, U256), UpgradeSignalError> {
        let (schedule_output, version_output) = try_join(
            self.call_at_block(
                IProtocolVersions::getScheduleCall {},
                l1_block,
                "getSchedule failed",
            ),
            self.call_at_block(
                IProtocolVersions::minimumProtocolVersionCall {},
                l1_block,
                "minimumProtocolVersion failed",
            ),
        )
        .await?;

        Self::validate_schedule_abi_length(schedule_output.as_ref())?;
        let timestamps =
            IProtocolVersions::getScheduleCall::abi_decode_returns(schedule_output.as_ref())
                .map_err(|error| UpgradeSignalError::decode("getSchedule decode failed", error))?;

        let minimum_protocol_version =
            IProtocolVersions::minimumProtocolVersionCall::abi_decode_returns(
                version_output.as_ref(),
            )
            .map_err(|error| {
                UpgradeSignalError::decode("minimumProtocolVersion decode failed", error)
            })?;

        Ok((timestamps, minimum_protocol_version))
    }

    /// Validates the dynamic ABI array length before the Solidity decoder allocates its `Vec`.
    pub fn validate_schedule_abi_length(encoded: &[u8]) -> Result<(), UpgradeSignalError> {
        const ABI_WORD_BYTES: usize = 32;
        const SCHEDULE_OFFSET: usize = ABI_WORD_BYTES;
        const LENGTH_END: usize = SCHEDULE_OFFSET + ABI_WORD_BYTES;

        if encoded.len() < LENGTH_END {
            return Err(UpgradeSignalError::decode(
                "getSchedule decode failed",
                "return data is shorter than the schedule offset and length words",
            ));
        }

        let offset = U256::from_be_slice(&encoded[..ABI_WORD_BYTES]);
        if offset != U256::from(SCHEDULE_OFFSET) {
            return Err(UpgradeSignalError::decode(
                "getSchedule decode failed",
                "schedule array has a non-canonical ABI offset",
            ));
        }

        let declared_length = U256::from_be_slice(&encoded[SCHEDULE_OFFSET..LENGTH_END]);
        if declared_length > U256::from(Self::MAX_SCHEDULE_LENGTH) {
            return Err(UpgradeSignalError::decode(
                "getSchedule decode failed",
                format!(
                    "schedule declares {declared_length} entries, exceeding the maximum {}",
                    Self::MAX_SCHEDULE_LENGTH
                ),
            ));
        }

        Ok(())
    }

    /// Maps the contract's id-ordered activation timestamps onto the node's hardfork ladder.
    ///
    /// The contract keys upgrades by ascending numeric registration id and keeps names offchain,
    /// so entries are aligned with [`BaseUpgrade::CONTRACT_VARIANTS`] by registration id: id `0`
    /// maps to the oldest contract-backed hardfork, and each following id maps to the next
    /// hardfork in the ladder. This is a positional mapping by id, not a sort by timestamp, so the
    /// timestamps need not be monotonic. Contract entries beyond the ladder
    /// belong to upgrades newer than this binary knows and are logged and ignored, and hardforks
    /// without a contract entry produce no signal. Every signal carries the contract's global
    /// minimum protocol version.
    pub fn map_schedule(
        timestamps: &[u64],
        minimum_protocol_version: U256,
        l1_block_number: u64,
    ) -> UpgradeSignalSchedule {
        if timestamps.len() > BaseUpgrade::CONTRACT_VARIANTS.len() {
            warn!(
                target: "upgrade_signal",
                contract_upgrades = timestamps.len(),
                known_upgrades = BaseUpgrade::CONTRACT_VARIANTS.len(),
                "L1 schedule has more upgrades than this binary knows; newest entries ignored"
            );
        }

        let signals: Vec<_> = BaseUpgrade::CONTRACT_VARIANTS
            .iter()
            .zip(timestamps.iter())
            .map(|(upgrade_id, activation_timestamp)| UpgradeSignal {
                upgrade_id: *upgrade_id,
                activation_timestamp: *activation_timestamp,
                protocol_version: minimum_protocol_version,
            })
            .collect();

        UpgradeSignalSchedule::new(l1_block_number, signals)
    }

    /// Reads the full contract-backed upgrade signal schedule.
    ///
    /// Records `l1_read_errors_total` for all contract-backed upgrades when the L1 block fetch or
    /// the schedule read fails; the whole schedule is read with one `getSchedule` call, so
    /// per-upgrade failures no longer exist.
    ///
    /// A successful read that yields no signals is rejected with
    /// [`UpgradeSignalError::EmptySchedule`] rather than mapped to an empty schedule: applying an
    /// empty schedule would replace every runtime override with base activations and report
    /// success, silently diverging from the live poller, which ignores empty reads. An empty read
    /// is not counted as an `l1_read_errors_total` failure, keeping empty success distinct from a
    /// read failure. Callers decide how to treat it: startup retries then tolerates it (see
    /// [`read_schedule_with_retries`](Self::read_schedule_with_retries)), while the manual admin
    /// refresh surfaces it as an error instead of clearing overrides.
    pub async fn read_schedule(
        &self,
        metrics_layers: &[UpgradeSignalMetricLayer],
    ) -> Result<UpgradeSignalSchedule, UpgradeSignalError> {
        let (l1_block_number, l1_block) = match self.pinned_l1_block_id().await {
            Ok(block) => block,
            Err(error) => {
                UpgradeSignalMetrics::record_l1_read_errors_for_layers(metrics_layers);
                return Err(error);
            }
        };

        let (timestamps, minimum_protocol_version) =
            match self.read_contract_schedule_at_l1_block(l1_block).await {
                Ok(values) => values,
                Err(error) => {
                    UpgradeSignalMetrics::record_l1_read_errors_for_layers(metrics_layers);
                    return Err(error);
                }
            };

        let schedule = Self::map_schedule(&timestamps, minimum_protocol_version, l1_block_number);
        if schedule.signals.is_empty() {
            return Err(UpgradeSignalError::EmptySchedule);
        }

        Ok(schedule)
    }

    /// Reads the schedule, retrying transient failures with bounded exponential jitter before
    /// giving up.
    ///
    /// Used on the startup path, where a single transient L1 error should not abort node launch
    /// outright; after `max_attempts` failures the last error is returned (fail-fast). This future
    /// is cancellation-safe: dropping it during shutdown cancels an in-flight HTTP request or
    /// retry sleep.
    ///
    /// [`UpgradeSignalError::EmptySchedule`] is retried alongside provider errors: a freshly
    /// deployed or just-initialized contract can briefly report an empty schedule at the finalized
    /// tag, so a few retries let a real schedule settle before callers fall back. Decode and
    /// protocol-version errors remain fail-fast.
    pub async fn read_schedule_with_retries(
        &self,
        max_attempts: u32,
        initial_backoff: Duration,
        max_backoff: Duration,
        metrics_layers: &[UpgradeSignalMetricLayer],
    ) -> Result<UpgradeSignalSchedule, UpgradeSignalError> {
        let max_attempts = max_attempts.max(1);
        let mut attempt = 1;
        let retry_config =
            RetryConfig::new(max_attempts.saturating_sub(1), initial_backoff, max_backoff);

        (|| self.read_schedule(metrics_layers))
            .retry(retry_config.to_backoff_builder())
            .when(|error| {
                matches!(
                    error,
                    UpgradeSignalError::Provider { .. } | UpgradeSignalError::EmptySchedule
                )
            })
            // Backon adds jitter after enforcing `max_delay`, so cap the yielded delay too.
            .adjust(|_, retry_delay| retry_delay.map(|delay| delay.min(max_backoff)))
            .notify(|error, retry_delay| {
                warn!(
                    target: "upgrade_signal",
                    attempt,
                    max_attempts,
                    retry_delay_ms = u64::try_from(retry_delay.as_millis()).unwrap_or(u64::MAX),
                    error = %error,
                    "retrying L1 upgrade signal read"
                );
                attempt += 1;
            })
            .await
    }

    /// Reads the schedule, tolerating read failures.
    ///
    /// Records `l1_read_errors_total` and returns `None` when the read fails. Intended for the live
    /// metrics poller, which must not abort the node because a schedule read failed.
    pub async fn read_schedule_tolerant(
        &self,
        metrics_layers: &[UpgradeSignalMetricLayer],
    ) -> Option<UpgradeSignalSchedule> {
        match self.read_schedule(metrics_layers).await {
            Ok(schedule) => Some(schedule),
            Err(error) => {
                warn!(
                    target: "upgrade_signal",
                    error = %error,
                    "failed to read live L1 upgrade signal schedule"
                );
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, hex};
    use alloy_rpc_types_eth::Block;
    use alloy_sol_types::SolCall;
    use httpmock::prelude::*;

    use super::*;

    fn signals(schedule: &UpgradeSignalSchedule) -> Vec<(BaseUpgrade, u64)> {
        schedule
            .signals
            .iter()
            .map(|signal| (signal.upgrade_id, signal.activation_timestamp))
            .collect()
    }

    fn schedule_abi_header(declared_length: U256) -> Vec<u8> {
        let mut encoded = vec![0_u8; 64];
        encoded[31] = 32;
        encoded[32..64].copy_from_slice(&declared_length.to_be_bytes::<32>());
        encoded
    }

    fn is_reqwest_timeout<T>(result: &alloy_transport::TransportResult<T>) -> bool {
        result
            .as_ref()
            .err()
            .and_then(|error| error.as_transport_err())
            .and_then(|error| error.as_custom())
            .and_then(|error| error.downcast_ref::<reqwest::Error>())
            .is_some_and(reqwest::Error::is_timeout)
    }

    #[tokio::test]
    async fn reads_block_and_contract_call_through_bounded_alloy_provider() {
        let server = MockServer::start_async().await;
        let block_hash = B256::repeat_byte(1);
        let mut block: Block = Block::default();
        block.header.hash = block_hash;
        block.header.inner.number = 42;
        let block_mock = server
            .mock_async(|when, then| {
                when.method(POST).path("/").body_includes("eth_getBlockByNumber");
                then.json_body(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 0,
                    "result": block,
                }));
            })
            .await;
        let call_output = Bytes::from(vec![7_u8; 32]);
        let expected_call_output = call_output.clone();
        let call_mock = server
            .mock_async(|when, then| {
                when.method(POST).path("/").body_includes("eth_call");
                then.json_body(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": call_output,
                }));
            })
            .await;
        let reader = AlloyUpgradeSignalReader::new(
            server.url("/").parse().unwrap(),
            Address::ZERO,
            Duration::from_secs(1),
        )
        .unwrap();

        let (block_number, block_id) = reader.pinned_l1_block_id().await.unwrap();
        let output = reader
            .call_at_block(
                IProtocolVersions::getScheduleCall {},
                block_id,
                "mock getSchedule failed",
            )
            .await
            .unwrap();

        assert_eq!(block_number, 42);
        assert_eq!(block_id, BlockId::hash(block_hash));
        assert_eq!(output, expected_call_output);
        block_mock.assert_calls_async(1).await;
        call_mock.assert_calls_async(1).await;
    }

    #[tokio::test]
    async fn times_out_slow_alloy_provider_response() {
        let server = MockServer::start_async().await;
        let response_delay = Duration::from_millis(250);
        let mock = server
            .mock_async(|when, then| {
                when.method(POST).path("/");
                then.status(200).delay(response_delay);
            })
            .await;
        let reader = AlloyUpgradeSignalReader::new(
            server.url("/").parse().unwrap(),
            Address::ZERO,
            Duration::from_millis(25),
        )
        .unwrap();

        let result = tokio::time::timeout(
            Duration::from_millis(500),
            reader.provider.get_block_by_number(BlockNumberOrTag::Latest),
        )
        .await
        .expect("provider request must not remain pending");

        assert!(is_reqwest_timeout(&result));
        mock.assert_calls_async(1).await;
    }

    #[tokio::test]
    async fn rejects_oversized_rpc_response() {
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(POST).path("/");
                then.body(vec![b' '; AlloyUpgradeSignalReader::MAX_RESPONSE_BYTES + 1]);
            })
            .await;
        let reader = AlloyUpgradeSignalReader::new(
            server.url("/").parse().unwrap(),
            Address::ZERO,
            Duration::from_secs(1),
        )
        .unwrap();

        let error = reader.pinned_l1_block_id().await.unwrap_err();

        assert!(error.to_string().contains("response exceeds 256 KiB"));
    }

    #[tokio::test]
    async fn retries_provider_errors() {
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(POST).path("/");
                then.status(503);
            })
            .await;
        let reader = AlloyUpgradeSignalReader::new(
            server.url("/").parse().unwrap(),
            Address::ZERO,
            Duration::from_secs(1),
        )
        .unwrap();

        let error = reader
            .read_schedule_with_retries(3, Duration::ZERO, Duration::ZERO, &[])
            .await
            .unwrap_err();

        assert!(matches!(error, UpgradeSignalError::Provider { .. }));
        mock.assert_calls_async(3).await;
    }

    #[tokio::test]
    async fn rejects_empty_schedule_read_without_recording_read_error() {
        let server = MockServer::start_async().await;
        let block_hash = B256::repeat_byte(1);
        let mut block: Block = Block::default();
        block.header.hash = block_hash;
        block.header.inner.number = 42;
        server
            .mock_async(|when, then| {
                when.method(POST).path("/").body_includes("eth_getBlockByNumber");
                then.json_body(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 0,
                    "result": block,
                }));
            })
            .await;
        let empty_schedule = Bytes::from(schedule_abi_header(U256::ZERO));
        server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/")
                    .body_includes(hex::encode(IProtocolVersions::getScheduleCall::SELECTOR));
                then.json_body(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 0,
                    "result": empty_schedule,
                }));
            })
            .await;
        let minimum_protocol_version = Bytes::from(vec![0_u8; 32]);
        server
            .mock_async(|when, then| {
                when.method(POST).path("/").body_includes(hex::encode(
                    IProtocolVersions::minimumProtocolVersionCall::SELECTOR,
                ));
                then.json_body(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 0,
                    "result": minimum_protocol_version,
                }));
            })
            .await;
        let reader = AlloyUpgradeSignalReader::new(
            server.url("/").parse().unwrap(),
            Address::ZERO,
            Duration::from_secs(1),
        )
        .unwrap();

        let error = reader.read_schedule(&[]).await.unwrap_err();

        assert!(matches!(error, UpgradeSignalError::EmptySchedule));
    }

    #[tokio::test]
    async fn retries_empty_schedule_reads() {
        let server = MockServer::start_async().await;
        let block_hash = B256::repeat_byte(1);
        let mut block: Block = Block::default();
        block.header.hash = block_hash;
        block.header.inner.number = 42;
        server
            .mock_async(|when, then| {
                when.method(POST).path("/").body_includes("eth_getBlockByNumber");
                then.json_body(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 0,
                    "result": block,
                }));
            })
            .await;
        let empty_schedule = Bytes::from(schedule_abi_header(U256::ZERO));
        let schedule_mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/")
                    .body_includes(hex::encode(IProtocolVersions::getScheduleCall::SELECTOR));
                then.json_body(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 0,
                    "result": empty_schedule,
                }));
            })
            .await;
        let minimum_protocol_version = Bytes::from(vec![0_u8; 32]);
        server
            .mock_async(|when, then| {
                when.method(POST).path("/").body_includes(hex::encode(
                    IProtocolVersions::minimumProtocolVersionCall::SELECTOR,
                ));
                then.json_body(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 0,
                    "result": minimum_protocol_version,
                }));
            })
            .await;
        let reader = AlloyUpgradeSignalReader::new(
            server.url("/").parse().unwrap(),
            Address::ZERO,
            Duration::from_secs(1),
        )
        .unwrap();

        let error = reader
            .read_schedule_with_retries(3, Duration::ZERO, Duration::ZERO, &[])
            .await
            .unwrap_err();

        assert!(matches!(error, UpgradeSignalError::EmptySchedule));
        schedule_mock.assert_calls_async(3).await;
    }

    #[tokio::test]
    async fn does_not_retry_decode_errors() {
        let server = MockServer::start_async().await;
        let block_hash = B256::repeat_byte(1);
        let mut block: Block = Block::default();
        block.header.hash = block_hash;
        block.header.inner.number = 42;
        let block_mock = server
            .mock_async(|when, then| {
                when.method(POST).path("/").body_includes("eth_getBlockByNumber");
                then.json_body(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 0,
                    "result": block,
                }));
            })
            .await;
        let call_mock = server
            .mock_async(|when, then| {
                when.method(POST).path("/").body_includes("eth_call");
                then.json_body(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": "0x",
                }));
            })
            .await;
        let reader = AlloyUpgradeSignalReader::new(
            server.url("/").parse().unwrap(),
            Address::ZERO,
            Duration::from_secs(1),
        )
        .unwrap();

        let error = reader
            .read_schedule_with_retries(3, Duration::ZERO, Duration::ZERO, &[])
            .await
            .unwrap_err();

        assert!(matches!(error, UpgradeSignalError::Decode { .. }));
        block_mock.assert_calls_async(1).await;
        call_mock.assert_calls_async(2).await;
    }

    #[test]
    fn rejects_unsupported_rpc_url_without_panicking() {
        let error = AlloyUpgradeSignalReader::new(
            "ws://127.0.0.1:8545".parse().unwrap(),
            Address::ZERO,
            Duration::from_secs(1),
        )
        .unwrap_err();

        assert!(matches!(error, UpgradeSignalError::Provider { .. }));
    }

    #[test]
    fn accepts_schedule_length_at_limit_before_decoding() {
        let encoded =
            schedule_abi_header(U256::from(AlloyUpgradeSignalReader::MAX_SCHEDULE_LENGTH));

        assert!(AlloyUpgradeSignalReader::validate_schedule_abi_length(&encoded).is_ok());
    }

    #[test]
    fn rejects_oversized_schedule_before_decoding() {
        let declared_length = AlloyUpgradeSignalReader::MAX_SCHEDULE_LENGTH + 1;
        let encoded = schedule_abi_header(U256::from(declared_length));

        let error = AlloyUpgradeSignalReader::validate_schedule_abi_length(&encoded).unwrap_err();

        assert!(matches!(error, UpgradeSignalError::Decode { .. }));
    }

    #[test]
    fn rejects_non_canonical_schedule_offset_before_decoding() {
        let mut encoded = schedule_abi_header(U256::ZERO);
        encoded[31] = 64;

        let error = AlloyUpgradeSignalReader::validate_schedule_abi_length(&encoded).unwrap_err();

        assert!(matches!(error, UpgradeSignalError::Decode { .. }));
    }

    #[test]
    fn maps_partial_schedule_to_oldest_hardforks() {
        let schedule = AlloyUpgradeSignalReader::map_schedule(&[10, 20, 0], U256::from(7), 99);

        assert_eq!(
            signals(&schedule),
            vec![(BaseUpgrade::Regolith, 10), (BaseUpgrade::Canyon, 20), (BaseUpgrade::Delta, 0)]
        );
        assert!(schedule.signals.iter().all(|signal| signal.protocol_version == U256::from(7)));
        assert_eq!(schedule.l1_block_number, 99);
    }

    #[test]
    fn maps_full_schedule_in_ladder_order() {
        let timestamps: Vec<u64> = (1..=BaseUpgrade::CONTRACT_VARIANTS.len() as u64).collect();

        let schedule = AlloyUpgradeSignalReader::map_schedule(&timestamps, U256::from(7), 1);

        assert_eq!(
            signals(&schedule),
            BaseUpgrade::CONTRACT_VARIANTS.iter().copied().zip(timestamps).collect::<Vec<_>>()
        );
    }

    #[test]
    fn ignores_entries_newer_than_known_ladder() {
        let mut timestamps: Vec<u64> = (1..=BaseUpgrade::CONTRACT_VARIANTS.len() as u64).collect();
        timestamps.push(777);

        let schedule = AlloyUpgradeSignalReader::map_schedule(&timestamps, U256::from(7), 1);

        assert_eq!(schedule.signals.len(), BaseUpgrade::CONTRACT_VARIANTS.len());
        assert_eq!(signals(&schedule).first().copied(), Some((BaseUpgrade::Regolith, 1)));
        assert!(!signals(&schedule).iter().any(|(_, timestamp)| *timestamp == 777));
    }

    #[test]
    fn produces_no_signal_for_hardforks_without_contract_entries() {
        let schedule = AlloyUpgradeSignalReader::map_schedule(&[42], U256::from(7), 1);

        assert_eq!(signals(&schedule), vec![(BaseUpgrade::Regolith, 42)]);
    }
}
