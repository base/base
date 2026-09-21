//! Runtime observations for deployed builder and validator nodes.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_consensus::SignableTransaction;
use alloy_eips::{BlockNumberOrTag, eip2718::Encodable2718};
use alloy_network::{ReceiptResponse, TransactionBuilder};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::Provider;
use alloy_signer::SignerSync;
use base_common_genesis::RollupConfig;
use base_common_rpc_types::BaseTransactionRequest;
use base_protocol::SyncStatus;
use eyre::{Result, WrapErr, bail, ensure};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::{sleep, timeout, timeout_at};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::{ForkActivation, Provisioner, ScenarioConfig, WorkloadContext};

const POLL: Duration = Duration::from_millis(100);
const REPLAY_QUIET: Duration = Duration::from_secs(2);
const MAX_GOSSIP_BLOCK_AGE: u64 = 60;
const RECIPIENT: Address = Address::repeat_byte(0xfe);

/// Runtime behavior exercised against the real Compose-deployed node binaries.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeCase {
    /// Requires the validator's Flashblocks-derived pending head to track the builder.
    PendingFlashblocks,
    /// Requires builder and validator canonical heads to converge by hash.
    BuilderClientSync,
    /// Ports the Denim-only builder cutover system test.
    BuilderCutover,
    /// Ports the Denim cutover followed by Zenith system test.
    BuilderCutoverZenith,
    /// Proves post-Isthmus V4 gossip after the legacy topics retire.
    GossipTopicRetirement,
}

impl RuntimeCase {
    /// Validates topology and fork prerequisites without contacting the devnet.
    pub fn validate(&self, config: &ScenarioConfig) -> Result<()> {
        let denim = config.devnet.l2.forks.get("denim").and_then(ForkActivation::block);
        if matches!(self, Self::PendingFlashblocks) {
            ensure!(denim.is_none(), "pending_flashblocks requires Denim to be disabled");
        }
        if matches!(self, Self::BuilderCutover | Self::BuilderCutoverZenith) {
            ensure!(
                denim.is_some_and(|block| block >= 20),
                "builder cutover requires Denim at block 20 or later"
            );
        }
        let zenith = config.devnet.l2.forks.get("zenith").and_then(ForkActivation::block);
        match self {
            Self::BuilderCutover => {
                ensure!(zenith.is_none(), "Denim-only cutover requires Zenith disabled")
            }
            Self::BuilderCutoverZenith => ensure!(
                zenith.is_some_and(|block| block > denim.unwrap_or(0)),
                "Zenith cutover requires Zenith after Denim"
            ),
            _ => {}
        }
        Ok(())
    }

