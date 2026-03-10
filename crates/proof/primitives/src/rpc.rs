use jsonrpsee::{core::RpcResult, proc_macros::rpc};

use crate::{ProofRequest, ProofResult};

/// JSON-RPC interface shared by all proof backends.
#[cfg_attr(not(feature = "rpc-client"), rpc(server, namespace = "prover"))]
#[cfg_attr(feature = "rpc-client", rpc(server, client, namespace = "prover"))]
pub trait ProverApi {
    /// Run the proof pipeline for a single request.
    #[method(name = "prove")]
    async fn prove(&self, request: ProofRequest) -> RpcResult<ProofResult>;
}
