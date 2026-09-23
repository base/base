//! Implementation of the `basectl batcher` command group.

use std::io::{self, Write};

use anyhow::Result;
use clap::{Args, Subcommand};
use serde::Serialize;
use tracing::{debug, info, warn};
use url::Url;

use crate::{BatcherClient, BatcherStatus, Confirm, JsonOutput, KeyValueTable, MonitoringConfig};

/// Inspect and control the batcher through its admin RPC.
#[derive(Debug, Args)]
pub struct BatcherCommand {
    /// Batcher operation to run.
    #[command(subcommand)]
    pub command: BatcherCommands,
}

/// Batcher inspection and control commands.
#[derive(Debug, Subcommand)]
pub enum BatcherCommands {
    /// Show whether the batcher is stopped, its in-flight submissions and its DA backlog.
    Status(BatcherStatusArgs),
    /// Stop batch submission. Submissions already in flight keep settling.
    Stop(BatcherActionArgs),
    /// Start batch submission again from the safe L2 head.
    Start(BatcherActionArgs),
    /// Close the current channel so its frames become eligible for submission.
    Flush(BatcherActionArgs),
}

/// Flags for `basectl batcher status`.
#[derive(Debug, Args)]
pub struct BatcherStatusArgs {
    /// Batcher admin RPC URL. Overrides `batcher_rpc` from the selected config.
    #[arg(long = "batcher-rpc", env = "BASECTL_BATCHER_RPC", value_name = "URL")]
    pub batcher_rpc: Option<Url>,
    /// Emit a structured JSON status instead of pretty text.
    #[arg(long)]
    pub json: bool,
}

/// Flags for the mutating `basectl batcher` actions.
#[derive(Debug, Args)]
pub struct BatcherActionArgs {
    /// Batcher admin RPC URL. Overrides `batcher_rpc` from the selected config.
    #[arg(long = "batcher-rpc", env = "BASECTL_BATCHER_RPC", value_name = "URL")]
    pub batcher_rpc: Option<Url>,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub yes: bool,
    /// Emit a structured JSON action outcome instead of pretty text.
    #[arg(long, requires = "yes")]
    pub json: bool,
}

impl BatcherCommand {
    /// Runs the selected batcher subcommand.
    pub async fn run(self, config: MonitoringConfig) -> Result<()> {
        match self.command {
            BatcherCommands::Status(args) => run_status(config, args).await,
            BatcherCommands::Stop(args) => run_action(config, BatcherAction::Stop, args).await,
            BatcherCommands::Start(args) => run_action(config, BatcherAction::Start, args).await,
            BatcherCommands::Flush(args) => run_action(config, BatcherAction::Flush, args).await,
        }
    }
}

async fn run_status(config: MonitoringConfig, args: BatcherStatusArgs) -> Result<()> {
    let rpc = config.resolve_batcher_rpc(args.batcher_rpc.as_ref())?;
    let display_rpc = BatcherClient::display_url(&rpc);
    info!(
        network = %config.name,
        rpc = %display_rpc,
        json = args.json,
        "running batcher status command"
    );

    let status = BatcherClient::status(&rpc).await.inspect_err(|error| {
        warn!(error = %error, network = %config.name, rpc = %display_rpc, "batcher status failed");
    })?;

    let status = BatcherStatusJson::new(&config.name, &rpc, status);
    if args.json {
        JsonOutput::print(&status)?;
    } else {
        print_status_pretty(&status)?;
    }
    Ok(())
}

async fn run_action(
    config: MonitoringConfig,
    action: BatcherAction,
    args: BatcherActionArgs,
) -> Result<()> {
    let rpc = config.resolve_batcher_rpc(args.batcher_rpc.as_ref())?;
    let display_rpc = BatcherClient::display_url(&rpc);
    info!(
        network = %config.name,
        rpc = %display_rpc,
        action = %action.as_str(),
        json = args.json,
        yes = args.yes,
        "running batcher action command"
    );

    let prompt = format!("{} on {} ({display_rpc})? [y/N] ", action.prompt(), config.name);
    if !Confirm::prompt_or_abort(&prompt, args.yes)? {
        debug!(
            network = %config.name,
            rpc = %display_rpc,
            action = %action.as_str(),
            "batcher action confirmation declined"
        );
        return Ok(());
    }

    let result = match action {
        BatcherAction::Stop => BatcherClient::stop(&rpc).await,
        BatcherAction::Start => BatcherClient::start(&rpc).await,
        BatcherAction::Flush => BatcherClient::flush(&rpc).await,
    };
    result.inspect_err(|error| {
        warn!(
            error = %error,
            network = %config.name,
            rpc = %display_rpc,
            action = %action.as_str(),
            "batcher action failed"
        );
    })?;

    let outcome = BatcherActionJson::new(&config.name, &rpc, action);
    JsonOutput::print_or_ok(&outcome, &outcome.message, args.json)?;
    info!(
        network = %config.name,
        rpc = %display_rpc,
        action = %action.as_str(),
        "batcher action completed"
    );
    Ok(())
}

