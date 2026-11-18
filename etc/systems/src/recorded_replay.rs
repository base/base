//! `base-bench replay --engine-store-dir`: replays the engine API calls a node recorded with
//! `--debug.engine-api-store`, verbatim and in recorded order, into a node restored to an earlier
//! head. Unlike block replay, this reproduces what the recording node saw: unsafe reorgs,
//! payload-building forkchoice updates, and safe and finalized head advances.

use std::{
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use alloy_primitives::B256;
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_engine::{ForkchoiceState, JwtSecret, PayloadStatus, PayloadStatusEnum};
use alloy_rpc_types_eth::BlockNumberOrTag;
use base_common_network::{Base, BaseEngineApi};
use base_common_rpc_types_engine::{BaseExecutionPayload, BasePayloadAttributes, ExecutionData};
use base_consensus_engine::BaseEngineClient;
use eyre::{Result, WrapErr, ensure};
use serde::{Deserialize, Serialize};

use crate::{InProcessClient, LocalBenchmarkArgs};

/// File the recorded replay result is written to inside `--output-dir`.
pub const RECORDED_REPLAY_RESULT_FILE: &str = "recorded-replay-result.json";

/// One engine API call in the format reth's `EngineMessageStore` writes for Base payload types.
///
/// Mirrors reth's `StoredEngineApiMessage` with the RPC attributes type: the stored type decodes
/// attributes into builder attributes, which would not send them back verbatim.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RecordedEngineMessage {
    /// An `engine_forkchoiceUpdated` call.
    ForkchoiceUpdated {
        /// Recorded forkchoice state.
        state: ForkchoiceState,
        /// Recorded payload attributes, if the call started a build.
        payload_attrs: Option<Box<BasePayloadAttributes>>,
    },
    /// An `engine_newPayload` call.
    NewPayload {
        /// Recorded payload and sidecar.
        #[serde(flatten)]
        payload: Box<ExecutionData>,
    },
}

/// Status the node returned for one replayed message.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedMessageOutcome {
    /// Recorded file name, `<millis>-<fcu|new_payload>-<block hash>.json`.
    pub file: String,
    /// Status returned by the node.
    pub status: PayloadStatus,
    /// Engine call round trip in milliseconds.
    pub latency_ms: f64,
}

/// Machine-readable outcome of one recorded replay, written to `recorded-replay-result.json`.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedReplayResult {
    /// Node head before the replay; replay starts at the first payload built on it.
    pub baseline_hash: B256,
    /// Recorded messages before the replay start.
    pub messages_skipped: u64,
    /// Head of the last replayed forkchoice update.
    pub expected_head: Option<B256>,
    /// Node head after the replay.
    pub final_hash: B256,
    /// Whether the node ended on `expected_head`.
    #[serde(rename = "match")]
    pub matches: bool,
    /// Error that stopped the replay early, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Per-message outcomes in replay order.
    pub messages: Vec<RecordedMessageOutcome>,
}

/// Replays an engine API recording into a running node.
#[derive(Debug)]
pub struct RecordedReplay;

impl RecordedReplay {
    /// Replays `store_dir` into `client` and writes the result to `output_dir`. Fails on an engine
    /// call error, an `INVALID` status, or a final head other than the last forkchoice update's.
    pub async fn run(
        client: &InProcessClient,
        jwt_secret: JwtSecret,
        store_dir: &Path,
        output_dir: &Path,
    ) -> Result<()> {
        let provider = RootProvider::<Base>::new_http(client.rpc_url()?);
        let engine = BaseEngineClient::<RootProvider, RootProvider<Base>>::rpc_client::<Base>(
            client.engine_url()?,
            jwt_secret,
        )
        .await
        .wrap_err("failed to connect to the replay node's engine API")?;
        let baseline_hash = Self::head(&provider).await?;
        let mut result = RecordedReplayResult { baseline_hash, ..Default::default() };
        result.error = Self::replay(&engine, store_dir, &mut result)
            .await
            .err()
            .map(|error| format!("{error:#}"));
        // After a rejected message the node may not answer; keep the result with an unknown head.
        result.final_hash = match Self::head(&provider).await {
            Err(error) if result.error.is_none() => return Err(error),
            head => head.unwrap_or_default(),
        };
        result.matches = result.expected_head == Some(result.final_hash);

        let result_path = output_dir.join(RECORDED_REPLAY_RESULT_FILE);
        LocalBenchmarkArgs::write_json(&result_path, &result, "recorded replay result")?;
        println!("recorded replay result: {}", result_path.display());
        if let Some(error) = &result.error {
            eyre::bail!(
                "recorded replay stopped after {} messages: {error}",
                result.messages.len()
            );
        }
        ensure!(
            result.matches,
            "recorded replay ended at {}, expected {:?}",
            result.final_hash,
            result.expected_head
        );
        Ok(())
    }

