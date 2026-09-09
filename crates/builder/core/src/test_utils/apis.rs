use alloy_eips::BlockNumberOrTag;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use serde_json::Value;
use tracing::info;

/// JSON-RPC interface for block queries used in tests.
#[rpc(server, client, namespace = "eth")]
pub trait BlockApi {
    /// Returns a block by number or tag, optionally including full transactions.
    #[method(name = "getBlockByNumber")]
    async fn get_block_by_number(
        &self,
        block_number: BlockNumberOrTag,
        include_txs: bool,
    ) -> RpcResult<Option<base_common_types_rpc::Block>>;
}

/// Generates a genesis JSON file from the embedded template, stamped with the current time.
pub fn generate_genesis(output: Option<String>) -> eyre::Result<()> {
    // Read the template file
    let template = include_str!("artifacts/genesis.json.tmpl");

    // Parse the JSON
    let mut genesis: Value = serde_json::from_str(template)?;

    // Update the timestamp field - example using current timestamp
    let timestamp = chrono::Utc::now().timestamp();
    if let Some(config) = genesis.as_object_mut() {
        // Assuming timestamp is at the root level - adjust path as needed
        config["timestamp"] = Value::String(format!("0x{timestamp:x}"));
    }

    // Write the result to the output file
    if let Some(output) = output {
        std::fs::write(&output, serde_json::to_string_pretty(&genesis)?)?;
        info!(output = %output, "Generated genesis file at");
    } else {
        println!("{}", serde_json::to_string_pretty(&genesis)?);
    }

    Ok(())
}
