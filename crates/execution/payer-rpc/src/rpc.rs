//! `payer_getTerms` and `payer_sendTransaction` proxy.

use std::fmt;

use alloy_consensus::BlockHeader;
use base_common_chains::Upgrades;
use base_execution_rpc::SequencerClient;
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
    types::{ErrorCode, ErrorObjectOwned},
};
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_storage_api::BlockReaderIdExt;
use serde_json::Value;
use tracing::{debug, warn};

use crate::{PayerRejection, PayerRequestFilter};

/// Rejection returned when `payer_*` is called before Everest activates EIP-8430.
pub const PAYER_PRE_EVEREST_RPC_ERROR: &str = "payer methods require EIP-8430, which is gated \
     behind Everest; they are not accepted before Everest is active";

/// `TEMPORARILY_UNAVAILABLE` reason for a request the payer did not answer.
pub const PAYER_UNREACHABLE_RPC_ERROR: &str = "payer unreachable";

/// Payer methods exposed on the execution RPC.
///
/// `payer_signTransaction` and `payer_getSponsorshipBalance` are intentionally
/// absent, so callers receive JSON-RPC method-not-found.
#[rpc(server, namespace = "payer")]
pub trait PayerApi {
    /// Quotes token-payment terms and forwards the request upstream.
    #[method(name = "getTerms")]
    async fn get_terms(&self, request: Value) -> RpcResult<Value>;

    /// Forwards a sender-signed transaction for the payer to co-sign and submit.
    #[method(name = "sendTransaction")]
    async fn send_transaction(&self, request: Value) -> RpcResult<Value>;
}

/// Proxies payer methods to the upstream payer service.
pub struct PayerApiImpl<Provider> {
    provider: Provider,
    client: SequencerClient,
}

impl<Provider> fmt::Debug for PayerApiImpl<Provider> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PayerApiImpl").finish_non_exhaustive()
    }
}

impl<Provider> PayerApiImpl<Provider> {
    /// Creates a proxy that forwards payer methods through `client`.
    pub const fn new(provider: Provider, client: SequencerClient) -> Self {
        Self { provider, client }
    }
}

impl<Provider> PayerApiImpl<Provider>
where
    Provider: BlockReaderIdExt + ChainSpecProvider + Clone,
    Provider::ChainSpec: Upgrades + EthChainSpec,
{
    fn chain_id(&self) -> u64 {
        self.provider.chain_spec().chain().id()
    }

    /// The latest block's timestamp, rejecting the call unless Everest is active at it.
    fn everest_timestamp(&self) -> RpcResult<u64> {
        let header = self.provider.latest_header().map_err(|error| {
            warn!(error = %error, "failed to read latest header for payer fork gate");
            PayerRejection::temporarily_unavailable("failed to resolve latest header")
        })?;
        header
            .map(|header| header.timestamp())
            .filter(|timestamp| {
                self.provider.chain_spec().is_everest_active_at_timestamp(*timestamp)
            })
            .ok_or_else(|| invalid_params(PAYER_PRE_EVEREST_RPC_ERROR))
    }

    /// Forwards `method`, rejecting it as `TEMPORARILY_UNAVAILABLE` when the payer does not
    /// answer. The payer's own errors pass through unchanged.
    async fn forward(&self, method: &'static str, request: &Value) -> RpcResult<Value> {
        self.client.request(method, (request,)).await.map_err(|error| {
            PayerRejection::from_upstream(&error).unwrap_or_else(|| {
                debug!(error = %error, method, "payer did not answer");
                PayerRejection::temporarily_unavailable(PAYER_UNREACHABLE_RPC_ERROR)
            })
        })
    }
}

#[async_trait]
impl<Provider> PayerApiServer for PayerApiImpl<Provider>
where
    Provider: BlockReaderIdExt + ChainSpecProvider + Clone + Send + Sync + 'static,
    Provider::ChainSpec: Upgrades + EthChainSpec,
{
    async fn get_terms(&self, request: Value) -> RpcResult<Value> {
        self.everest_timestamp()?;
        PayerRequestFilter::validate_get_terms(&request, self.chain_id()).map_err(|error| {
            debug!(error = %error, "rejected payer_getTerms before forward");
            invalid_params(error)
        })?;
        self.forward("payer_getTerms", &request).await
    }

    async fn send_transaction(&self, request: Value) -> RpcResult<Value> {
        self.everest_timestamp()?;
        PayerRequestFilter::validate_send_transaction(&request).map_err(|error| {
            debug!(error = %error, "rejected payer_sendTransaction before forward");
            invalid_params(error)
        })?;
        self.forward("payer_sendTransaction", &request).await
    }
}

