//! End-to-end Glamsterdam protocol assertions over an externally provisioned devnet.

use std::{fs, path::Path, time::Duration};

use alloy_primitives::{Address, U256};
use alloy_provider::RootProvider;
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use eyre::ensure;
use serde_json::{Value, json};
use tokio::time::Instant;

use super::{Rpc, Schedule, Submissions, Transfer, TransferRequest};
use crate::{CheckResult, EndpointMap, Status};

/// Executes the fixed, evidence-backed Glamsterdam acceptance workflow.
#[derive(Debug)]
pub struct GlamsterdamCheck;

impl GlamsterdamCheck {
    /// Runs every assertion in order and leaves stable blocked rows after the first failure.
    pub async fn run(
        id: &str,
        endpoints: &EndpointMap,
        output: &Path,
        within: Duration,
        results: &mut [CheckResult],
    ) {
        let names = [
            "schedule",
            "pre-transfer",
            "pre-batch",
            "pre-safe",
            "boundary",
            "post-transfer",
            "post-batch",
            "post-safe",
            "l2-rules",
            "finality",
            "canonical",
        ];
        debug_assert_eq!(results.len(), names.len());
        for (result, name) in results.iter_mut().zip(names) {
            *result = Self::result(
                id,
                name,
                Status::Blocked,
                json!({}),
                "prerequisite not reached",
                Duration::ZERO,
            );
        }
        let started = Instant::now();
        let execution =
            tokio::time::timeout(within, Self::execute(endpoints, output, results, id)).await;
        let failure = match execution {
            Ok(Ok(())) => None,
            Ok(Err(failure)) => Some(failure),
            Err(error) => Some((
                results.iter().position(|result| result.status == Status::Blocked).unwrap_or(10),
                error.into(),
            )),
        };
        if let Some((index, error)) = failure {
            let status = if error.downcast_ref::<reqwest::Error>().is_some()
                || error.downcast_ref::<std::io::Error>().is_some()
                || error.downcast_ref::<tokio::time::error::Elapsed>().is_some()
            {
                Status::Error
            } else {
                Status::Failed
            };
            results[index] = Self::result(
                id,
                names[index],
                status,
                json!({ "elapsed_ms": started.elapsed().as_millis() }),
                &error.to_string(),
                started.elapsed().saturating_sub(Duration::from_millis(
                    results[..index].iter().map(|result| result.duration_ms).sum(),
                )),
            );
        }
    }

