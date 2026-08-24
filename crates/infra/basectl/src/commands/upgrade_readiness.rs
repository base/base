//! Implementation of the `basectl upgrade-readiness` subcommand.
//!
//! A pre-flight check node operators run to confirm their node will follow an upcoming
//! contract-backed upgrade rather than fall behind (or fail closed) at activation. It calls the
//! public `base_upgradeReadiness` RPC and exits non-zero when the node is not ready, so it can gate
//! a fleet-wide rollout before the upgrade is scheduled on L1.

use anyhow::Result;
use base_upgrade_signal::UpgradeReadiness;
use clap::Args;
use url::Url;

use crate::{CommandOutcome, JsonOutput, KeyValueTable, MonitoringConfig, fetch_upgrade_readiness};

/// Arguments for the upgrade-readiness pre-flight check.
#[derive(Debug, Args)]
pub struct UpgradeReadinessCommand {
    /// Override the consensus-node RPC URL.
    ///
    /// Defaults to the chain config's `consensus_node_rpc` field.
    #[arg(long = "cl-rpc", value_name = "URL")]
    pub cl_rpc: Option<Url>,
    /// Announced upgrade version to check support against *before* it is scheduled on L1.
    ///
    /// Plain `major.minor.patch` semver, optionally with an `-rc.N` pre-release (e.g. `1.2.3` or
    /// `1.2.3-rc.4`). Use this in the window between rolling a release out to operators and
    /// publishing the schedule on L1, when the contract does not yet carry the new minimum: the
    /// overall `ready` answer is then judged against this version. Omit it once the upgrade is
    /// scheduled to judge readiness against the on-chain minimum instead.
    #[arg(long = "target-version", value_name = "SEMVER")]
    pub target_version: Option<String>,
    /// Emit JSON instead of the pretty table.
    #[arg(long)]
    pub json: bool,
}

impl UpgradeReadinessCommand {
    /// Fetches upgrade readiness and renders it, returning a non-zero outcome when not ready.
    pub async fn run(self, config: MonitoringConfig) -> Result<CommandOutcome> {
        let cl_rpc = config.resolve_cl_rpc(self.cl_rpc.as_ref(), "upgrade-readiness")?;
        let readiness = fetch_upgrade_readiness(&cl_rpc, self.target_version).await?;

        if self.json {
            JsonOutput::print(&readiness)?;
        } else {
            print_pretty(&config.name, &readiness)?;
        }

        // Exit non-zero when the node is not ready so a rollout script can gate on the exit code.
        Ok(CommandOutcome::from_failures(!readiness.ready))
    }
}

/// Renders upgrade readiness as the pretty key-value table.
fn print_pretty(network: &str, readiness: &UpgradeReadiness) -> Result<()> {
    let mut table = KeyValueTable::new();
    table
        .row("network", network)
        .row("ready", readiness.ready.to_string())
        .row("mode", format!("{:?}", readiness.mode))
        .row("node_protocol_version", &readiness.node_protocol_version)
        .row(
            "l1_block_number",
            readiness
                .l1_block_number
                .map_or_else(|| "none (no schedule on L1)".to_string(), |block| block.to_string()),
        );

    if let Some(reason) = &readiness.reason {
        table.row("reason", reason);
    }

    if readiness.upgrades.is_empty() {
        table.row("scheduled_upgrades", "none");
    } else {
        for upgrade in &readiness.upgrades {
            table.row(
                format!("upgrade[{}]", upgrade.upgrade_id),
                format!(
                    "required={} activation={} supported={} malformed={} would_halt={}",
                    upgrade.required_protocol_version,
                    upgrade.activation_timestamp,
                    upgrade.supported,
                    upgrade.malformed,
                    upgrade.would_halt,
                ),
            );
        }
    }

    table.print()?;
    Ok(())
}
