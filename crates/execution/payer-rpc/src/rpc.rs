//! `payer_getTerms` and `payer_sendTransaction` proxy.

use std::{fmt, sync::Arc};

use alloy_consensus::BlockHeader;
use base_common_chains::Upgrades;
use base_execution_rpc::{SequencerClient, SequencerClientError};
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
    types::{ErrorCode, ErrorObjectOwned},
};
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_storage_api::BlockReaderIdExt;
use serde_json::Value;
use tracing::{debug, warn};

use crate::{PayerRequestFilter, TermsCache};

/// Rejection returned when `payer_*` is called before Everest activates EIP-8430.
pub const PAYER_PRE_EVEREST_RPC_ERROR: &str = "payer methods require EIP-8430, which is gated \
     behind Everest; they are not accepted before Everest is active";

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
    cache: Option<Arc<TermsCache>>,
}

impl<Provider> fmt::Debug for PayerApiImpl<Provider> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PayerApiImpl").field("cache_get_terms", &self.cache.is_some()).finish()
    }
}

impl<Provider> PayerApiImpl<Provider> {
    /// Creates a proxy. `cache_get_terms` stores one shared `payer_getTerms` response.
    pub fn new(provider: Provider, client: SequencerClient, cache_get_terms: bool) -> Self {
        Self { provider, client, cache: cache_get_terms.then(|| Arc::new(TermsCache::new())) }
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

    /// Returns the latest block number once EIP-8430 is active.
    fn ensure_everest(&self) -> RpcResult<u64> {
        let header = self.provider.latest_header().map_err(|error| {
            warn!(error = %error, "failed to read latest header for payer fork gate");
            ErrorObjectOwned::owned(
                ErrorCode::InternalError.code(),
                "failed to resolve latest header",
                None::<()>,
            )
        })?;
        header
            .filter(|header| {
                self.provider.chain_spec().is_everest_active_at_timestamp(header.timestamp())
            })
            .map(|header| header.number())
            .ok_or_else(|| invalid_params(PAYER_PRE_EVEREST_RPC_ERROR))
    }

    async fn forward(&self, method: &'static str, request: &Value) -> RpcResult<Value> {
        self.client.request(method, (request,)).await.map_err(ErrorObjectOwned::from)
    }

    /// Serves `payer_getTerms` from `cache`, refetching once per block with
    /// concurrent misses sharing one upstream call.
    async fn cached_terms(
        &self,
        cache: &TermsCache,
        block: u64,
        request: &Value,
    ) -> RpcResult<Value> {
        if let Some(cached) = cache.get(block) {
            return Ok(TermsCache::order_for(request, cached));
        }
        let _refreshing = cache.refreshing().await;
        if let Some(cached) = cache.get(block) {
            return Ok(TermsCache::order_for(request, cached));
        }
        match self.client.request::<_, Value>("payer_getTerms", (request,)).await {
            Ok(response) => {
                cache.insert(block, &response);
                Ok(response)
            }
            Err(error) => {
                let SequencerClientError::HttpError(inner) = &error;
                if inner.is_error_resp() {
                    cache.clear();
                } else if let Some(stale) = cache.get_stale() {
                    warn!(error = %error, block, "payer unreachable; serving cached payer_getTerms");
                    return Ok(TermsCache::order_for(request, stale));
                }
                Err(error.into())
            }
        }
    }
}

#[async_trait]
impl<Provider> PayerApiServer for PayerApiImpl<Provider>
where
    Provider: BlockReaderIdExt + ChainSpecProvider + Clone + Send + Sync + 'static,
    Provider::ChainSpec: Upgrades + EthChainSpec,
{
    async fn get_terms(&self, request: Value) -> RpcResult<Value> {
        let block = self.ensure_everest()?;
        PayerRequestFilter::validate_get_terms(&request, self.chain_id()).map_err(|error| {
            debug!(error = %error, "rejected payer_getTerms before forward");
            invalid_params(error)
        })?;
        match &self.cache {
            Some(cache) => self.cached_terms(cache, block, &request).await,
            None => self.forward("payer_getTerms", &request).await,
        }
    }

    async fn send_transaction(&self, request: Value) -> RpcResult<Value> {
        let block = self.ensure_everest()?;
        PayerRequestFilter::validate_send_transaction(&request).map_err(|error| {
            debug!(error = %error, "rejected payer_sendTransaction before forward");
            invalid_params(error)
        })?;
        let result = self.forward("payer_sendTransaction", &request).await;
        if let (Err(error), Some(cache)) = (&result, &self.cache) {
            let data = error.data().and_then(|data| serde_json::from_str(data.get()).ok());
            cache.invalidate(block, data.as_ref());
        }
        result
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
    use base_execution_chainspec::BaseChainSpec;
    use base_execution_rpc::SequencerClient;
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
        use base_execution_chainspec::BaseChainSpecBuilder;

        MockEthProvider::<BasePrimitives>::new()
            .with_chain_spec(Arc::new(
                BaseChainSpecBuilder::base_mainnet().cobalt_activated().build(),
            ))
            .with_genesis_block()
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

    #[tokio::test]
    async fn rejects_get_terms_before_everest_without_forwarding() {
        let sequencer = MockServer::start();
        let mock = sequencer.mock(|when, then| {
            when.method(POST).path("/");
            then.status(500);
        });
        let client = SequencerClient::new_http_with_headers(sequencer.base_url(), vec![]).unwrap();
        let api = PayerApiImpl::new(pre_everest_provider(), client, false);

        let error = api.get_terms(terms_request(1)).await.expect_err("pre-everest rejection");

        assert_eq!(error.code(), ErrorCode::InvalidParams.code());
        assert_eq!(error.message(), PAYER_PRE_EVEREST_RPC_ERROR);
        mock.assert_calls(0);
    }

    #[tokio::test]
    async fn proxies_get_terms_and_shares_one_cached_response() {
        let sequencer = MockServer::start();
        let response = terms_response();
        let mock = sequencer.mock(|when, then| {
            when.method(POST).path("/").json_body(json!({
                "jsonrpc": "2.0",
                "id": 0,
                "method": "payer_getTerms",
                "params": [terms_request(1)]
            }));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({ "jsonrpc": "2.0", "id": 0, "result": response }));
        });
        let client = SequencerClient::new_http_with_headers(sequencer.base_url(), vec![]).unwrap();
        let api = PayerApiImpl::new(everest_provider(), client, true);

        let first = api.get_terms(terms_request(1)).await.unwrap();
        let second = api.get_terms(terms_request(2)).await.unwrap();

        assert_eq!(first, response);
        assert_eq!(second, response);
        mock.assert_calls(1);
    }

    fn rpc_result(then: httpmock::Then, result: Value) {
        then.status(200)
            .header("content-type", "application/json")
            .json_body(json!({ "jsonrpc": "2.0", "id": 0, "result": result }));
    }

    fn rpc_error(then: httpmock::Then, code: &str) {
        then.status(200).header("content-type", "application/json").json_body(json!({
            "jsonrpc": "2.0",
            "id": 0,
            "error": { "code": -32000, "message": "PAYER_REJECTED", "data": { "code": code, "message": "rejected" } }
        }));
    }

    /// Rejects one `payer_sendTransaction` with `code`.
    async fn reject_send(
        api: &PayerApiImpl<
            impl BlockReaderIdExt
            + ChainSpecProvider<ChainSpec = Arc<BaseChainSpec>>
            + Clone
            + 'static,
        >,
        sequencer: &MockServer,
        code: &str,
    ) {
        let mut mock = sequencer.mock(|when, then| {
            when.method(POST).body_includes("payer_sendTransaction");
            rpc_error(then, code);
        });
        api.send_transaction(json!({ "transaction": "0x02" })).await.expect_err("rejected");
        mock.delete();
    }

    #[tokio::test]
    async fn refetches_terms_after_a_requote_and_serves_them_while_the_payer_is_unreachable() {
        let sequencer = MockServer::start();
        let mut terms = sequencer.mock(|when, then| {
            when.method(POST).body_includes("payer_getTerms");
            rpc_result(then, terms_response());
        });
        let client = SequencerClient::new_http_with_headers(sequencer.base_url(), vec![]).unwrap();
        let api = PayerApiImpl::new(everest_provider(), client, true);
        api.get_terms(terms_request(1)).await.unwrap();

        reject_send(&api, &sequencer, "PAYMENT_INSUFFICIENT").await;
        api.get_terms(terms_request(1)).await.unwrap();
        terms.assert_calls(2);

        terms.delete();
        let unreachable = sequencer.mock(|when, then| {
            when.method(POST).body_includes("payer_getTerms");
            then.status(502).body("bad gateway");
        });
        reject_send(&api, &sequencer, "TEMPORARILY_UNAVAILABLE").await;
        assert_eq!(api.get_terms(terms_request(1)).await.unwrap(), terms_response());
        unreachable.assert_calls(0);
    }

    #[tokio::test]
    async fn stops_serving_cached_terms_once_the_payer_rejects_get_terms() {
        let sequencer = MockServer::start();
        let mut terms = sequencer.mock(|when, then| {
            when.method(POST).body_includes("payer_getTerms");
            rpc_result(then, terms_response());
        });
        let client = SequencerClient::new_http_with_headers(sequencer.base_url(), vec![]).unwrap();
        let api = PayerApiImpl::new(everest_provider(), client, true);
        api.get_terms(terms_request(1)).await.unwrap();
        terms.delete();

        let mut paused = sequencer.mock(|when, then| {
            when.method(POST).body_includes("payer_getTerms");
            rpc_error(then, "TEMPORARILY_UNAVAILABLE");
        });
        reject_send(&api, &sequencer, "TEMPORARILY_UNAVAILABLE").await;
        let error = api.get_terms(terms_request(1)).await.expect_err("paused");
        assert_eq!(error.code(), -32000);
        paused.delete();

        sequencer.mock(|when, then| {
            when.method(POST).body_includes("payer_getTerms");
            then.status(502).body("bad gateway");
        });
        assert!(api.get_terms(terms_request(1)).await.is_err());
    }

    #[tokio::test]
    async fn orders_cached_tokens_by_each_request_preference() {
        let sequencer = MockServer::start();
        let usdc = "0x00000000000000000000000000000000000000aa";
        let response = json!({
            "options": [{
                "kind": "token",
                "ttl": 60,
                "tokens": [
                    { "token": "native", "symbol": "ETH", "decimals": 18, "rate": "0x1" },
                    { "token": usdc, "symbol": "USDC", "decimals": 6, "rate": "0x2" }
                ]
            }]
        });
        let mock = sequencer.mock(|when, then| {
            when.method(POST).body_includes("payer_getTerms");
            rpc_result(then, response.clone());
        });
        let client = SequencerClient::new_http_with_headers(sequencer.base_url(), vec![]).unwrap();
        let api = PayerApiImpl::new(everest_provider(), client, true);
        api.get_terms(terms_request(1)).await.unwrap();

        let mut request = terms_request(2);
        request["preferredTokens"] = json!([usdc]);
        let ordered = api.get_terms(request).await.unwrap();

        assert_eq!(ordered["options"][0]["tokens"][0]["symbol"], "USDC");
        mock.assert_calls(1);
    }

    #[tokio::test]
    async fn forwards_each_get_terms_when_the_cache_is_disabled() {
        let sequencer = MockServer::start();
        let response = terms_response();
        let mock = sequencer.mock(|when, then| {
            when.method(POST).path("/");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({ "jsonrpc": "2.0", "id": 0, "result": response }));
        });
        let client = SequencerClient::new_http_with_headers(sequencer.base_url(), vec![]).unwrap();
        let api = PayerApiImpl::new(everest_provider(), client, false);

        api.get_terms(terms_request(1)).await.unwrap();
        api.get_terms(terms_request(2)).await.unwrap();

        mock.assert_calls(2);
    }

