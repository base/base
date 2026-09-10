use alloy_eips::BlockNumberOrTag;
use alloy_rpc_types_debug::ExecutionWitness;
use jsonrpsee::{
    core::{ClientError, client::ClientT},
    http_client::HttpClient,
    rpc_params,
};

/// Client for comparing execution witnesses with a healthy node.
#[derive(Debug)]
pub struct ExecutionWitnessClient;

impl ExecutionWitnessClient {
    /// Fetches the execution witness using the server's default witness mode.
    pub async fn fetch(
        client: &HttpClient,
        block: BlockNumberOrTag,
    ) -> Result<ExecutionWitness, ClientError> {
        client.request("debug_executionWitness", rpc_params![block, Option::<()>::None]).await
    }
}
