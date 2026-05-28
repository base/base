//! JSON-RPC trait definition for the prover service API.
//!
//! This module is gated behind `rpc-server` or `rpc-client` in `lib.rs`.

use jsonrpsee::proc_macros::rpc;

use crate::{
    ClaimProofJobRequest, ClaimProofJobResponse, CompleteProofJobRequest, CompleteProofJobResponse,
    FailProofJobRequest, FailProofJobResponse, GetProofJobRequest, GetProofJobResponse,
    GetProofRequest, GetProofResponse, HeartbeatProofJobRequest, HeartbeatProofJobResponse,
    ListProofsRequest, ListProofsResponse, SubmitProofRequest, SubmitProofResponse,
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
/// JSON-RPC interface for submitting proof requests and coordinating proof jobs.
pub trait ProverServiceApi {
    /// Submit a proof request.
    #[method(name = "submitProof")]
    async fn submit_proof(
        &self,
        request: SubmitProofRequest,
    ) -> jsonrpsee::core::RpcResult<SubmitProofResponse>;

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

    /// Return a worker-owned proof job by session id.
    #[method(name = "getProofJob")]
    async fn get_proof_job(
        &self,
        request: GetProofJobRequest,
    ) -> jsonrpsee::core::RpcResult<GetProofJobResponse>;

    /// Claim the next eligible queued proof job.
    #[method(name = "claimProofJob")]
    async fn claim_proof_job(
        &self,
        request: ClaimProofJobRequest,
    ) -> jsonrpsee::core::RpcResult<ClaimProofJobResponse>;

    /// Extend a proof job lease.
    #[method(name = "heartbeatProofJob")]
    async fn heartbeat_proof_job(
        &self,
        request: HeartbeatProofJobRequest,
    ) -> jsonrpsee::core::RpcResult<HeartbeatProofJobResponse>;

    /// Complete a leased proof job.
    #[method(name = "completeProofJob")]
    async fn complete_proof_job(
        &self,
        request: CompleteProofJobRequest,
    ) -> jsonrpsee::core::RpcResult<CompleteProofJobResponse>;

    /// Fail a leased proof job.
    #[method(name = "failProofJob")]
    async fn fail_proof_job(
        &self,
        request: FailProofJobRequest,
    ) -> jsonrpsee::core::RpcResult<FailProofJobResponse>;
}
