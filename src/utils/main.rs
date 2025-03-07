use clap::Parser;
use serde_json::Value;

/// CLI Commands
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Cli {
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Parser, Debug)]
enum Commands {
    /// Generate genesis configuration
    Genesis {
        #[clap(long, help = "Output path for genesis files")]
        output: Option<String>,
    },
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Genesis { output } => generate_genesis(output).await,
    }
}

pub async fn generate_genesis(output: Option<String>) -> eyre::Result<()> {
    // Read the template file
    let template = include_str!("fixtures/genesis.json.tmpl");

    // Parse the JSON
    let mut genesis: Value = serde_json::from_str(template)?;

    // Update the timestamp field - example using current timestamp
    let timestamp = chrono::Utc::now().timestamp();
    if let Some(config) = genesis.as_object_mut() {
        // Assuming timestamp is at the root level - adjust path as needed
        config["timestamp"] = Value::String(format!("0x{:x}", timestamp));
    }

    // Write the result to the output file
    if let Some(output) = output {
        std::fs::write(&output, serde_json::to_string_pretty(&genesis)?)?;
        println!("Generated genesis file at: {}", output);
    } else {
        println!("{}", serde_json::to_string_pretty(&genesis)?);
    }

    Ok(())
}