    /// Sends each recorded message from the first payload built on the baseline head, stopping at
    /// the first call error or `INVALID` status.
    async fn replay(
        engine: &RootProvider<Base>,
        store_dir: &Path,
        result: &mut RecordedReplayResult,
    ) -> Result<()> {
        let mut started = false;
        for path in Self::message_files(store_dir)? {
            let message = Self::read(&path)?;
            started |= matches!(&message, RecordedEngineMessage::NewPayload { payload }
                if payload.payload.as_v1().parent_hash == result.baseline_hash);
            if !started {
                result.messages_skipped += 1;
                continue;
            }
            let file = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let sent = Instant::now();
            let status = match message {
                RecordedEngineMessage::ForkchoiceUpdated { state, payload_attrs } => {
                    result.expected_head = Some(state.head_block_hash);
                    engine
                        .fork_choice_updated_v3(state, payload_attrs.map(|attrs| *attrs))
                        .await
                        .map(|u| u.payload_status)
                }
                RecordedEngineMessage::NewPayload { payload } => {
                    let parent_beacon_block_root =
                        payload.sidecar.parent_beacon_block_root().unwrap_or_default();
                    match payload.payload {
                        BaseExecutionPayload::V3(payload) => {
                            engine.new_payload_v3(payload, parent_beacon_block_root).await
                        }
                        BaseExecutionPayload::V4(payload) => {
                            engine.new_payload_v4(payload, parent_beacon_block_root).await
                        }
                        _ => eyre::bail!("{file} holds a pre-Ecotone payload"),
                    }
                }
            }
            .wrap_err_with(|| format!("engine call failed for {file}"))?;
            let invalid = matches!(status.status, PayloadStatusEnum::Invalid { .. });
            result.messages.push(RecordedMessageOutcome {
                file,
                status,
                latency_ms: sent.elapsed().as_secs_f64() * 1_000.0,
            });
            ensure!(!invalid, "node rejected a recorded message as INVALID");
        }
        ensure!(
            started,
            "recording holds no payload built on the baseline head {}",
            result.baseline_hash
        );
        Ok(())
    }