/// Mutating batcher admin action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BatcherAction {
    /// Stop batch submission.
    Stop,
    /// Start batch submission again.
    Start,
    /// Close the current channel.
    Flush,
}

impl BatcherAction {
    /// Returns the action name used in logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Start => "start",
            Self::Flush => "flush",
        }
    }

    /// Returns the question asked before running the action.
    pub const fn prompt(self) -> &'static str {
        match self {
            Self::Stop => "Stop batch submission",
            Self::Start => "Start batch submission",
            Self::Flush => "Flush the current channel",
        }
    }

    /// Returns the result reported once the batcher has applied the action.
    pub const fn message(self) -> &'static str {
        match self {
            Self::Stop => "batch submission stopped",
            Self::Start => "batch submission running",
            Self::Flush => "current channel flushed",
        }
    }
}

/// JSON shape for `basectl batcher status`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatcherStatusJson {
    /// Network name.
    pub network: String,
    /// Batcher admin RPC URL, without credentials.
    pub rpc: String,
    /// Whether batch submission is stopped.
    pub stopped: bool,
    /// Number of L1 transactions submitted but not yet confirmed.
    pub in_flight: u64,
    /// Estimated unsubmitted DA backlog in bytes.
    pub da_backlog_bytes: u64,
}

impl BatcherStatusJson {
    /// Builds the status output for one batcher.
    pub fn new(network: &str, rpc: &Url, status: BatcherStatus) -> Self {
        Self {
            network: network.to_string(),
            rpc: BatcherClient::display_url(rpc),
            stopped: status.stopped,
            in_flight: status.in_flight,
            da_backlog_bytes: status.da_backlog_bytes,
        }
    }
}

/// JSON shape for the mutating `basectl batcher` actions.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatcherActionJson {
    /// Network name.
    pub network: String,
    /// Batcher admin RPC URL, without credentials.
    pub rpc: String,
    /// Action performed.
    pub action: BatcherAction,
    /// Human-readable result.
    pub message: String,
}

impl BatcherActionJson {
    /// Builds the outcome of an action the batcher has applied.
    pub fn new(network: &str, rpc: &Url, action: BatcherAction) -> Self {
        Self {
            network: network.to_string(),
            rpc: BatcherClient::display_url(rpc),
            action,
            message: action.message().to_string(),
        }
    }
}

fn print_status_pretty(status: &BatcherStatusJson) -> Result<()> {
    let mut stdout = io::stdout().lock();
    print_status_pretty_to(&mut stdout, status)?;
    Ok(())
}

fn print_status_pretty_to<W: Write>(writer: &mut W, status: &BatcherStatusJson) -> Result<()> {
    let mut table = KeyValueTable::new();
    table
        .row("network", status.network.as_str())
        .row("rpc", status.rpc.as_str())
        .row("stopped", status.stopped.to_string())
        .row("in flight", status.in_flight.to_string())
        .row("da backlog bytes", status.da_backlog_bytes.to_string());
    table.render(writer)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn rpc() -> Url {
        Url::parse("http://operator:secret@127.0.0.1:6545").unwrap()
    }

    #[test]
    fn status_json_is_camel_case_and_hides_credentials() {
        let status = BatcherStatus { stopped: true, in_flight: 2, da_backlog_bytes: 1024 };

        let value =
            serde_json::to_value(BatcherStatusJson::new("sepolia", &rpc(), status)).unwrap();

        assert_eq!(
            value,
            json!({
                "network": "sepolia",
                "rpc": "http://127.0.0.1:6545",
                "stopped": true,
                "inFlight": 2,
                "daBacklogBytes": 1024,
            })
        );
    }

    #[test]
    fn action_json_names_the_action() {
        let outcome = BatcherActionJson::new("sepolia", &rpc(), BatcherAction::Stop);

        assert_eq!(
            serde_json::to_value(outcome).unwrap(),
            json!({
                "network": "sepolia",
                "rpc": "http://127.0.0.1:6545",
                "action": "stop",
                "message": "batch submission stopped",
            })
        );
    }

    #[test]
    fn pretty_status_lists_every_field() {
        let status = BatcherStatus { stopped: false, in_flight: 3, da_backlog_bytes: 42 };
        let mut rendered = Vec::new();

        print_status_pretty_to(&mut rendered, &BatcherStatusJson::new("mainnet", &rpc(), status))
            .unwrap();

        let rendered = String::from_utf8(rendered).unwrap();
        for (label, value) in [
            ("network", "mainnet"),
            ("rpc", "http://127.0.0.1:6545"),
            ("stopped", "false"),
            ("in flight", "3"),
            ("da backlog bytes", "42"),
        ] {
            assert!(
                rendered.lines().any(|line| line.starts_with(label) && line.ends_with(value)),
                "missing `{label}` row with `{value}` in {rendered}"
            );
        }
    }
}
