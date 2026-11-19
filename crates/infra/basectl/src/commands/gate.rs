//! `basectl conductor transfer`: leader transfers through an approval gateway.

use anyhow::Result;
use chrono::Utc;
use clap::{Args, Subcommand};

use crate::{
    CommandOutcome, ConductorCommandError, Confirm, GateClient, GateOutcomeKind, GateSettings,
    JsonOutput, ProposalStatusResponse,
};

/// Leader transfer through the approval gateway: propose, have a different
/// operator approve at the review URL, then execute.
#[derive(Debug, Args)]
pub struct TransferCommand {
    /// Transfer step to run.
    #[command(subcommand)]
    pub command: TransferCommands,
}

/// Approval gateway transfer steps.
#[derive(Debug, Subcommand)]
pub enum TransferCommands {
    /// Propose a leader transfer; prints the review URL for the approver.
    Propose(TransferProposeArgs),
    /// Show a proposal's review status and transfer outcome.
    Status(TransferStatusArgs),
    /// Transfer leadership for an approved proposal.
    Execute(TransferExecuteArgs),
}

/// Flags for `basectl conductor transfer propose`.
#[derive(Debug, Args)]
pub struct TransferProposeArgs {
    /// Target raft server ID.
    #[arg(long = "to", value_name = "ID")]
    pub to: String,
    /// Why the transfer is needed. Control characters are stripped and the
    /// text is capped before it is sent.
    #[arg(long)]
    pub reason: String,
}

/// Flags for `basectl conductor transfer status`.
#[derive(Debug, Args)]
pub struct TransferStatusArgs {
    /// Proposal ID.
    #[arg(value_name = "ID")]
    pub id: String,
    /// Emit the gateway's status response as JSON.
    #[arg(long)]
    pub json: bool,
}

/// Flags for `basectl conductor transfer execute`.
#[derive(Debug, Args)]
pub struct TransferExecuteArgs {
    /// Proposal ID.
    #[arg(value_name = "ID")]
    pub id: String,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub yes: bool,
}

impl TransferCommand {
    /// Runs the selected transfer step against the gateway.
    pub async fn run(self, gate: &GateSettings) -> Result<CommandOutcome> {
        let client = gate.client().await.map_err(ConductorCommandError::from)?;
        let outcome = self.run_with(&client).await;
        client.flush_cache().await;
        outcome
    }

    async fn run_with(self, client: &GateClient) -> Result<CommandOutcome> {
        match self.command {
            TransferCommands::Propose(args) => {
                let created = client
                    .propose(&args.to, &args.reason)
                    .await
                    .map_err(ConductorCommandError::from)?;
                println!("proposal  {}", created.id.escape_debug());
                if let Some(url) = &created.review_url {
                    println!("review    {}", url.escape_debug());
                }
                println!("Next: {}.", created.next_steps());
                Ok(CommandOutcome::Success)
            }
            TransferCommands::Status(args) => {
                let status = client.status(&args.id).await.map_err(ConductorCommandError::from)?;
                if args.json {
                    JsonOutput::print(&status)?;
                } else {
                    Self::print(&status);
                }
                Ok(CommandOutcome::Success)
            }
            TransferCommands::Execute(args) => {
                let status = client.status(&args.id).await.map_err(ConductorCommandError::from)?;
                Self::print(&status);
                if let Some(warning) = status.not_approved_warning() {
                    println!("{warning}");
                }
                let (from, to) = status.action.as_ref().map_or(("?", "?"), |action| {
                    (action.from_leader_id.as_str(), action.target_id.as_str())
                });
                let prompt = format!(
                    "Executing will transfer leadership from {} to {} now. Continue? [y/N] ",
                    from.escape_debug(),
                    to.escape_debug()
                );
                if !Confirm::prompt_or_abort(&prompt, args.yes)? {
                    return Ok(CommandOutcome::Success);
                }
                let result = client.execute(&args.id).await.map_err(ConductorCommandError::from)?;
                println!("outcome   {}", result.message());
                Ok(CommandOutcome::from_failures(result.outcome != GateOutcomeKind::Succeeded))
            }
        }
    }

    fn print(status: &ProposalStatusResponse) {
        for line in status.lines(Utc::now()) {
            println!("{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        Router,
        http::StatusCode,
        routing::{get, post},
    };
    use serde_json::json;
    use tokio::net::TcpListener;
    use url::Url;

    use super::{TransferCommand, TransferCommands, TransferExecuteArgs};
    use crate::{CommandOutcome, GateSettings};

    /// A loopback gateway whose proposal is approved and whose execute
    /// answers `status` with `outcome`.
    async fn gateway(status: u16, outcome: &'static str) -> GateSettings {
        let proposal = json!({
            "id": "rev_1", "status": "approved", "author": "alice",
            "action": {"target_id": "seq-1", "from_leader_id": "seq-0"},
            "approval": {"approvers": ["bob"]},
        })
        .to_string();
        let router = Router::new()
            .route("/admin/conductor/proposals/{id}", get(move || async move { proposal }))
            .route(
                "/admin/conductor/proposals/{id}/execute",
                post(move || async move {
                    let body = json!({"id": "rev_1", "outcome": outcome, "detail": "d"});
                    (StatusCode::from_u16(status).unwrap(), body.to_string())
                }),
            );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        GateSettings {
            url: Url::parse(&format!("http://{address}/")).unwrap(),
            token_file: None,
            token_command: None,
            timeout: GateSettings::DEFAULT_TIMEOUT,
            env_token: Some("tok".to_string()),
            cache_root: None,
            token_command_timeout: GateSettings::TOKEN_COMMAND_TIMEOUT,
        }
    }

    #[tokio::test]
    async fn execute_exit_status_follows_the_transfer_outcome() {
        for (status, outcome, expected) in [
            (200, "SUCCEEDED", CommandOutcome::Success),
            (502, "FAILED", CommandOutcome::HasFailures),
            (502, "UNKNOWN", CommandOutcome::HasFailures),
        ] {
            let gate = gateway(status, outcome).await;
            let command = TransferCommand {
                command: TransferCommands::Execute(TransferExecuteArgs {
                    id: "rev_1".to_string(),
                    yes: true,
                }),
            };

            assert_eq!(command.run(&gate).await.unwrap(), expected, "{outcome}");
        }
    }
}