    /// Returns endpoint roles required by this workload, including observation-only roles.
    pub const fn required_roles(&self) -> &'static [&'static str] {
        match self {
            Self::PendingFlashblocks | Self::BuilderClientSync => &["builder", "validator"],
            Self::BuilderCutover => &["builder", "validator", "builder-flashblocks"],
            Self::BuilderCutoverZenith => {
                &["builder", "validator", "builder-flashblocks", "builder-metrics"]
            }
            Self::GossipTopicRetirement => {
                &["builder", "validator", "builder-consensus", "validator-consensus"]
            }
        }
    }

    /// Executes the bounded observation and returns machine-readable evidence.
    pub async fn execute(
        &self,
        context: &WorkloadContext<'_>,
        provisioner: &Provisioner,
    ) -> Result<Value> {
        self.validate(context.config)?;
        match self {
            Self::PendingFlashblocks => Self::pending_flashblocks(context).await,
            Self::BuilderClientSync => Self::builder_client_sync(context).await,
            Self::BuilderCutover => Self::builder_cutover(context, false).await,
            Self::BuilderCutoverZenith => Self::builder_cutover(context, true).await,
            Self::GossipTopicRetirement => {
                Self::gossip_topic_retirement(context, provisioner).await
            }
        }
    }

    /// Retires old gossip topics and proves the validator still imports unbatched unsafe blocks.
    pub async fn gossip_topic_retirement(
        context: &WorkloadContext<'_>,
        provisioner: &Provisioner,
    ) -> Result<Value> {
        let builder_cl = context.endpoint("builder-consensus")?;
        let validator_cl = context.endpoint("validator-consensus")?;
        let rollup: RollupConfig = serde_json::from_value(
            context.rpc.call(builder_cl, "optimism_rollupConfig", json!([])).await?,
        )?;
        let isthmus = rollup
            .upgrades
            .isthmus_time
            .ok_or_else(|| eyre::eyre!("rollup config is missing Isthmus activation"))?;

        timeout_at(context.deadline, async {
            loop {
                let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
                if now >= isthmus + MAX_GOSSIP_BLOCK_AGE
                    && Self::all_topics_retired(context, [builder_cl, validator_cl]).await?
                {
                    return Ok::<_, eyre::Error>(());
                }
                sleep(Duration::from_millis(250)).await;
            }
        })
        .await
        .wrap_err("nodes did not retain V4 peering after old topics retired")??;

        let unsafe_head = context.rpc.call(builder_cl, "admin_stopSequencer", json!([])).await?;
        provisioner.stop_batchers().await?;
        let builder = context.provider("builder")?;
        let validator = context.provider("validator")?;
        let target = builder.get_block_number().await? + 5;
        context.rpc.call(builder_cl, "admin_startSequencer", json!([unsafe_head])).await?;

        timeout_at(context.deadline, async {
            loop {
                let status = context
                    .rpc
                    .call(validator_cl, "optimism_syncStatus", json!([]))
                    .await?;
                let (unsafe_number, safe_number) = Self::sync_numbers(&status)?;
                if unsafe_number >= target {
                    ensure!(safe_number < target, "target block arrived through safe derivation");
                    let tag = BlockNumberOrTag::Number(target);
                    let expected = builder
                        .get_block_by_number(tag)
                        .await?
                        .ok_or_else(|| eyre::eyre!("builder target block missing"))?;
                    let received = validator
                        .get_block_by_number(tag)
                        .await?
                        .ok_or_else(|| eyre::eyre!("validator target block missing"))?;
                    ensure!(received.header.hash == expected.header.hash, "target hash mismatch");
                    ensure!(
                        Self::all_topics_retired(context, [builder_cl, validator_cl]).await?,
                        "peer topics regressed after unsafe propagation"
                    );
                    return Ok(json!({"target": target, "hash": expected.header.hash, "validator_unsafe": unsafe_number, "validator_safe": safe_number}));
                }
                sleep(Duration::from_millis(250)).await;
            }
        })
        .await
        .wrap_err("validator stopped importing unsafe blocks after topic retirement")?
    }

    /// Checks legacy topic retirement on both consensus RPC endpoints.
    pub async fn all_topics_retired(
        context: &WorkloadContext<'_>,
        roles: [&str; 2],
    ) -> Result<bool> {
        for role in roles {
            let stats = context.rpc.call(role, "opp2p_peerStats", json!([])).await?;
            if !Self::topics_retired(&stats)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Requires connected V4 peers with no legacy-topic peers.
    pub fn topics_retired(stats: &Value) -> Result<bool> {
        let field = |name| {
            stats
                .get(name)
                .and_then(Value::as_u64)
                .ok_or_else(|| eyre::eyre!("peer stats missing {name}"))
        };
        Ok(field("connected")? > 0
            && field("blocksTopic")? == 0
            && field("blocksTopicV2")? == 0
            && field("blocksTopicV3")? == 0
            && field("blocksTopicV4")? > 0)
    }

    /// Reads unsafe and safe heights using the consensus RPC wire schema.
    pub fn sync_numbers(status: &Value) -> Result<(u64, u64)> {
        let status: SyncStatus = serde_json::from_value(status.clone())?;
        Ok((status.unsafe_l2.block_info.number, status.safe_l2.block_info.number))
    }

    /// Performs the source test's bounded ten-poll, three-consecutive pending observation.
    pub async fn pending_flashblocks(context: &WorkloadContext<'_>) -> Result<Value> {
        let builder = context.provider("builder")?;
        let validator = context.provider("validator")?;
        let mut consecutive = 0_u64;
        for observations in 1..=10_u64 {
            let builder_head = builder
                .get_block_by_number(BlockNumberOrTag::Pending)
                .await?
                .ok_or_else(|| eyre::eyre!("builder pending block is absent"))?
                .header
                .number;
            let validator_head = validator
                .get_block_by_number(BlockNumberOrTag::Pending)
                .await?
                .ok_or_else(|| eyre::eyre!("validator pending block is absent"))?
                .header
                .number;
            consecutive =
                if builder_head.abs_diff(validator_head) <= 1 { consecutive + 1 } else { 0 };
            if consecutive == 3 {
                return Ok(
                    json!({"observations": observations, "consecutive_matches": 3, "builder_pending": builder_head, "validator_pending": validator_head}),
                );
            }
            sleep(Duration::from_millis(250)).await;
        }
        bail!("pending Flashblocks did not match for three consecutive observations")
    }

    /// Waits for both canonical chains and compares an actual common block hash.
    pub async fn builder_client_sync(context: &WorkloadContext<'_>) -> Result<Value> {
        let builder = context.provider("builder")?;
        let validator = context.provider("validator")?;
        timeout_at(context.deadline, async {
            loop {
                let builder_head = builder.get_block_number().await?;
                let validator_head = validator.get_block_number().await?;
                let number = builder_head.min(validator_head);
                if number >= 3 {
                    let tag = BlockNumberOrTag::Number(number);
                    let a = builder.get_block_by_number(tag).await?;
                    let b = validator.get_block_by_number(tag).await?;
                    if let (Some(a), Some(b)) = (a, b) && a.header.hash == b.header.hash {
                        return Ok(json!({"compared_block": number, "hash": a.header.hash, "builder_head": builder_head, "validator_head": validator_head}));
                    }
                }
                sleep(Duration::from_millis(250)).await;
            }
        }).await.wrap_err("builder/client convergence deadline elapsed")?
    }

    /// Executes transaction, cadence, chain, replay, and optional Zenith metric contracts.
    pub async fn builder_cutover(
        context: &WorkloadContext<'_>,
        with_zenith: bool,
    ) -> Result<Value> {
        let denim = context.config.devnet.l2.forks["denim"]
            .block()
            .ok_or_else(|| eyre::eyre!("Denim disabled"))?;
        let builder = context.provider("builder")?;
        ensure!(builder.get_block_number().await? < denim, "Denim pre-window was missed");
        let pre_receipt = Self::send_transaction(context).await?;
        ensure!(pre_receipt < denim, "pre-cutover transaction landed at block {pre_receipt}");
        Self::wait_for_block(context, "builder", denim + 1).await?;
        let post_receipt = Self::send_transaction(context).await?;
        ensure!(post_receipt > denim, "post-cutover transaction did not land after Denim");

        let mut last = post_receipt + 4;
        let mut metrics = Value::Null;
        if with_zenith {
            let before = reqwest::get(context.endpoint("builder-metrics")?).await?.text().await?;
            let flash_before = Self::selected_build_count(&before, "flashblocks")?;
            let basic_before = Self::selected_build_count(&before, "basic")?;
            ensure!(
                flash_before > 0 && basic_before > 0,
                "builder selections missing across Denim"
            );
            let zenith = Self::fork_timestamp(context, "zenith")?;
            let active_head = Self::wait_for_timestamp(context, zenith).await?;
            let zenith_receipt = Self::send_transaction(context).await?;
            ensure!(
                zenith_receipt > active_head,
                "post-Zenith transaction did not land after activation"
            );
            last = zenith_receipt + 4;
            Self::wait_for_block(context, "builder", last).await?;
            let after = reqwest::get(context.endpoint("builder-metrics")?).await?.text().await?;
            let flash_after = Self::selected_build_count(&after, "flashblocks")?;
            let basic_after = Self::selected_build_count(&after, "basic")?;
            ensure!(flash_after == flash_before, "Zenith restarted Flashblocks builder selection");
            ensure!(basic_after > basic_before, "basic builder did not continue after Zenith");
            metrics = json!({"flashblocks_before": flash_before, "flashblocks_after": flash_after, "basic_before": basic_before, "basic_after": basic_after});
        }
        Self::wait_for_block(context, "builder", last).await?;
        Self::wait_for_block(context, "validator", last).await?;
        Self::verify_chain_and_cadence(context, denim, last).await?;
        let replay =
            Self::verify_flashblocks_stop(context.endpoint("builder-flashblocks")?, denim).await?;
        Ok(
            json!({"denim_block": denim, "pre_receipt_block": pre_receipt, "post_receipt_block": post_receipt, "last_verified_block": last, "replay_positions": replay, "zenith_metrics": metrics}),
        )
    }

    /// Sends a signed transfer and returns its successful receipt block.
    pub async fn send_transaction(context: &WorkloadContext<'_>) -> Result<u64> {
        let provider = context.provider("builder")?;
        let signer = WorkloadContext::signer(0)?;
        let nonce = provider.get_transaction_count(signer.address()).await?;
        let tx = BaseTransactionRequest::default()
            .from(signer.address())
            .to(RECIPIENT)
            .value(U256::from(1))
            .transaction_type(2)
            .with_gas_limit(21_000)
            .with_max_fee_per_gas(2_000_000_000)
            .with_max_priority_fee_per_gas(1_000_000)
            .with_chain_id(context.config.devnet.l2.chain_id)
            .with_nonce(nonce)
            .build_typed_tx()
            .map_err(|error| eyre::eyre!("invalid transaction: {error:?}"))?;
        let signature = signer.sign_hash_sync(&tx.signature_hash())?;
        let raw: Bytes = tx.into_signed(signature).encoded_2718().into();
        let pending = provider.send_raw_transaction(&raw).await?;
        let hash = *pending.tx_hash();
        drop(pending);
        let receipt = context.receipt("builder", hash).await?;
        ensure!(receipt.status(), "transaction {hash} failed");
        receipt.block_number().ok_or_else(|| eyre::eyre!("receipt missing block number"))
    }

    /// Waits for a role to reach a target block.
    pub async fn wait_for_block(
        context: &WorkloadContext<'_>,
        role: &str,
        target: u64,
    ) -> Result<()> {
        let provider = context.provider(role)?;
        timeout_at(context.deadline, async {
            while provider.get_block_number().await? < target {
                sleep(POLL).await;
            }
            Ok::<_, eyre::Error>(())
        })
        .await
        .wrap_err_with(|| format!("timed out waiting for {role} block {target}"))?
    }

    /// Resolves a generated rollup fork timestamp, rather than assuming block cadence.
    pub fn fork_timestamp(context: &WorkloadContext<'_>, name: &str) -> Result<u64> {
        context
            .config
            .verified_forks(&context.output.join("private/devnet/l2/configs/rollup.json"))?
            .into_iter()
            .find(|fork| fork.name == name)
            .map(|fork| fork.activation_timestamp)
            .ok_or_else(|| eyre::eyre!("generated {name} timestamp missing"))
    }

    /// Waits until the latest builder block reaches a generated timestamp.
    pub async fn wait_for_timestamp(context: &WorkloadContext<'_>, timestamp: u64) -> Result<u64> {
        let provider = context.provider("builder")?;
        timeout_at(context.deadline, async {
            loop {
                let block = provider
                    .get_block_by_number(BlockNumberOrTag::Latest)
                    .await?
                    .ok_or_else(|| eyre::eyre!("builder block missing"))?;
                if block.header.timestamp >= timestamp {
                    return Ok(block.header.number);
                }
                sleep(POLL).await;
            }
        })
        .await
        .wrap_err_with(|| format!("timed out waiting for timestamp {timestamp}"))?
    }

    /// Parses one exact Prometheus builder label, excluding substring label matches.
    pub fn selected_build_count(metrics: &str, builder: &str) -> Result<u64> {
        let expected = format!("builder=\"{builder}\"");
        for line in metrics.lines().filter(|line| !line.starts_with('#')) {
            let Some((series, value)) = line.split_once(char::is_whitespace) else { continue };
            if !series.starts_with("mux_selected_builds_total{") {
                continue;
            }
            let labels = series
                .trim_end_matches('}')
                .split_once('{')
                .map(|(_, labels)| labels)
                .unwrap_or("");
            if labels.split(',').any(|label| label.trim() == expected) {
                return value
                    .trim()
                    .parse()
                    .wrap_err_with(|| format!("invalid selected-build metric for {builder}"));
            }
        }
        bail!("missing selected-build metric for {builder}")
    }

    /// Verifies all canonical hashes, parent links, and 2s/200ms timestamp-ms cadence.
    pub async fn verify_chain_and_cadence(
        context: &WorkloadContext<'_>,
        denim: u64,
        last: u64,
    ) -> Result<()> {
        let builder = context.provider("builder")?;
        let validator = context.provider("validator")?;
        let mut samples = Vec::new();
        for number in 0..=last {
            let a = builder
                .get_block_by_number(BlockNumberOrTag::Number(number))
                .await?
                .ok_or_else(|| eyre::eyre!("builder block {number} missing"))?;
            let b = validator
                .get_block_by_number(BlockNumberOrTag::Number(number))
                .await?
                .ok_or_else(|| eyre::eyre!("validator block {number} missing"))?;
            ensure!(a.header.hash == b.header.hash, "block {number} mismatch");
            samples.push((
                number,
                a.header.hash,
                a.header.parent_hash,
                a.header.timestamp_ms.unwrap_or_else(|| a.header.timestamp.saturating_mul(1_000)),
            ));
        }
        Self::validate_chain_samples(&samples, denim)
    }

    /// Validates collected chain samples; exposed to make mismatch behavior directly testable.
    pub fn validate_chain_samples(samples: &[(u64, B256, B256, u64)], denim: u64) -> Result<()> {
        for pair in samples.windows(2) {
            let previous = pair[0];
            let current = pair[1];
            ensure!(
                current.0 == previous.0 + 1 && current.2 == previous.1,
                "non-contiguous block {}",
                current.0
            );
            let expected = if current.0 <= denim { 2_000 } else { 200 };
            ensure!(
                current.3.checked_sub(previous.3) == Some(expected),
                "block {} cadence mismatch",
                current.0
            );
        }
        Ok(())
    }

    /// Replays Flashblocks and proves immediate pre-Denim evidence with no post-boundary payload.
    pub async fn verify_flashblocks_stop(url: &str, denim: u64) -> Result<Vec<(u64, u64)>> {
        let websocket =
            format!("{}?block_number=0&flashblock_index=0", url.replacen("http://", "ws://", 1));
        let (stream, _) = connect_async(websocket).await?;
        let (_, mut messages) = stream.split();
        let mut positions = Vec::new();
        while let Ok(Some(message)) = timeout(REPLAY_QUIET, messages.next()).await {
            let Message::Text(text) = message? else { continue };
            let value: Value = serde_json::from_str(&text)?;
            let number = value
                .pointer("/metadata/block_number")
                .and_then(Value::as_u64)
                .ok_or_else(|| eyre::eyre!("Flashblocks metadata block number missing"))?;
            let index = value
                .get("index")
                .and_then(Value::as_u64)
                .ok_or_else(|| eyre::eyre!("Flashblocks index missing"))?;
            positions.push((number, index));
        }
        Self::validate_replay_positions(&positions, denim)?;
        Ok(positions)
    }

    /// Validates replay boundary evidence independently from websocket transport.
    pub fn validate_replay_positions(positions: &[(u64, u64)], denim: u64) -> Result<()> {
        ensure!(
            positions.iter().any(|(number, _)| *number == denim - 1),
            "no Flashblocks immediately before Denim"
        );
        ensure!(
            positions.iter().all(|(number, _)| *number < denim),
            "post-Denim Flashblock published"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_cadence_and_parent_mismatches() {
        let one = B256::repeat_byte(1);
        let two = B256::repeat_byte(2);
        assert!(
            RuntimeCase::validate_chain_samples(
                &[(0, one, B256::ZERO, 0), (1, two, one, 1_999)],
                1
            )
            .is_err()
        );
        assert!(
            RuntimeCase::validate_chain_samples(
                &[(0, one, B256::ZERO, 0), (1, two, B256::ZERO, 2_000)],
                1
            )
            .is_err()
        );
    }

    #[test]
    fn metric_label_matching_is_exact() {
        let metrics = "mux_selected_builds_total{builder=\"basic-extra\"} 9\nmux_selected_builds_total{network=\"x\",builder=\"basic\"} 4\n";
        assert_eq!(RuntimeCase::selected_build_count(metrics, "basic").unwrap(), 4);
    }

    #[test]
    fn replay_requires_pre_boundary_and_rejects_post_boundary() {
        assert!(RuntimeCase::validate_replay_positions(&[(24, 0)], 25).is_ok());
        assert!(RuntimeCase::validate_replay_positions(&[(23, 0)], 25).is_err());
        assert!(RuntimeCase::validate_replay_positions(&[(24, 0), (25, 0)], 25).is_err());
    }

    #[test]
    fn gossip_peer_stats_require_only_v4_connections() {
        let retired = json!({
            "connected": 1,
            "blocksTopic": 0,
            "blocksTopicV2": 0,
            "blocksTopicV3": 0,
            "blocksTopicV4": 1
        });
        assert!(RuntimeCase::topics_retired(&retired).unwrap());

        let mut legacy = retired;
        legacy["blocksTopicV3"] = json!(1);
        assert!(!RuntimeCase::topics_retired(&legacy).unwrap());
        assert!(RuntimeCase::topics_retired(&json!({})).is_err());
    }

    #[test]
    fn sync_status_numbers_are_strictly_parsed() {
        let mut status = SyncStatus::default();
        status.unsafe_l2.block_info.number = 10;
        status.safe_l2.block_info.number = 4;
        assert_eq!(RuntimeCase::sync_numbers(&json!(status)).unwrap(), (10, 4));
        assert!(RuntimeCase::sync_numbers(&json!({})).is_err());
    }
}
