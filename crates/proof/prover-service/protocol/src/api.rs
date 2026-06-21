//! Role-specific JSON-RPC trait definitions for the prover service API.

use jsonrpsee::proc_macros::rpc;

use crate::{
    GetNextProofRequest, GetNextProofResponse, GetProofRequest, GetProofResponse,
    GetProofSessionRequest, GetProofSessionResponse, HeartbeatRequest, HeartbeatResponse,
    ListProofsRequest, ListProofsResponse, ProveBlockRangeRequest, ProveBlockRangeResponse,
    RecordProofSessionRequest, RecordProofSessionResponse, WorkerSubmitProofRequest,
    WorkerSubmitProofResponse,
};

#[cfg_attr(
    all(feature = "rpc-server", feature = "rpc-client"),
    rpc(server, client, namespace = "prover")
)]
#[cfg_attr(
    all(feature = "rpc-server", not(feature = "rpc-client")),
    rpc(server, namespace = "prover")
)]
#[cfg_attr(
    all(feature = "rpc-client", not(feature = "rpc-server")),
    rpc(client, namespace = "prover")
)]
/// JSON-RPC interface for proof requesters.
pub trait ProverRequesterApi {
    /// Submit a prove-block-range proof request.
    #[method(name = "proveBlockRange")]
    async fn prove_block_range(
        &self,
        request: ProveBlockRangeRequest,
    ) -> jsonrpsee::core::RpcResult<ProveBlockRangeResponse>;

    /// Return proof status and result data for a submitted proof request.
    #[method(name = "getProof")]
    async fn get_proof(
        &self,
        request: GetProofRequest,
    ) -> jsonrpsee::core::RpcResult<GetProofResponse>;

    /// List submitted proof requests.
    #[method(name = "listProofs")]
    async fn list_proofs(
        &self,
        request: ListProofsRequest,
    ) -> jsonrpsee::core::RpcResult<ListProofsResponse>;
}

#[cfg_attr(
    all(feature = "rpc-server", feature = "rpc-client"),
    rpc(server, client, namespace = "prover")
)]
#[cfg_attr(
    all(feature = "rpc-server", not(feature = "rpc-client")),
    rpc(server, namespace = "prover")
)]
#[cfg_attr(
    all(feature = "rpc-client", not(feature = "rpc-server")),
    rpc(client, namespace = "prover")
)]
/// JSON-RPC interface for prover workers.
pub trait ProverWorkerApi {
    /// Return and atomically claim the next available proof job.
    #[method(name = "getNextProof")]
    async fn get_next_proof(
        &self,
        request: GetNextProofRequest,
    ) -> jsonrpsee::core::RpcResult<GetNextProofResponse>;

    /// Extend a worker-owned proof job lock.
    #[method(name = "heartbeat")]
    async fn heartbeat(
        &self,
        request: HeartbeatRequest,
    ) -> jsonrpsee::core::RpcResult<HeartbeatResponse>;

    /// Submit a proof result for a proof job.
    #[method(name = "submitProof")]
    async fn submit_proof(
        &self,
        request: WorkerSubmitProofRequest,
    ) -> jsonrpsee::core::RpcResult<WorkerSubmitProofResponse>;

    /// Look up the active backend session recorded for a claimed proof job.
    #[method(name = "getProofSession")]
    async fn get_proof_session(
        &self,
        request: GetProofSessionRequest,
    ) -> jsonrpsee::core::RpcResult<GetProofSessionResponse>;

    /// Record (insert or update) the backend session for a claimed proof job.
    #[method(name = "recordProofSession")]
    async fn record_proof_session(
        &self,
        request: RecordProofSessionRequest,
    ) -> jsonrpsee::core::RpcResult<RecordProofSessionResponse>;
}
