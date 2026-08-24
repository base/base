//! Telemetry inspection utilities.

use base_telemetry_client::{NodeIdentity, NodeReportBuilder, TelemetryId};
use base_telemetry_types::{Heads, NetHealth, NetworkName, NodeConfigReport, NodeLayer, NodeRole};
use clap::{Args, Subcommand};

use crate::{cli::TelemetryArgs, config::ResolvedChainConfig};

/// Arguments for `base telemetry`.
#[derive(Args, Clone, Debug)]
pub(crate) struct TelemetryCommand {
    /// The telemetry action to run.
    #[command(subcommand)]
    pub(crate) action: TelemetryAction,
}

/// Actions under `base telemetry`.
#[derive(Subcommand, Clone, Debug)]
pub(crate) enum TelemetryAction {
    /// Print the report this node would send, without sending or persisting anything.
    #[command(name = "preview")]
    Preview,
}

impl TelemetryCommand {
    /// Runs the selected telemetry action.
    pub(crate) fn run(
        self,
        resolved_chain: ResolvedChainConfig,
        metrics_enabled: bool,
        telemetry: TelemetryArgs,
    ) -> eyre::Result<()> {
        match self.action {
            TelemetryAction::Preview => Self::preview(resolved_chain, metrics_enabled, telemetry),
        }
    }

    /// Prints the payload a consensus node would send, as JSON.
    ///
    /// Nothing is sent and no identity is minted: an operator inspecting the payload must not
    /// become a reporter by doing so. A node that has already minted an identity sees its real
    /// one, and every other run sees a throwaway.
    ///
    /// Head and network values are zeroed because there is no running node behind this command.
    /// The point of the preview is the shape of the payload and the real `hw.*` and `cfg.*`
    /// values, which are the fields worth reviewing before a node reports anywhere.
    ///
    /// The disk fields need a directory to measure and there is no node here to supply one, so
    /// they are absent unless `--telemetry.data-dir` names it. Absent is the honest answer: the
    /// alternative is measuring whichever volume holds the identity file and presenting that as
    /// what the node would report.
    fn preview(
        resolved_chain: ResolvedChainConfig,
        metrics_enabled: bool,
        telemetry: TelemetryArgs,
    ) -> eyre::Result<()> {
        let l2_chain_id = resolved_chain.consensus_chain_args().l2_chain_id;
        let config = telemetry.config(l2_chain_id.id());

        let identity = NodeIdentity {
            telemetry_id: TelemetryId::read(&config.id_path).unwrap_or_else(TelemetryId::generate),
            instance_id: config.instance_id.clone(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            l2_chain_id: l2_chain_id.id(),
            network: NetworkName::for_chain_id(l2_chain_id.id()),
            layer: NodeLayer::Consensus,
            role: NodeRole::Validator,
            data_dir: config.data_dir.clone(),
        };
        let node_config = NodeConfigReport {
            prune_mode: None,
            p2p_enabled: true,
            discovery_enabled: true,
            sequencer_enabled: false,
            supervisor_enabled: false,
            flashblocks_enabled: false,
            metrics_enabled,
            experimental_flags: Vec::new(),
            report_interval_secs: config.report_interval.as_secs(),
            sample_interval_secs: config.sample_interval.as_secs(),
        };

        let report = NodeReportBuilder::new(identity, node_config)
            .build(Heads::default(), NetHealth::default());
        println!("{}", serde_json::to_string_pretty(&report)?);
        Ok(())
    }
}
