use std::sync::Arc;

use async_trait::async_trait;
use base_zk_client::ProveBlockRequest;
use base_zk_db::{ProofRequest, ProofRequestRepo, ProofSession};
use tracing::info;

use super::traits::{
    BackendConfig, BackendType, ProofProcessingResult, ProveResult, ProvingBackend, SessionStatus,
};

pub(super) struct GenericZkvmBackend {
    _config: BackendConfig,
}

impl GenericZkvmBackend {
    pub(super) const fn new(config: BackendConfig) -> Self {
        Self { _config: config }
    }
}

#[async_trait]
impl ProvingBackend for GenericZkvmBackend {
    fn backend_type(&self) -> BackendType {
        BackendType::GenericZkvm
    }

    async fn prove(&self, _request: &ProveBlockRequest) -> anyhow::Result<ProveResult> {
        Err(anyhow::anyhow!("GenericZkvmBackend::prove not yet implemented"))
    }

    async fn process_proof_request(
        &self,
        _proof_request: &ProofRequest,
        _repo: &ProofRequestRepo,
    ) -> anyhow::Result<ProofProcessingResult> {
        Err(anyhow::anyhow!("GenericZkvmBackend::process_proof_request not yet implemented"))
    }

    async fn get_session_status(&self, _session: &ProofSession) -> anyhow::Result<SessionStatus> {
        Err(anyhow::anyhow!("GenericZkvmBackend::get_session_status not yet implemented"))
    }

    fn name(&self) -> &'static str {
        "generic_zkvm"
    }
}

/// Creates a [`GenericZkvmBackend`] from the provided configuration and returns
/// it wrapped behind the [`ProvingBackend`] trait.
pub async fn build_backend(config: BackendConfig) -> anyhow::Result<Arc<dyn ProvingBackend>> {
    info!("Building generic zkvm backend");
    Ok(Arc::new(GenericZkvmBackend::new(config)))
}
