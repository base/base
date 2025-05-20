use alloy_primitives::Address;
use clap::Parser;
use op_rbuilder::tests::*;

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
    /// Run the testing system
    Run {
        #[clap(long, short, action)]
        validation: bool,

        #[clap(long, short, action, default_value = "false")]
        no_tx_pool: bool,

        #[clap(long, short, action, default_value = "1")]
        block_time_secs: u64,

        #[clap(long, short, action)]
        flashblocks_endpoint: Option<String>,

        #[clap(long, action, default_value = "false")]
        no_sleep: bool,
    },
    /// Deposit funds to the system
    Deposit {
        #[clap(long, help = "Address to deposit funds to")]
        address: Address,
        #[clap(long, help = "Amount to deposit")]
        amount: u128,
    },
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Genesis { output } => generate_genesis(output).await,
        Commands::Run {
            validation,
            no_tx_pool,
            block_time_secs,
            flashblocks_endpoint,
            no_sleep,
        } => {
            run_system(
                validation,
                no_tx_pool,
                block_time_secs,
                flashblocks_endpoint,
                no_sleep,
            )
            .await
        }
        Commands::Deposit { address, amount } => {
            let engine_api = EngineApi::builder().build().unwrap();
            let mut generator = BlockGenerator::new(engine_api, None, false, 1, None);

            generator.init().await?;

            let block_hash = generator.deposit(address, amount).await?;
            println!("Deposit transaction included in block: {block_hash}");
            Ok(())
        }
    }
}

#[allow(dead_code)]
pub async fn run_system(
    validation: bool,
    no_tx_pool: bool,
    block_time_secs: u64,
    flashblocks_endpoint: Option<String>,
    no_sleep: bool,
) -> eyre::Result<()> {
    println!("Validation: {validation}");

    let engine_api = EngineApi::new("http://localhost:4444").unwrap();
    let validation_api = if validation {
        Some(EngineApi::new("http://localhost:5555").unwrap())
    } else {
        None
    };

    let mut generator = BlockGenerator::new(
        engine_api,
        validation_api,
        no_tx_pool,
        block_time_secs,
        flashblocks_endpoint,
    );

    generator.init().await?;

    // Infinite loop generating blocks
    loop {
        println!("Generating new block...");
        let block_hash = generator.submit_payload(None, 0, no_sleep).await?;
        println!("Generated block: {block_hash}");
    }
}
