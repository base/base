//! Real consensus schedule, execution-boundary, and post-Gloas finality observations.

use std::time::Duration;

use alloy_primitives::B256;
use eyre::{Result, WrapErr, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};

use super::{header::AuthenticatedHeader, rpc::Rpc};

/// EL timestamp and CL epoch schedule read back from generated config and live beacon APIs.
#[derive(Debug, Serialize)]
pub struct Schedule {
    /// Shared execution/consensus genesis time.
    pub genesis: u64,
    /// Configured EL Amsterdam instant.
    pub activation: u64,
    /// Configured CL Gloas epoch.
    pub gloas_epoch: u64,
    /// Configured CL Gloas fork version.
    pub gloas_version: Value,
    /// CL slot duration.
    pub seconds_per_slot: u64,
    /// CL epoch size.
    pub slots_per_epoch: u64,
    /// Unmodified live beacon spec, retained as configuration evidence.
    pub beacon_spec: Value,
}

impl Schedule {
    /// Verifies live CL parameters match the EL genesis schedule before the fork begins.
    pub async fn read(rpc: &Rpc, beacon: &str, el_genesis: &Value) -> Result<Self> {
        let beacon_genesis = rpc.beacon(beacon, "/eth/v1/beacon/genesis").await?;
        let beacon_spec = rpc.beacon(beacon, "/eth/v1/config/spec").await?;
        let genesis = Self::decimal(&beacon_genesis["data"]["genesis_time"])?;
        ensure!(Rpc::quantity(&el_genesis["timestamp"])? == genesis, "EL/CL genesis time mismatch");
        let activation = el_genesis["config"]["amsterdamTime"]
            .as_u64()
            .ok_or_else(|| eyre::eyre!("EL Amsterdam is not scheduled"))?;
        let seconds_per_slot = Self::decimal(&beacon_spec["data"]["SECONDS_PER_SLOT"])?;
        let slots_per_epoch = Self::decimal(&beacon_spec["data"]["SLOTS_PER_EPOCH"])?;
        let gloas_epoch = Self::decimal(&beacon_spec["data"]["GLOAS_FORK_EPOCH"])?;
        ensure!(
            seconds_per_slot > 0 && slots_per_epoch > 0 && gloas_epoch > 0,
            "Gloas must be scheduled after genesis"
        );
        let gloas_version = beacon_spec["data"]["GLOAS_FORK_VERSION"].clone();
        ensure!(gloas_version.as_str().is_some(), "missing CL Gloas fork version");
        let expected = gloas_epoch
            .checked_mul(slots_per_epoch)
            .and_then(|n| n.checked_mul(seconds_per_slot))
            .and_then(|n| n.checked_add(genesis))
            .ok_or_else(|| eyre::eyre!("fork schedule overflow"))?;
        ensure!(
            activation == expected,
            "EL Amsterdam timestamp {activation} != CL Gloas timestamp {expected}"
        );
        let fork = rpc.beacon(beacon, "/eth/v1/beacon/states/head/fork").await?;
        ensure!(
            fork["data"]["current_version"] != gloas_version,
            "setup missed the pre-Gloas window"
        );
        Ok(Self {
            genesis,
            activation,
            gloas_epoch,
            gloas_version,
            seconds_per_slot,
            slots_per_epoch,
            beacon_spec,
        })
    }

    /// Parses the decimal strings mandated by the beacon REST API.
    pub fn decimal(value: &Value) -> Result<u64> {
        value
            .as_str()
            .ok_or_else(|| eyre::eyre!("expected beacon decimal string: {value}"))?
            .parse()
            .wrap_err("invalid beacon decimal")
    }

    /// Walks canonical numbered headers so the authenticated pair is the actual fork boundary.
    pub async fn wait_for_boundary(
        &self,
        rpc: &Rpc,
        l1: &str,
        initial: AuthenticatedHeader,
    ) -> Result<(AuthenticatedHeader, AuthenticatedHeader)> {
        ensure!(initial.header.timestamp < self.activation, "setup missed pre-Amsterdam blocks");
        let mut last = initial;
        let remaining = self.activation.saturating_sub(last.header.timestamp) + 60;
        let result = timeout(Duration::from_secs(remaining), async {
            loop {
                let head = rpc.header(l1, json!("latest")).await?;
                while last.header.number < head.header.number {
                    let next =
                        rpc.header(l1, json!(format!("{:#x}", last.header.number + 1))).await?;
                    ensure!(
                        next.header.parent_hash == last.hash,
                        "L1 canonical linkage changed while waiting for activation"
                    );
                    if next.header.timestamp >= self.activation {
                        AuthenticatedHeader::check_boundary(
                            &last,
                            &next,
                            self.activation,
                            self.genesis,
                            self.seconds_per_slot,
                        )?;
                        return Ok::<_, eyre::Report>((last.clone(), next));
                    }
                    last = next;
                }
                sleep(Duration::from_millis(500)).await;
            }
        })
        .await;
        result.wrap_err_with(|| {
            format!(
                "L1 activation at {} not reached; last block {} hash {} timestamp {}",
                self.activation, last.header.number, last.hash, last.header.timestamp
            )
        })?
    }

    /// Requires a finalized Gloas beacon block and a real finalized post-Amsterdam EL block.
    pub async fn wait_for_finality(&self, rpc: &Rpc, l1: &str, beacon: &str) -> Result<Value> {
        let mut last = Value::Null;
        let result = timeout(Duration::from_secs(120), async {
            loop {
                let fork = rpc.beacon(beacon, "/eth/v1/beacon/states/head/fork").await?;
                let checkpoints = rpc.beacon(beacon, "/eth/v1/beacon/states/head/finality_checkpoints").await?;
                let finalized = rpc.header(l1, json!("finalized")).await?;
                last = json!({"fork": fork, "checkpoints": checkpoints, "el_finalized": finalized});
                if Self::decimal(&checkpoints["data"]["finalized"]["epoch"])? > self.gloas_epoch
                    && finalized.header.timestamp >= self.activation {
                    ensure!(fork["data"]["current_version"] == self.gloas_version && Self::decimal(&fork["data"]["epoch"])? == self.gloas_epoch, "live consensus did not activate expected Gloas fork");
                    ensure!(checkpoints["execution_optimistic"] == false, "beacon finality is execution-optimistic");
                    let root: B256 = serde_json::from_value(checkpoints["data"]["finalized"]["root"].clone())?;
                    ensure!(root != B256::ZERO, "finalized CL root is zero");
                    let block = rpc.beacon(beacon, &format!("/eth/v2/beacon/blocks/{root}")).await?;
                    ensure!(block["version"] == "gloas" && block["execution_optimistic"] == false, "finalized beacon block is not executed Gloas: {block}");
                    let slot = Self::decimal(&block["data"]["message"]["slot"])?;
                    ensure!(slot / self.slots_per_epoch >= self.gloas_epoch, "finalized beacon block predates Gloas");
                    ensure!(finalized.header.block_access_list_hash.is_some() && finalized.header.slot_number.is_some(), "finalized EL block lacks Amsterdam fields");
                    return Ok::<_, eyre::Report>(json!({"fork": fork, "checkpoints": checkpoints, "beacon_block": block, "el_finalized": finalized}));
                }
                sleep(Duration::from_millis(500)).await;
            }
        }).await;
        result.wrap_err_with(|| {
            format!("post-Gloas finality timed out beyond epoch {}; last: {last}", self.gloas_epoch)
        })?
    }
}
