//! Real consensus schedule, execution-boundary, and post-Gloas finality observations.

use std::time::Duration;

use alloy_primitives::B256;
use eyre::{Result, WrapErr, ensure};
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};

use super::{header::AuthenticatedHeader, rpc::Rpc};

/// EL timestamp and CL epoch schedule read back from generated config and live beacon APIs.
#[derive(Debug)]
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
        // Finality is epoch-paced. Allow four real CL epochs, including one epoch of
        // scheduling headroom, rather than assuming the fixture always uses 2s slots.
        let finality_window = self
            .seconds_per_slot
            .checked_mul(self.slots_per_epoch)
            .and_then(|epoch| epoch.checked_mul(4))
            .ok_or_else(|| eyre::eyre!("finality window overflow"))?;
        let mut last = Value::Null;
        let result = timeout(Duration::from_secs(finality_window), async {
            loop {
                let fork = rpc.beacon(beacon, "/eth/v1/beacon/states/head/fork").await?;
                let checkpoints =
                    rpc.beacon(beacon, "/eth/v1/beacon/states/head/finality_checkpoints").await?;
                let finalized = rpc.header(l1, json!("finalized")).await?;
                last = json!({
                    "fork": fork,
                    "checkpoints": checkpoints,
                    "el_finalized": {
                        "hash": finalized.hash,
                        "number": finalized.header.number,
                    },
                });
                if Self::decimal(&checkpoints["data"]["finalized"]["epoch"])? > self.gloas_epoch
                    && finalized.header.timestamp >= self.activation
                {
                    ensure!(
                        fork["data"]["current_version"] == self.gloas_version
                            && Self::decimal(&fork["data"]["epoch"])? == self.gloas_epoch,
                        "live consensus did not activate expected Gloas fork"
                    );
                    ensure!(
                        checkpoints["execution_optimistic"] == false,
                        "beacon finality is execution-optimistic"
                    );
                    let root: B256 =
                        serde_json::from_value(checkpoints["data"]["finalized"]["root"].clone())?;
                    ensure!(root != B256::ZERO, "finalized CL root is zero");
                    let block =
                        rpc.beacon(beacon, &format!("/eth/v2/beacon/blocks/{root}")).await?;
                    let checkpoint_hash = self.execution_checkpoint(&block)?;
                    let checkpoint = AuthenticatedHeader::parse(
                        rpc.call(l1, "eth_getBlockByHash", json!([checkpoint_hash, false])).await?,
                    )?;
                    let canonical =
                        rpc.header(l1, json!(format!("{:#x}", checkpoint.header.number))).await?;
                    ensure!(
                        canonical.hash == checkpoint_hash,
                        "finalized Gloas execution checkpoint is not on the canonical EL chain"
                    );
                    if finalized.header.number < checkpoint.header.number {
                        sleep(Duration::from_millis(500)).await;
                        continue;
                    }
                    ensure!(
                        finalized.header.block_access_list_hash.is_some()
                            && finalized.header.slot_number.is_some(),
                        "finalized EL block lacks Amsterdam fields"
                    );
                    return Ok::<_, eyre::Report>(json!({
                        "fork": fork,
                        "checkpoints": checkpoints,
                        "beacon_block": block,
                        "el_finalized": {
                            "hash": finalized.hash,
                            "number": finalized.header.number,
                        },
                    }));
                }
                sleep(Duration::from_millis(500)).await;
            }
        })
        .await;
        result.wrap_err_with(|| {
            format!("post-Gloas finality timed out beyond epoch {}; last: {last}", self.gloas_epoch)
        })?
    }

    /// Returns the EL checkpoint committed by a finalized Gloas beacon block.
    pub fn execution_checkpoint(&self, block: &Value) -> Result<B256> {
        ensure!(
            block["version"] == "gloas" && block["execution_optimistic"] == false,
            "finalized beacon block is not executed Gloas: {block}"
        );
        let slot = Self::decimal(&block["data"]["message"]["slot"])?;
        ensure!(
            slot / self.slots_per_epoch >= self.gloas_epoch,
            "finalized beacon block predates Gloas"
        );
        // Gloas notify_forkchoice_updated finalizes the bid's parent. The new payload in this
        // bid is revealed after the beacon block and is not itself finalized by this checkpoint.
        // https://github.com/ethereum/consensus-specs/blob/master/specs/gloas/fork-choice.md
        Ok(serde_json::from_value(
            block["data"]["message"]["body"]["signed_execution_payload_bid"]["message"]
                ["parent_block_hash"]
                .clone(),
        )?)
    }

    /// Waits until the finalized execution head covers every required inclusion block.
    pub async fn wait_for_finalized_height(
        &self,
        rpc: &Rpc,
        l1: &str,
        required: u64,
        within: Duration,
    ) -> Result<AuthenticatedHeader> {
        let mut last = 0;
        let result = timeout(within, async {
            loop {
                let finalized = rpc.header(l1, json!("finalized")).await?;
                last = finalized.header.number;
                if last >= required {
                    return Ok::<_, eyre::Report>(finalized);
                }
                sleep(Duration::from_millis(500)).await;
            }
        })
        .await;
        result.wrap_err_with(|| {
            format!("L1 finalized head {last} did not cover required block {required}")
        })?
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use serde_json::json;

    use super::Schedule;

    #[test]
    fn gloas_finality_commits_the_parent_not_the_new_payload_bid() {
        let schedule = Schedule {
            genesis: 0,
            activation: 384,
            gloas_epoch: 8,
            gloas_version: json!("0x80000000"),
            seconds_per_slot: 6,
            slots_per_epoch: 8,
        };
        let mut block = json!({
            "version": "gloas",
            "execution_optimistic": false,
            "data": {"message": {"slot": "72", "body": {
                "signed_execution_payload_bid": {"message": {
                    "parent_block_hash": B256::repeat_byte(1),
                    "block_hash": B256::repeat_byte(2),
                }}
            }}}
        });
        assert_eq!(schedule.execution_checkpoint(&block).unwrap(), B256::repeat_byte(1));
        block["execution_optimistic"] = json!(true);
        assert!(schedule.execution_checkpoint(&block).is_err());
    }
}
