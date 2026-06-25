//! Historical proofs RPC server implementation.

use std::time::Instant;

use alloy_eips::BlockId;
use alloy_primitives::Address;
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use alloy_serde::JsonStorageKey;
use async_trait::async_trait;
use base_execution_trie::{BaseProofsStorage, BaseProofsStore};
use jsonrpsee::proc_macros::rpc;
use jsonrpsee_core::RpcResult;
use jsonrpsee_types::error::{ErrorCode, ErrorObject};
use reth_provider::StateProofProvider;
use reth_rpc_api::eth::helpers::FullEthApi;

use crate::{metrics::EthApiExtMetrics, state::BaseStateProviderFactory};

/// Maximum number of storage keys accepted in a single `eth_getProof` request. Matches go-ethereum.
pub const MAX_PROOF_KEYS: usize = 1024;

/// Validates the storage-key count for `eth_getProof`.
#[derive(Debug)]
pub struct ProofKeyLimit;

impl ProofKeyLimit {
    /// Returns an `InvalidParams` error when `keys_len` exceeds [`MAX_PROOF_KEYS`].
    pub fn check(keys_len: usize) -> Result<(), ErrorObject<'static>> {
        if keys_len > MAX_PROOF_KEYS {
            return Err(ErrorObject::owned(
                ErrorCode::InvalidParams.code(),
                format!("too many storage keys: max {MAX_PROOF_KEYS}, got {keys_len}"),
                None::<()>,
            ));
        }
        Ok(())
    }
}

#[cfg_attr(not(test), rpc(server, namespace = "eth"))]
#[cfg_attr(test, rpc(server, client, namespace = "eth"))]
pub trait EthApiOverride {
    /// Returns the account and storage values of the specified account including the Merkle-proof.
    /// This call can be used to verify that the data you are pulling from is not tampered with.
    #[method(name = "getProof")]
    async fn get_proof(
        &self,
        address: Address,
        keys: Vec<JsonStorageKey>,
        block_number: Option<BlockId>,
    ) -> RpcResult<EIP1186AccountProofResponse>;
}

#[derive(Debug)]
/// Overrides applied to the `eth_` namespace of the RPC API for historical proofs `ExEx`.
pub struct EthApiExt<Eth, P> {
    state_provider_factory: BaseStateProviderFactory<Eth, P>,
}

impl<Eth, P> EthApiExt<Eth, P>
where
    Eth: FullEthApi + Send + Sync + 'static,
    ErrorObject<'static>: From<Eth::Error>,
    P: BaseProofsStore + Clone + 'static,
{
    /// Creates a new instance of the `EthApiExt`.
    pub const fn new(eth_api: Eth, preimage_store: BaseProofsStorage<P>) -> Self {
        Self { state_provider_factory: BaseStateProviderFactory::new(eth_api, preimage_store) }
    }
}

#[async_trait]
impl<Eth, P> EthApiOverrideServer for EthApiExt<Eth, P>
where
    Eth: FullEthApi + Send + Sync + 'static,
    ErrorObject<'static>: From<Eth::Error>,
    P: BaseProofsStore + Clone + 'static,
{
    async fn get_proof(
        &self,
        address: Address,
        keys: Vec<JsonStorageKey>,
        block_number: Option<BlockId>,
    ) -> RpcResult<EIP1186AccountProofResponse> {
        // Reject oversized batches before any DB access; each key triggers a trie traversal.
        ProofKeyLimit::check(keys.len())?;

        let start = Instant::now();
        EthApiExtMetrics::get_proof_requests().increment(1);

        let storage_keys = keys.iter().map(|key| key.as_b256()).collect::<Vec<_>>();

        let result = async {
            let proof = self
                .state_provider_factory
                .state_provider(block_number)
                .await
                .map_err(Into::into)?
                .proof(Default::default(), address, &storage_keys)
                .map_err(Into::into)?;

            Ok(proof.into_eip1186_response(keys))
        }
        .await;

        match &result {
            Ok(_) => {
                EthApiExtMetrics::get_proof_latency().record(start.elapsed().as_secs_f64());
                EthApiExtMetrics::get_proof_successful_responses().increment(1);
            }
            Err(_) => EthApiExtMetrics::get_proof_failures().increment(1),
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use jsonrpsee_types::error::ErrorCode;

    use super::{MAX_PROOF_KEYS, ProofKeyLimit};

    #[test]
    fn accepts_at_limit() {
        assert!(ProofKeyLimit::check(MAX_PROOF_KEYS).is_ok());
    }

    #[test]
    fn accepts_below_limit() {
        assert!(ProofKeyLimit::check(0).is_ok());
        assert!(ProofKeyLimit::check(1).is_ok());
        assert!(ProofKeyLimit::check(MAX_PROOF_KEYS - 1).is_ok());
    }

    #[test]
    fn rejects_above_limit() {
        let err = ProofKeyLimit::check(MAX_PROOF_KEYS + 1).expect_err("must reject");
        assert_eq!(err.code(), ErrorCode::InvalidParams.code());
        assert!(
            err.message().contains("too many storage keys"),
            "unexpected error message: {}",
            err.message()
        );
    }
}
