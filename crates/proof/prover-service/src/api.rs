// This module requires at least one of the RPC features to compile correctly.
// The `lib.rs` cfg gate normally ensures this, but we add an explicit guard for
// safety in case the module is ever included directly.
#[cfg(not(any(feature = "rpc-server", feature = "rpc-client")))]
compile_error!("this module requires the `rpc-server` or `rpc-client` feature");

#[cfg(feature = "rpc-server")]
use jsonrpsee::core::RpcResult;
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
    async fn submit_proof(&self, request: SubmitProofRequest) -> RpcResult<SubmitProofResponse>;

    /// Return proof status and result data for a submitted proof request.
    #[method(name = "getProof")]
    async fn get_proof(&self, request: GetProofRequest) -> RpcResult<GetProofResponse>;

    /// List submitted proof requests.
    #[method(name = "listProofs")]
    async fn list_proofs(&self, request: ListProofsRequest) -> RpcResult<ListProofsResponse>;

    /// Return a worker-owned proof job by session id.
    #[method(name = "getProofJob")]
    async fn get_proof_job(&self, request: GetProofJobRequest) -> RpcResult<GetProofJobResponse>;

    /// Claim the next eligible queued proof job.
    #[method(name = "claimProofJob")]
    async fn claim_proof_job(
        &self,
        request: ClaimProofJobRequest,
    ) -> RpcResult<ClaimProofJobResponse>;

    /// Extend a proof job lease.
    #[method(name = "heartbeatProofJob")]
    async fn heartbeat_proof_job(
        &self,
        request: HeartbeatProofJobRequest,
    ) -> RpcResult<HeartbeatProofJobResponse>;

    /// Complete a leased proof job.
    #[method(name = "completeProofJob")]
    async fn complete_proof_job(
        &self,
        request: CompleteProofJobRequest,
    ) -> RpcResult<CompleteProofJobResponse>;

    /// Fail a leased proof job.
    #[method(name = "failProofJob")]
    async fn fail_proof_job(&self, request: FailProofJobRequest)
    -> RpcResult<FailProofJobResponse>;
}