    /// Executes protocol stages, recording each completed stage directly in caller-owned results.
    pub async fn execute(
        endpoints: &EndpointMap,
        output: &Path,
        results: &mut [CheckResult],
        id: &str,
    ) -> std::result::Result<(), (usize, eyre::Report)> {
        let operation = async {
            let mut stage_started = Instant::now();
            let endpoint = |name: &str| {
                endpoints
                    .get(name)
                    .map(String::as_str)
                    .ok_or_else(|| eyre::eyre!("missing {name} endpoint"))
            };
            let l1 = endpoint("l1")?;
            let beacon = endpoint("beacon")?;
            let sequencer_url = endpoint("builder")?;
            let verifier_url = endpoint("validator")?;
            let sequencer_cl = endpoint("builder-consensus")?;
            let verifier_cl = endpoint("validator-consensus")?;
            let rpc = Rpc::new()?;
            let sequencer = RootProvider::<Base>::new_http(sequencer_url.parse()?);
            let rollup: RollupConfig = serde_json::from_slice(&fs::read(
                output.join("private/devnet/l2/configs/rollup.json"),
            )?)?;
            let genesis: Value = serde_json::from_slice(&fs::read(
                output.join("private/devnet/l1/configs/el/genesis.json"),
            )?)?;
            let l2_genesis: Value = serde_json::from_slice(&fs::read(
                output.join("private/devnet/l2/configs/genesis.json"),
            )?)?;

            let schedule = Schedule::read(&rpc, beacon, &genesis).await?;
            let initial = rpc.header(l1, json!("latest")).await?;
            ensure!(
                initial.header.timestamp < schedule.activation,
                "setup missed pre-fork L1 window"
            );
            initial.without_amsterdam()?;
            ensure!(
                l2_genesis["config"]["amsterdamTime"].is_null(),
                "L2 Amsterdam must not be configured"
            );
            ensure!(rollup.upgrades.base.zenith.is_none(), "L2 Zenith must remain unscheduled");
            results[0] = Self::passed(
                id,
                "schedule",
                json!({
                    "activation": schedule.activation,
                    "genesis": schedule.genesis,
                }),
                stage_started.elapsed(),
            );
            stage_started = Instant::now();

            let mut batches = Submissions::new(rollup.genesis.l1.number, beacon.into(), &schedule);
            let pre = Transfer::send(
                &rpc,
                &sequencer,
                sequencer_url,
                l1,
                &rollup,
                TransferRequest { recipient: Address::repeat_byte(0x73), value: 1_337 },
                Duration::from_secs(90),
            )
            .await?;
            ensure!(
                pre.l1_origin.header.timestamp < schedule.activation,
                "pre-transfer used post-fork origin"
            );
            pre.block.without_amsterdam()?;
            results[1] = Self::passed(
                id,
                "pre-transfer",
                Self::transfer_evidence(&pre),
                stage_started.elapsed(),
            );
            stage_started = Instant::now();

            let pre_batch = batches.wait_for_transfer(&rpc, l1, &rollup, &pre).await?;
            ensure!(
                !pre_batch.submissions.is_empty()
                    && pre_batch
                        .submissions
                        .iter()
                        .all(|submission| submission.block.timestamp < schedule.activation),
                "pre-transfer channel crossed activation"
            );
            results[2] = Self::passed(id, "pre-batch", json!(pre_batch), stage_started.elapsed());
            stage_started = Instant::now();

            let pre_safe = pre
                .wait_until_safe(&rpc, [sequencer_cl, verifier_cl], sequencer_url, verifier_url)
                .await?;
            ensure!(
                rpc.header(l1, json!("latest")).await?.header.timestamp < schedule.activation,
                "pre-transfer was not safe before activation"
            );
            results[3] = Self::passed(id, "pre-safe", pre_safe, stage_started.elapsed());
            stage_started = Instant::now();

            let (before, after) = schedule.wait_for_boundary(&rpc, l1, initial).await?;
            let l1_slotnum = rpc.slotnum(l1, after.hash).await?;
            ensure!(l1_slotnum.get("error").is_none(), "post-Amsterdam L1 SLOTNUM failed");
            let slotnum: U256 = serde_json::from_value(l1_slotnum["result"].clone())?;
            ensure!(
                Some(slotnum.to::<u64>()) == after.header.slot_number,
                "L1 SLOTNUM/header mismatch"
            );
            results[4] = Self::passed(
                id,
                "boundary",
                json!({
                    "before": before.hash,
                    "after": after.hash,
                    "slot": slotnum,
                }),
                stage_started.elapsed(),
            );
            stage_started = Instant::now();

            rpc.wait_for_l1_origin(
                sequencer_cl,
                l1,
                after.header.number,
                after.hash,
                Duration::from_secs(60),
            )
            .await?;
            let post = Transfer::send(
                &rpc,
                &sequencer,
                sequencer_url,
                l1,
                &rollup,
                TransferRequest { recipient: Address::repeat_byte(0x74), value: 2_003 },
                Duration::from_secs(90),
            )
            .await?;
            ensure!(
                post.l1_origin.header.timestamp >= schedule.activation,
                "post-transfer retained pre-fork origin"
            );
            post.block.without_amsterdam()?;
            results[5] = Self::passed(
                id,
                "post-transfer",
                Self::transfer_evidence(&post),
                stage_started.elapsed(),
            );
            stage_started = Instant::now();

            let post_batch = batches.wait_for_transfer(&rpc, l1, &rollup, &post).await?;
            ensure!(
                !post_batch.submissions.is_empty()
                    && post_batch
                        .submissions
                        .iter()
                        .all(|submission| submission.block.timestamp >= schedule.activation),
                "post-transfer channel contains pre-fork submissions"
            );
            results[6] = Self::passed(id, "post-batch", json!(post_batch), stage_started.elapsed());
            stage_started = Instant::now();
            results[7] = Self::passed(
                id,
                "post-safe",
                post.wait_until_safe(
                    &rpc,
                    [sequencer_cl, verifier_cl],
                    sequencer_url,
                    verifier_url,
                )
                .await?,
                stage_started.elapsed(),
            );
            stage_started = Instant::now();

            for url in [sequencer_url, verifier_url] {
                for transfer in [&pre, &post] {
                    Rpc::require_inactive_slotnum(&rpc.slotnum(url, transfer.block.hash).await?)?;
                    let header = rpc
                        .header(url, json!(format!("{:#x}", transfer.block.header.number)))
                        .await?;
                    ensure!(header.hash == transfer.block.hash, "L2 transfer block changed");
                    header.without_amsterdam()?;
                }
            }
            results[8] = Self::passed(
                id,
                "l2-rules",
                json!({
                    "slotnum": "inactive",
                    "amsterdam_fields": "absent",
                    "zenith": "unscheduled",
                }),
                stage_started.elapsed(),
            );
            stage_started = Instant::now();

            results[9] = Self::passed(
                id,
                "finality",
                schedule.wait_for_finality(&rpc, l1, beacon).await?,
                stage_started.elapsed(),
            );
            stage_started = Instant::now();
            let required = pre_batch
                .submissions
                .iter()
                .chain(&post_batch.submissions)
                .map(|s| s.block.number)
                .chain([after.header.number])
                .max()
                .unwrap();
            let finalized = schedule
                .wait_for_finalized_height(&rpc, l1, required, Duration::from_secs(180))
                .await?;
            for boundary in [&before, &after] {
                ensure!(
                    rpc.header(l1, json!(format!("{:#x}", boundary.header.number))).await?.hash
                        == boundary.hash,
                    "finalized chain replaced boundary"
                );
            }
            for attribution in [&pre_batch, &post_batch] {
                Submissions::require_canonical(&rpc, l1, attribution).await?;
            }
            for url in [sequencer_url, verifier_url] {
                for transfer in [&pre, &post] {
                    let header = rpc
                        .header(url, json!(format!("{:#x}", transfer.block.header.number)))
                        .await?;
                    ensure!(header.hash == transfer.block.hash, "final L2 transfer block changed");
                    let receipt = rpc
                        .call(url, "eth_getTransactionReceipt", json!([transfer.transaction_hash]))
                        .await?;
                    ensure!(receipt == transfer.receipt, "final L2 transfer receipt changed");
                }
            }
            results[10] = Self::passed(
                id,
                "canonical",
                json!({
                    "finalized": finalized.hash,
                    "height": finalized.header.number,
                }),
                stage_started.elapsed(),
            );
            Ok::<(), eyre::Report>(())
        };
        match operation.await {
            Ok(()) => Ok(()),
            Err(error) => Err((
                results.iter().position(|result| result.status == Status::Blocked).unwrap_or(10),
                error,
            )),
        }
    }

