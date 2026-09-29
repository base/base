//! ERC-8168 `payer_*` JSON-RPC namespace.

use std::time::{SystemTime, UNIX_EPOCH};

use alloy_signer::Signer;
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
};
use reth_chainspec::ChainSpecProvider;
use reth_storage_api::{BlockReaderIdExt, StateProviderFactory};

use crate::{
    GetTermsParams, GetTermsResult, PayerService, SendTransactionParams, SendTransactionResult,
    ValidityIngress,
};

/// ERC-8168 payer methods served by the payer.
#[rpc(server, namespace = "payer")]
pub trait PayerApi {
    /// Quotes the tokens the payer accepts for an intent.
    #[method(name = "getTerms")]
    async fn get_terms(&self, params: GetTermsParams) -> RpcResult<GetTermsResult>;

    /// Co-signs a sender-signed transaction and submits it.
    #[method(name = "sendTransaction")]
    async fn send_transaction(
        &self,
        params: SendTransactionParams,
    ) -> RpcResult<SendTransactionResult>;
}

#[async_trait]
impl<Client, Ingress, S> PayerApiServer for PayerService<Client, Ingress, S>
where
    Client: StateProviderFactory + BlockReaderIdExt + ChainSpecProvider + 'static,
    Ingress: ValidityIngress,
    S: Signer + Send + Sync + 'static,
{
    async fn get_terms(&self, params: GetTermsParams) -> RpcResult<GetTermsResult> {
        self.terms(&params)
    }

    async fn send_transaction(
        &self,
        params: SendTransactionParams,
    ) -> RpcResult<SendTransactionResult> {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX));
        self.sponsor(&params.signed_transaction, now_ms).await
    }
}
