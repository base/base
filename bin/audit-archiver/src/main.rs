//! Audit archiver binary entry point.

mod cli;

use audit_archiver_lib::AuditArchiverConfig;
use clap::Parser;
use cli::Args;

base_cli_utils::define_log_args!("TIPS_AUDIT");
base_cli_utils::define_metrics_args!("TIPS_AUDIT", 9002);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    let args = Args::parse();
    let config = AuditArchiverConfig::try_from(args)?;

    audit_archiver_lib::run(config).await
}