    #[tokio::test]
    async fn proxies_send_transaction_without_caching() {
        let sequencer = MockServer::start();
        let request = json!({ "transaction": "0x02" });
        let mock = sequencer.mock(|when, then| {
            when.method(POST).path("/");
            then.status(200).header("content-type", "application/json").json_body(json!({
                "jsonrpc": "2.0",
                "id": 0,
                "result": { "transactionHash": "0x1111111111111111111111111111111111111111111111111111111111111111" }
            }));
        });
        let client = SequencerClient::new_http_with_headers(sequencer.base_url(), vec![]).unwrap();
        let api = PayerApiImpl::new(everest_provider(), client, true);

        let result = api.send_transaction(request.clone()).await.unwrap();
        api.send_transaction(request).await.unwrap();

        assert_eq!(
            result["transactionHash"],
            "0x1111111111111111111111111111111111111111111111111111111111111111"
        );
        mock.assert_calls(2);
    }

    #[test]
    fn registers_only_the_enabled_payer_methods() {
        let client = SequencerClient::new_http_with_headers("http://127.0.0.1:9", vec![]).unwrap();
        let module = PayerApiImpl::new(everest_provider(), client, false).into_rpc();
        let names: Vec<_> = module.method_names().collect();

        assert!(names.contains(&"payer_getTerms"));
        assert!(names.contains(&"payer_sendTransaction"));
        assert!(!names.contains(&"payer_signTransaction"));
        assert!(!names.contains(&"payer_getSponsorshipBalance"));
    }
}
