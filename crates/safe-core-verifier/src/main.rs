use anyhow::Result;
use clap::{Parser, Subcommand};
use safe_core_verifier::PolyglotVerifier;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "safe-core-verify")]
#[command(about = "Verificador poliglota de código gerado por IA com Safe-Core")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Verifica um arquivo ou diretório
    Verify {
        #[arg(short, long)]
        path: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Default checks can be added here
    let verifier = PolyglotVerifier::new(vec![]);

    match cli.command {
        Commands::Verify { path } => {
            if path.is_file() || path.is_dir() {
                let path_str = path.to_str().unwrap();
                let global_report = verifier.verify_project(path_str).await?;
                println!("📊 Safe-Core Verification Report");
                println!("   Total files: {}", global_report.total_files);
                println!("   ✅ Passed: {}", global_report.passed_files);
                println!("   ❌ Failed: {}", global_report.failed_files);
            }
        }
    }

    Ok(())
}
