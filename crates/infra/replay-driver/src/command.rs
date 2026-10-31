use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use alloy_rpc_types_engine::JwtSecret;
use base_clock::{Clock, GlobalClock, OffsetClock, SystemClock};
use base_common_network::Base;
use base_consensus_engine::BaseEngineClient;
use base_execution_chainspec::BaseChainSpec;
use base_execution_cli::chainspec::chain_value_parser;
use clap::Parser;
use eyre::{OptionExt, ensure};
use reth_tasks::TaskExecutor;
use url::Url;

use crate::{BuilderIngressInjector, ReplayDataset, ReplayRunner, ShadowDriver};

/// Attach to a replay-clock builder and write candidate-versus-canonical reports.
#[derive(Debug, Parser)]
pub struct RunCommand {
    /// Directory containing manifest.json and records.jsonl.
    #[arg(long)]
    pub export: PathBuf,
    /// Print the builder clock flag without connecting or running replay.
    #[arg(long)]
    pub print_clock_start: bool,
    /// Canonical L2 HTTP RPC endpoint.
    #[arg(long, required_unless_present = "print_clock_start")]
    pub l2_rpc: Option<Url>,
    /// Builder Engine API endpoint, including file:// for local IPC.
    #[arg(long, required_unless_present = "print_clock_start")]
    pub builder_engine: Option<Url>,
    /// Existing builder JWT file; never creates or replaces it.
    #[arg(long, required_unless_present = "print_clock_start")]
    pub builder_jwt: Option<PathBuf>,
    /// Builder ingress HTTP RPC endpoint.
    #[arg(long, required_unless_present = "print_clock_start")]
    pub builder_rpc: Option<Url>,
    /// System Unix milliseconds when the builder installed its `OffsetClock`.
    #[arg(long, required_unless_present = "print_clock_start")]
    pub builder_started_at_ms: Option<u64>,
    /// Override the epoch passed to the builder's --replay.clock-start-ms.
    #[arg(long)]
    pub clock_start_ms: Option<u64>,
    /// Fork schedule: built-in chain name or execution genesis JSON file.
    #[arg(long, default_value = "mainnet", value_parser = chain_value_parser)]
    pub chain: Arc<BaseChainSpec>,
    /// New report directory; existing paths are rejected.
    #[arg(long, required_unless_present = "print_clock_start")]
    pub out: Option<PathBuf>,
}

impl RunCommand {
    /// Load the export, align the runner clock, and execute against existing builder state.
    /// Returns errors without writing successful reports when replay fails.
    pub async fn execute(self, executor: &TaskExecutor) -> eyre::Result<()> {
        let dataset = ReplayDataset::load(&self.export).await?;
        let epoch = self.clock_start_ms.unwrap_or_else(|| dataset.clock_start_ms());
        println!("--replay.clock-start-ms {epoch}");
        if self.print_clock_start {
            return Ok(());
        }
        ensure!(
            self.chain.inner.chain.id() == dataset.manifest.window.chain_id,
            "--chain differs from export chain ID"
        );
        let canonical_url = self.l2_rpc.ok_or_eyre("--l2-rpc required")?;
        let ingress_url = self.builder_rpc.ok_or_eyre("--builder-rpc required")?;
        ensure!(
            matches!(canonical_url.scheme(), "http" | "https")
                && matches!(ingress_url.scheme(), "http" | "https"),
            "canonical and ingress RPC require HTTP URLs"
        );
        let out = self.out.ok_or_eyre("--out required")?;
        let jwt = JwtSecret::from_hex(
            tokio::fs::read_to_string(self.builder_jwt.ok_or_eyre("--builder-jwt required")?)
                .await?
                .trim(),
        )?;
        let engine = tokio::time::timeout(
            Duration::from_secs(15),
            BaseEngineClient::<RootProvider, RootProvider<Base>>::rpc_client::<Base>(
                self.builder_engine.ok_or_eyre("--builder-engine required")?,
                jwt,
            ),
        )
        .await??;
        let builder_chain =
            tokio::time::timeout(Duration::from_secs(15), engine.get_chain_id()).await??;
        ensure!(
            builder_chain == dataset.manifest.window.chain_id,
            "builder chain ID differs from export"
        );
        let now = u64::try_from(SystemClock.now_unix_millis()?)?;
        let started = self.builder_started_at_ms.ok_or_eyre("--builder-started-at-ms required")?;
        let elapsed = now.checked_sub(started).ok_or_eyre("builder start time is in the future")?;
        let replay_now = epoch.checked_add(elapsed).ok_or_eyre("replay epoch overflow")?;
        let start = UNIX_EPOCH
            .checked_add(Duration::from_millis(replay_now))
            .ok_or_eyre("replay clock outside system range")?;
        GlobalClock::set(OffsetClock::new(start))?;
        tokio::fs::create_dir(&out).await?;
        let runner = ReplayRunner {
            canonical: ProviderBuilder::new_with_network::<Base>().connect_http(canonical_url),
            engine,
            injector: BuilderIngressInjector::new(ingress_url, executor)?,
            chain: self.chain.as_ref(),
            driver: ShadowDriver::default(),
        };
        let report = runner.run(dataset).await?;
        tokio::fs::write(out.join("diff.json"), serde_json::to_vec_pretty(&report)?).await?;
        tokio::fs::write(out.join("diff.md"), report.markdown()).await?;
        println!("reports written to {}", out.display());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::RunCommand;

    #[test]
    fn requires_endpoints_when_running_but_not_when_printing_clock() {
        assert!(RunCommand::try_parse_from(["run", "--export", "export"]).is_err());
        let command =
            RunCommand::try_parse_from(["run", "--export", "export", "--print-clock-start"])
                .unwrap();
        assert!(command.print_clock_start);
        assert!(command.builder_engine.is_none());
    }
}