    /// Returns receipt-anchored evidence for an exact submitted transfer.
    pub fn transfer_evidence(transfer: &Transfer) -> Value {
        json!({
            "transaction": transfer.transaction_hash,
            "signed_transaction": transfer.raw_transaction,
            "receipt": transfer.receipt,
            "block": transfer.block.hash,
            "origin": transfer.l1_origin.hash,
            "balance_before": transfer.balance_before,
            "balance_after": transfer.balance_after,
        })
    }

    /// Builds a successful stage result with its measured duration.
    pub fn passed(id: &str, stage: &str, observed: Value, duration: Duration) -> CheckResult {
        Self::result(id, stage, Status::Passed, observed, "protocol assertion passed", duration)
    }

    /// Builds a protocol stage result without claiming unmeasured samples.
    pub fn result(
        id: &str,
        stage: &str,
        status: Status,
        observed: Value,
        message: &str,
        duration: Duration,
    ) -> CheckResult {
        CheckResult {
            id: format!("{id}-{stage}"),
            kind: "glamsterdam_blob_transfers".into(),
            status,
            duration_ms: duration.as_millis() as u64,
            expected: Self::expected(stage),
            observed,
            message: message.into(),
            next_step: "inspect protocol evidence and bounded service logs".into(),
            samples: 0,
            rpc_errors: 0,
            evidence: Vec::new(),
        }
    }