fn invalid_params(error: impl ToString) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(ErrorCode::InvalidParams.code(), error.to_string(), None::<()>)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use base_common_chains::ChainConfig;
    use base_common_consensus::BasePrimitives;
    use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
    use base_test_utils::build_test_genesis_everest;
    use httpmock::prelude::*;
    use reth_provider::test_utils::MockEthProvider;
    use serde_json::json;

    use super::*;

    fn everest_provider() -> MockEthProvider<BasePrimitives, Arc<BaseChainSpec>> {
        let mut genesis = build_test_genesis_everest();
        genesis.config.chain_id = ChainConfig::mainnet().chain_id;
        MockEthProvider::<BasePrimitives>::new()
            .with_chain_spec(Arc::new(BaseChainSpec::from_genesis(genesis)))
            .with_genesis_block()
    }

    fn pre_everest_provider() -> MockEthProvider<BasePrimitives, Arc<BaseChainSpec>> {
        MockEthProvider::<BasePrimitives>::new()
            .with_chain_spec(Arc::new(
                BaseChainSpecBuilder::base_mainnet().cobalt_activated().build(),
            ))
            .with_genesis_block()
    }

    fn client(sequencer: &MockServer) -> SequencerClient {
        SequencerClient::new_http_with_headers(sequencer.base_url(), vec![]).unwrap()
    }

    fn terms_request(from_byte: u8) -> Value {
        json!({
            "chainId": "0x2105",
            "from": format!("0x{:040x}", from_byte),
            "calls": [{ "to": "0x0000000000000000000000000000000000000002" }]
        })
    }

    fn terms_response() -> Value {
        json!({
            "options": [{
                "kind": "token",
                "ttl": 60,
                "tokens": [{ "token": "native", "symbol": "ETH", "decimals": 18, "rate": "0x1" }]
            }]
        })
    }

    fn send_request() -> Value {
        json!({ "transaction": "0x7b01" })
    }

    fn rejection_data(error: &ErrorObjectOwned) -> Value {
        assert_eq!(error.code(), PayerRejection::CODE);
        serde_json::from_str(error.data().expect("rejection data").get()).unwrap()
    }

    fn rpc_result(then: httpmock::Then, result: Value) {
        then.status(200)
            .header("content-type", "application/json")
            .json_body(json!({ "jsonrpc": "2.0", "id": 0, "result": result }));
    }

    /// Answers as a gateway such as proxyd does when no payer backend is healthy.
    fn gateway_payer_down(then: httpmock::Then) {
        then.status(503).header("content-type", "application/json").json_body(json!({
            "jsonrpc": "2.0",
            "id": 0,
            "error": {
                "code": -32011,
                "message": "no backend is currently healthy to serve traffic"
            }
        }));
    }

    #[tokio::test]
    async fn rejects_get_terms_before_everest_without_forwarding() {
        let sequencer = MockServer::start();
        let mock = sequencer.mock(|when, then| {
            when.method(POST).path("/");
            then.status(500);
        });
        let api = PayerApiImpl::new(pre_everest_provider(), client(&sequencer));

        let error = api.get_terms(terms_request(1)).await.expect_err("pre-everest rejection");

        assert_eq!(error.code(), ErrorCode::InvalidParams.code());
        assert_eq!(error.message(), PAYER_PRE_EVEREST_RPC_ERROR);
        mock.assert_calls(0);
    }

    #[tokio::test]
    async fn forwards_each_get_terms() {
        let sequencer = MockServer::start();
        let mock = sequencer.mock(|when, then| {
            when.method(POST).body_includes("payer_getTerms");
            rpc_result(then, terms_response());
        });
        let api = PayerApiImpl::new(everest_provider(), client(&sequencer));

        assert_eq!(api.get_terms(terms_request(1)).await.unwrap(), terms_response());
        assert_eq!(api.get_terms(terms_request(1)).await.unwrap(), terms_response());

        mock.assert_calls(2);
    }

    #[tokio::test]
    async fn answers_get_terms_temporarily_unavailable_when_the_payer_does_not_answer() {
        let sequencer = MockServer::start();
        sequencer.mock(|when, then| {
            when.method(POST).body_includes("payer_getTerms");
            gateway_payer_down(then);
        });
        let api = PayerApiImpl::new(everest_provider(), client(&sequencer));

        let error = api.get_terms(terms_request(1)).await.expect_err("payer down");

        assert_eq!(rejection_data(&error)["code"], "TEMPORARILY_UNAVAILABLE");
    }

    #[tokio::test]
    async fn forwards_each_send_transaction() {
        let sequencer = MockServer::start();
        let hash = format!("0x{}", "11".repeat(32));
        let mock = sequencer.mock(|when, then| {
            when.method(POST).body_includes("payer_sendTransaction");
            rpc_result(then, json!({ "transactionHash": hash }));
        });
        let api = PayerApiImpl::new(everest_provider(), client(&sequencer));

        let result = api.send_transaction(send_request()).await.unwrap();
        api.send_transaction(send_request()).await.unwrap();

        assert_eq!(result["transactionHash"], hash);
        mock.assert_calls(2);
    }

    #[tokio::test]
    async fn passes_the_payer_rejection_through_unchanged_whatever_its_http_status() {
        let rejection = json!({ "code": "PAYMENT_INSUFFICIENT", "reason": "rate moved" });

        for status in [200, 400, 500] {
            let sequencer = MockServer::start();
            sequencer.mock(|when, then| {
                when.method(POST).body_includes("payer_sendTransaction");
                then.status(status).header("content-type", "application/json").json_body(json!({
                    "jsonrpc": "2.0",
                    "id": 0,
                    "error": { "code": -32000, "message": "PAYER_REJECTED", "data": rejection }
                }));
            });
            let api = PayerApiImpl::new(everest_provider(), client(&sequencer));

            let error = api.send_transaction(send_request()).await.expect_err("rejected");

            assert_eq!(rejection_data(&error), rejection, "status {status}");
        }
    }

    #[tokio::test]
    async fn answers_temporarily_unavailable_only_when_the_payer_does_not_answer() {
        let sequencer = MockServer::start();
        let mut payer_down = sequencer.mock(|when, then| {
            when.method(POST).body_includes("payer_sendTransaction");
            gateway_payer_down(then);
        });
        let api = PayerApiImpl::new(everest_provider(), client(&sequencer));
        let error = api.send_transaction(send_request()).await.expect_err("payer down");
        assert_eq!(rejection_data(&error)["code"], "TEMPORARILY_UNAVAILABLE");
        payer_down.delete();

        let unreachable =
            SequencerClient::new_http_with_headers("http://127.0.0.1:9", vec![]).unwrap();
        let api = PayerApiImpl::new(everest_provider(), unreachable);
        let error = api.send_transaction(send_request()).await.expect_err("unreachable");
        assert_eq!(rejection_data(&error)["code"], "TEMPORARILY_UNAVAILABLE");

        sequencer.mock(|when, then| {
            when.method(POST).body_includes("payer_sendTransaction");
            then.status(200).header("content-type", "application/json").json_body(json!({
                "jsonrpc": "2.0",
                "id": 0,
                "error": { "code": -32601, "message": "Method not found" }
            }));
        });
        let api = PayerApiImpl::new(everest_provider(), client(&sequencer));
        let error = api.send_transaction(send_request()).await.expect_err("no payer");
        assert_eq!(error.code(), ErrorCode::MethodNotFound.code());
    }

    #[test]
    fn registers_only_the_enabled_payer_methods() {
        let client = SequencerClient::new_http_with_headers("http://127.0.0.1:9", vec![]).unwrap();
        let module = PayerApiImpl::new(everest_provider(), client).into_rpc();
        let names: Vec<_> = module.method_names().collect();

        assert!(names.contains(&"payer_getTerms"));
        assert!(names.contains(&"payer_sendTransaction"));
        assert!(!names.contains(&"payer_signTransaction"));
        assert!(!names.contains(&"payer_getSponsorshipBalance"));
    }
}