    /// Recorded message files in recorded order. Files often share a millisecond; within one,
    /// payloads go first, by block number, because a payload needs only its parent inserted,
    /// while a forkchoice update may need a payload from the same millisecond. Remaining ties,
    /// which file names cannot order, fall back to the path so replays are reproducible.
    pub fn message_files(store_dir: &Path) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for entry in fs::read_dir(store_dir)
            .wrap_err_with(|| format!("failed to read engine store {}", store_dir.display()))?
        {
            let path = entry?.path();
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            let Some((millis, _)) = name.strip_suffix(".json").and_then(|n| n.split_once('-'))
            else {
                continue;
            };
            let millis: u64 = millis
                .parse()
                .wrap_err_with(|| format!("engine store file {name} has no timestamp prefix"))?;
            let number = match Self::read(&path)? {
                RecordedEngineMessage::NewPayload { payload } => {
                    Some(payload.payload.as_v1().block_number)
                }
                RecordedEngineMessage::ForkchoiceUpdated { .. } => None,
            };
            files.push(((millis, number.is_none(), number), path));
        }
        files.sort();
        Ok(files.into_iter().map(|(_, path)| path).collect())
    }

    fn read(path: &Path) -> Result<RecordedEngineMessage> {
        serde_json::from_slice(&fs::read(path)?)
            .wrap_err_with(|| format!("failed to decode {}", path.display()))
    }

    async fn head(provider: &RootProvider<Base>) -> Result<B256> {
        let block = provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await?
            .ok_or_else(|| eyre::eyre!("replay node has no latest block"))?;
        Ok(block.header.hash)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_primitives::Bytes;
    use alloy_rpc_types_engine::JwtSecret;
    use base_execution_chainspec::BaseChainSpec;
    use base_execution_txpool::ValiditySignatureMode;
    use base_node_runner::test_utils::{TestHarness, load_chain_spec};

    use super::RecordedReplay;
    use crate::{
        ChainSpecSource, InProcessClient, InProcessClientConfig, InProcessNodeRuntime,
        replay_cli::{UNREACHABLE_PEER_ENODE, UNUSED_SEQUENCER_URL},
    };

    #[tokio::test(flavor = "multi_thread")]
    async fn replaying_a_recorded_devnet_session_reproduces_its_head() -> eyre::Result<()> {
        let store = tempfile::tempdir()?;
        let output = tempfile::tempdir()?;
        // Jovian base fee parameters (chain defaults, 1 gwei minimum). Without them a node rejects
        // block 1; the recorder accepts it only because it built it.
        let mut genesis = load_chain_spec().inner.genesis.clone();
        genesis.extra_data =
            Bytes::from_static(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x3b, 0x9a, 0xca, 0x00]);
        let chain_spec = Arc::new(BaseChainSpec::from_genesis(genesis));
        let recorder = TestHarness::builder()
            .with_chain_spec(Arc::clone(&chain_spec))
            .with_engine_api_store(store.path().to_path_buf())
            .build()
            .await?;
        recorder.advance_chain(5).await?;
        let recorded_head = recorder.latest_block().hash();
        drop(recorder);
        // Move every payload into the first payload's millisecond and name them so path order is
        // child first; they replay in order only if ties are broken by block number.
        let mut payloads = Vec::new();
        for entry in std::fs::read_dir(store.path())? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if let Some((millis, hash)) = name.split_once("-new_payload-") {
                payloads.push((millis.to_owned(), hash.to_owned()));
            }
        }
        payloads.sort();
        let first = payloads[0].0.clone();
        let count = payloads.len();
        for (index, (millis, hash)) in payloads.into_iter().enumerate() {
            std::fs::rename(
                store.path().join(format!("{millis}-new_payload-{hash}")),
                store.path().join(format!("{first}-new_payload-{}.json", count - index)),
            )?;
        }

        let jwt_secret = JwtSecret::random();
        let replayer = InProcessClient::start(InProcessClientConfig {
            runtime: InProcessNodeRuntime::Host,
            chain_spec: ChainSpecSource::Parsed(chain_spec),
            datadir: None,
            jwt_secret,
            builder_rpc_url: UNUSED_SEQUENCER_URL.to_string(),
            builder_p2p_enode: UNREACHABLE_PEER_ENODE.to_string(),
            http_port: None,
            ws_port: None,
            auth_port: None,
            p2p_port: None,
            metrics_port: None,
            persistence_threshold: Some(0),
            persistence_backpressure_threshold: None,
            // Matches the replay CLI default.
            state_root_task_compare_updates: true,
            tx_forwarding_config: None,
            validity_signature_mode: ValiditySignatureMode::Off,
            upgrade_signal: None,
            extra_extensions: Vec::new(),
        })
        .await?;

        RecordedReplay::run(&replayer, jwt_secret, store.path(), output.path()).await?;

        let result: serde_json::Value = serde_json::from_slice(&std::fs::read(
            output.path().join(super::RECORDED_REPLAY_RESULT_FILE),
        )?)?;
        assert_eq!(result["finalHash"], serde_json::json!(recorded_head));
        replayer.shutdown().await
    }
}