    /// Returns the concrete predicate evaluated by a protocol stage.
    pub fn expected(stage: &str) -> Value {
        match stage {
            "schedule" => json!({
                "l1": "Amsterdam/Gloas scheduled and inactive",
                "l2": "Amsterdam and Zenith unscheduled",
            }),
            "pre-transfer" => json!({
                "origin": "before activation",
                "amsterdam_fields": "absent",
                "value_transfer": "successful",
            }),
            "pre-batch" => json!({
                "channel": "complete and attributed",
                "submissions": "non-empty and before activation",
            }),
            "pre-safe" => json!({ "transfer": "safe on builder and validator before activation" }),
            "boundary" => json!({
                "headers": "adjacent authenticated fork boundary",
                "slotnum": "matches header",
            }),
            "post-transfer" => json!({
                "origin": "at or after activation",
                "amsterdam_fields": "absent",
                "value_transfer": "successful",
            }),
            "post-batch" => json!({
                "channel": "complete and attributed",
                "submissions": "non-empty and at or after activation",
            }),
            "post-safe" => json!({ "transfer": "safe on builder and validator" }),
            "l2-rules" => json!({
                "slotnum": "inactive",
                "amsterdam_fields": "absent",
                "zenith": "unscheduled",
            }),
            "finality" => json!({ "gloas": "active", "execution_payload": "finalized" }),
            "canonical" => json!({
                "l1_batches_and_boundary": "canonical",
                "l2_transfer_receipts_and_blocks": "canonical on both nodes",
            }),
            _ => json!({ "assertion": stage }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, time::Duration};

    use serde_json::json;

    use super::GlamsterdamCheck;
    use crate::{CheckResult, EndpointMap, Status};

    fn placeholder() -> CheckResult {
        CheckResult {
            id: "placeholder".into(),
            kind: "placeholder".into(),
            status: Status::Passed,
            duration_ms: 42,
            expected: json!({}),
            observed: json!({}),
            message: String::new(),
            next_step: String::new(),
            samples: 9,
            rpc_errors: 0,
            evidence: Vec::new(),
        }
    }

    #[tokio::test]
    async fn timeout_retains_updates_in_caller_owned_results() {
        let mut results = vec![placeholder(); 11];
        GlamsterdamCheck::run(
            "fork",
            &EndpointMap::new(),
            Path::new("unused"),
            Duration::ZERO,
            &mut results,
        )
        .await;

        assert_eq!(results[0].id, "fork-schedule");
        assert_ne!(results[0].status, Status::Blocked);
        assert!(results[1..].iter().all(|result| result.status == Status::Blocked));
        assert!(results.iter().all(|result| result.samples == 0));
        assert_eq!(
            results[10].expected["l2_transfer_receipts_and_blocks"],
            "canonical on both nodes"
        );
    }

    #[test]
    fn successful_result_reports_measured_duration_and_actionable_predicate() {
        let result = GlamsterdamCheck::passed(
            "fork",
            "pre-batch",
            json!({ "channel": "0x01" }),
            Duration::from_millis(17),
        );

        assert_eq!(result.duration_ms, 17);
        assert_eq!(result.samples, 0);
        assert_eq!(result.expected["channel"], "complete and attributed");
    }
}
