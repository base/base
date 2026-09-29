//! Forwarding of `payer_*` from query nodes to the sequencer.

use alloy_json_rpc::{RpcError, RpcRecv, RpcSend};
use base_execution_rpc::SequencerClient;
use jsonrpsee::{
    core::{RpcResult, async_trait},
    types::ErrorObjectOwned,
};
use tracing::warn;

use crate::{
    GetTermsParams, GetTermsResult, PayerApiServer, PayerErrorCode, PayerRejection,
    SendTransactionParams, SendTransactionResult,
};

/// Serves `payer_*` by forwarding each call to the sequencer's payer.
///
/// Rejections from the sequencer reach the caller unchanged. A sequencer that
/// cannot be reached is reported as `TEMPORARILY_UNAVAILABLE`, which callers
/// retry.
#[derive(Debug, Clone)]
pub struct PayerProxy {
    sequencer: SequencerClient,
}

impl PayerProxy {
    /// Creates a proxy forwarding to `sequencer`.
    pub const fn new(sequencer: SequencerClient) -> Self {
        Self { sequencer }
    }

    async fn forward<Params: RpcSend, Resp: RpcRecv>(
        &self,
        method: &'static str,
        params: Params,
    ) -> RpcResult<Resp> {
        self.sequencer.client().request(method, params).await.map_err(|error| match error {
            RpcError::ErrorResp(payload) => {
                ErrorObjectOwned::owned(payload.code as i32, payload.message, payload.data)
            }
            error => {
                warn!(method, error = %error, "failed to forward payer request to sequencer");
                PayerRejection::new(PayerErrorCode::TemporarilyUnavailable, "sequencer unreachable")
                    .into()
            }
        })
    }
}

#[async_trait]
impl PayerApiServer for PayerProxy {
    async fn get_terms(&self, params: GetTermsParams) -> RpcResult<GetTermsResult> {
        self.forward("payer_getTerms", (params,)).await
    }

    async fn send_transaction(
        &self,
        params: SendTransactionParams,
    ) -> RpcResult<SendTransactionResult> {
        self.forward("payer_sendTransaction", (params,)).await
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use jsonrpsee::server::Server;

    use super::*;
    use crate::Requote;

    /// Sequencer payer that requotes every call.
    struct Requoting;

    impl Requoting {
        fn rejection() -> PayerRejection {
            PayerRejection {
                requote: Some(Box::new(Requote {
                    token: Address::repeat_byte(0x83),
                    payment_amount: U256::from(210_000),
                    rate: U256::from(2_000_000_000u64),
                    ttl: 15,
                })),
                ..PayerRejection::new(PayerErrorCode::PaymentInsufficient, "requote")
            }
        }
    }

    #[async_trait]
    impl PayerApiServer for Requoting {
        async fn get_terms(&self, _params: GetTermsParams) -> RpcResult<GetTermsResult> {
            Err(Self::rejection().into())
        }

        async fn send_transaction(
            &self,
            _params: SendTransactionParams,
        ) -> RpcResult<SendTransactionResult> {
            Err(Self::rejection().into())
        }
    }

    fn rejection(error: &ErrorObjectOwned) -> PayerRejection {
        assert_eq!(error.code(), PayerRejection::RPC_CODE);
        serde_json::from_str(error.data().unwrap().get()).unwrap()
    }

    #[tokio::test]
    async fn forwards_sequencer_rejections_unchanged() {
        let server = Server::builder().build("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", server.local_addr().unwrap());
        let handle = server.start(Requoting.into_rpc());
        let proxy =
            PayerProxy::new(SequencerClient::new_http_with_headers(url, Vec::new()).unwrap());

        let error = proxy.get_terms(GetTermsParams::default()).await.unwrap_err();

        assert_eq!(rejection(&error), Requoting::rejection());
        handle.stop().unwrap();
    }

    #[tokio::test]
    async fn reports_unreachable_sequencer_as_temporarily_unavailable() {
        let server = Server::builder().build("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", server.local_addr().unwrap());
        drop(server);
        let proxy =
            PayerProxy::new(SequencerClient::new_http_with_headers(url, Vec::new()).unwrap());

        let error = proxy.get_terms(GetTermsParams::default()).await.unwrap_err();

        assert_eq!(rejection(&error).code, PayerErrorCode::TemporarilyUnavailable);
    }
}
