#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod cli;

#[tokio::main]
async fn main() {
    let _ = reth_node_core::args::DefaultTraceValues::default()
        .with_service_name("base-proof-tee-registrar")
        .try_init();
    let result: eyre::Result<()> = async {
        let cli = base_cli_utils::EnvAlias::parse::<cli::Cli>(cli::ENV_ALIASES);
        let traces = cli.traces.clone();
        let config = cli.config()?;
        config.log_config.init_with_trace_args(&traces, &[])?;
        config.run().await?;
        Ok(())
    }
    .await;

    if let Err(err) = result {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
