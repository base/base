//! Backend-session tracking over the prover-service worker API.

use base_prover_service_client::{ProverServiceClientError, ProverWorkerProvider};
use base_prover_service_protocol::{
    BackendSession, BackendSessionState, GetProofSessionRequest, RecordProofSessionRequest,
    SessionType,
};

/// Tracks the backend session for one claimed proof job via the prover service.
#[derive(Clone, Debug)]
pub struct ProofSessionHandle<Client> {
    client: Client,
    session_id: String,
    lock_id: String,
    worker_id: String,
}

impl<Client> ProofSessionHandle<Client> {
    /// Bind a worker client to a claimed job's session identifiers.
    pub const fn new(
        client: Client,
        session_id: String,
        lock_id: String,
        worker_id: String,
    ) -> Self {
        Self { client, session_id, lock_id, worker_id }
    }
}

impl<Client> ProofSessionHandle<Client>
where
    Client: ProverWorkerProvider,
{
    /// Look up the active backend session recorded for this job, if any.
    pub async fn get(
        &self,
        session_type: SessionType,
    ) -> Result<Option<BackendSession>, ProverServiceClientError> {
        let response = self
            .client
            .get_proof_session(GetProofSessionRequest {
                session_id: self.session_id.clone(),
                session_type,
            })
            .await?;

        Ok(response.session)
    }

    /// Record (insert or update) the backend session for this job.
    pub async fn record(
        &self,
        session_type: SessionType,
        backend_session_id: String,
        state: BackendSessionState,
    ) -> Result<BackendSession, ProverServiceClientError> {
        let response = self
            .client
            .record_proof_session(RecordProofSessionRequest {
                session_id: self.session_id.clone(),
                lock_id: self.lock_id.clone(),
                worker_id: self.worker_id.clone(),
                session_type,
                backend_session_id,
                state,
            })
            .await?;

        Ok(response.session)
    }
}
