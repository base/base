//! `payer_getTerms` and `payer_sendTransaction` proxy.

use std::{fmt, sync::Arc, time::Instant};

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

use crate::{PayerChainState, PayerRejection, PayerRequestFilter, PayerTransaction, TermsCache};

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

    /// Checks a sender-signed transaction and forwards it for the payer to co-sign and submit.
    #[method(name = "sendTransaction")]
    async fn send_transaction(&self, request: Value) -> RpcResult<Value>;
}

/// Proxies payer methods to the upstream payer service.
pub struct PayerApiImpl<Provider, State> {
    provider: Provider,
    client: SequencerClient,
    state: State,
    cache: Option<Arc<TermsCache>>,
}

impl<Provider, State> fmt::Debug for PayerApiImpl<Provider, State> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PayerApiImpl").field("cache_get_terms", &self.cache.is_some()).finish()
    }
}

impl<Provider, State> PayerApiImpl<Provider, State> {
    /// Creates a proxy that forwards payer methods through `client` and checks each
    /// `payer_sendTransaction` against `state`. `cache_get_terms` stores one shared
    /// `payer_getTerms` response.
    pub fn new(
        provider: Provider,
        client: SequencerClient,
        state: State,
        cache_get_terms: bool,
    ) -> Self {
        Self {
            provider,
            client,
            state,
            cache: cache_get_terms.then(|| Arc::new(TermsCache::new())),
        }
    }
}

impl<Provider, State> PayerApiImpl<Provider, State>
where
    Provider: BlockReaderIdExt + ChainSpecProvider + Clone,
    Provider::ChainSpec: Upgrades + EthChainSpec,
    State: PayerChainState,
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

    /// Rejects a transaction the payer would reject as `INVALID_TRANSACTION`, without forwarding
    /// it: one that does not decode, does not leave a payer to co-sign, lacks a valid sender
    /// signature, or reuses a nonce.
    ///
    /// Execution is left to the payer, which simulates before co-signing and answers a failed
    /// payment with a more specific rejection than the node could, such as
    /// `SENDER_BALANCE_INSUFFICIENT` with the sender's shortfall. A nonce that cannot be read
    /// passes the transaction on, since the payer checks it too.
    async fn check_transaction(&self, raw: &[u8], timestamp: u64) -> RpcResult<()> {
        let keystore = self.provider.chain_spec().is_zenith_active_at_timestamp(timestamp);
        let transaction =
            PayerTransaction::decode(raw, self.chain_id(), keystore).map_err(|error| {
                debug!(error = %error, "rejected payer_sendTransaction transaction before forward");
                PayerRejection::invalid_transaction(error)
            })?;
        let Some((sender, nonce_key)) = transaction.nonce_channel() else {
            return Ok(());
        };
        match self.state.nonce(sender, nonce_key).await {
            Ok(next) => transaction.check_nonce(next).map_err(|error| {
                debug!(error = %error, "rejected payer_sendTransaction nonce before forward");
                PayerRejection::invalid_transaction(error)
            }),
            Err(error) => {
                warn!(error = %error, "payer_sendTransaction nonce could not be read");
                Ok(())
            }
        }
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

    /// Serves `payer_getTerms` from `cache`, refetching it on demand with
    /// concurrent misses sharing one upstream call.
    async fn cached_terms(&self, cache: &TermsCache, request: &Value) -> RpcResult<Value> {
        if let Some(cached) = cache.get() {
            return Ok(TermsCache::order_for(request, cached));
        }
        let queued_at = Instant::now();
        let mut unreachable_at = cache.refreshing().await;
        if let Some(cached) = cache.get() {
            return Ok(TermsCache::order_for(request, cached));
        }
        if unreachable_at.is_some_and(|failed_at| failed_at >= queued_at) {
            return cache
                .get_stale()
                .map(|stale| TermsCache::order_for(request, stale))
                .ok_or_else(|| {
                    PayerRejection::temporarily_unavailable(PAYER_UNREACHABLE_RPC_ERROR)
                });
        }
        // The cached body is shared, so it must carry every offer kind in the payer's own token
        // order and currency rather than what this caller's hints would produce.
        let mut shared_request = request.clone();
        if let Some(object) = shared_request.as_object_mut() {
            object.remove("preferredTokens");
            object.remove("kinds");
            object.remove("fiatCurrency");
        }
        let result = self.client.request::<_, Value>("payer_getTerms", (&shared_request,)).await;
        *unreachable_at = None;
        match result {
            Ok(response) => {
                cache.insert(&response);
                Ok(TermsCache::order_for(request, response))
            }
            Err(error) => {
                if let Some(answer) = PayerRejection::from_upstream(&error) {
                    cache.clear_on(&answer);
                    return Err(answer);
                }
                *unreachable_at = Some(Instant::now());
                if let Some(stale) = cache.get_stale() {
                    warn!(error = %error, "payer unreachable; serving cached payer_getTerms");
                    return Ok(TermsCache::order_for(request, stale));
                }
                warn!(error = %error, "payer unreachable with no cached payer_getTerms");
                Err(PayerRejection::temporarily_unavailable(PAYER_UNREACHABLE_RPC_ERROR))
            }
        }
    }
}

#[async_trait]
impl<Provider, State> PayerApiServer for PayerApiImpl<Provider, State>
where
    Provider: BlockReaderIdExt + ChainSpecProvider + Clone + Send + Sync + 'static,
    Provider::ChainSpec: Upgrades + EthChainSpec,
    State: PayerChainState,
{
    async fn get_terms(&self, request: Value) -> RpcResult<Value> {
        self.everest_timestamp()?;
        PayerRequestFilter::validate_get_terms(&request, self.chain_id()).map_err(|error| {
            debug!(error = %error, "rejected payer_getTerms before forward");
            invalid_params(error)
        })?;
        match &self.cache {
            Some(cache) => self.cached_terms(cache, &request).await,
            None => self.forward("payer_getTerms", &request).await,
        }
    }

    async fn send_transaction(&self, request: Value) -> RpcResult<Value> {
        let timestamp = self.everest_timestamp()?;
        let raw = PayerRequestFilter::validate_send_transaction(&request).map_err(|error| {
            debug!(error = %error, "rejected payer_sendTransaction before forward");
            invalid_params(error)
        })?;
        self.check_transaction(&raw, timestamp).await?;
        let result = self.forward("payer_sendTransaction", &request).await;
        if let (Err(error), Some(cache)) = (&result, &self.cache) {
            let data = error.data().and_then(|data| serde_json::from_str(data.get()).ok());
            cache.invalidate(data.as_ref());
        }
        result
    }
}

fn invalid_params(error: impl ToString) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(ErrorCode::InvalidParams.code(), error.to_string(), None::<()>)
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use alloy_eips::Encodable2718;
    use alloy_primitives::{Address, Bytes, U256, hex};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_chains::ChainConfig;
    use base_common_consensus::{BasePrimitives, BaseTxEnvelope, Call, Eip8130Signed, TxEip8130};
    use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
    use base_test_utils::build_test_genesis_everest;
    use httpmock::prelude::*;
    use reth_provider::test_utils::MockEthProvider;
    use serde_json::json;

    use super::*;
    use crate::{PayerTransactionError, state::MockPayerChainState};

    type TestApi =
        PayerApiImpl<MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>, MockPayerChainState>;

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

    fn terms_request(sender_byte: u8) -> Value {
        json!({
            "chainId": "0x2105",
            "sender": format!("0x{:040x}", sender_byte),
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

    fn state_error(code: i32, message: &'static str) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(code, message, Some("0x"))
    }

    /// State under which every nonce is unused.
    fn passing_state() -> MockPayerChainState {
        let mut state = MockPayerChainState::new();
        state.expect_nonce().returning(|_, _| Ok(U256::ZERO));
        state
    }

    /// A `payer_sendTransaction` request for an EOA-signed transaction paid by `payer`.
    fn send_request_paid_by(payer: Option<Address>) -> Value {
        let signer = PrivateKeySigner::random();
        let call = Call { to: Address::repeat_byte(0x11), value: U256::ZERO, data: Bytes::new() };
        let tx = TxEip8130 {
            chain_id: ChainConfig::mainnet().chain_id,
            max_fee_per_gas: 1,
            gas_limit: 100_000,
            calls: vec![vec![call]],
            payer,
            ..Default::default()
        };
        let sender_auth = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap().as_bytes();
        let signed = Eip8130Signed::new(tx, Bytes::from(sender_auth.to_vec()), Bytes::new());
        json!({ "transaction": hex::encode_prefixed(BaseTxEnvelope::Eip8130(signed).encoded_2718()) })
    }

    fn send_request() -> Value {
        send_request_paid_by(Some(Address::repeat_byte(0xaa)))
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

    /// Asserts `served` is [`terms_response`], with its `ttl` counted down by at most a few
    /// seconds of cache age.
    fn assert_terms(mut served: Value) {
        let ttl = served["options"][0]["ttl"].as_u64().expect("ttl");
        assert!((55..=60).contains(&ttl), "ttl {ttl}");
        served["options"][0]["ttl"] = 60.into();
        assert_eq!(served, terms_response());
    }

    fn rpc_error(then: httpmock::Then, code: &str) {
        then.status(200).header("content-type", "application/json").json_body(json!({
            "jsonrpc": "2.0",
            "id": 0,
            "error": { "code": -32000, "message": "PAYER_REJECTED", "data": { "code": code, "message": "rejected" } }
        }));
    }

    /// Has the payer reject one `payer_sendTransaction` with `code`.
    async fn reject_send(api: &TestApi, sequencer: &MockServer, code: &str) {
        let mut mock = sequencer.mock(|when, then| {
            when.method(POST).body_includes("payer_sendTransaction");
            rpc_error(then, code);
        });
        api.send_transaction(send_request()).await.expect_err("rejected");
        mock.assert_calls(1);
        mock.delete();
    }

    #[tokio::test]
    async fn rejects_get_terms_before_everest_without_forwarding() {
        let sequencer = MockServer::start();
        let mock = sequencer.mock(|when, then| {
            when.method(POST).path("/");
            then.status(500);
        });
        let api = PayerApiImpl::new(
            pre_everest_provider(),
            client(&sequencer),
            MockPayerChainState::new(),
            false,
        );

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
        let api = PayerApiImpl::new(everest_provider(), client(&sequencer), passing_state(), false);

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
        for cache_get_terms in [false, true] {
            let api = PayerApiImpl::new(
                everest_provider(),
                client(&sequencer),
                passing_state(),
                cache_get_terms,
            );
            let error = api.get_terms(terms_request(1)).await.expect_err("payer down");
            assert_eq!(rejection_data(&error)["code"], "TEMPORARILY_UNAVAILABLE");
        }
    }

    #[tokio::test]
    async fn forwards_each_send_transaction() {
        let sequencer = MockServer::start();
        let hash = format!("0x{}", "11".repeat(32));
        let mock = sequencer.mock(|when, then| {
            when.method(POST).body_includes("payer_sendTransaction");
            rpc_result(then, json!({ "transactionHash": hash }));
        });
        let api = PayerApiImpl::new(everest_provider(), client(&sequencer), passing_state(), false);

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
            let api =
                PayerApiImpl::new(everest_provider(), client(&sequencer), passing_state(), false);

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
        let api = PayerApiImpl::new(everest_provider(), client(&sequencer), passing_state(), false);
        let error = api.send_transaction(send_request()).await.expect_err("payer down");
        assert_eq!(rejection_data(&error)["code"], "TEMPORARILY_UNAVAILABLE");
        payer_down.delete();

        let unreachable =
            SequencerClient::new_http_with_headers("http://127.0.0.1:9", vec![]).unwrap();
        let api = PayerApiImpl::new(everest_provider(), unreachable, passing_state(), false);
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
        let api = PayerApiImpl::new(everest_provider(), client(&sequencer), passing_state(), false);
        let error = api.send_transaction(send_request()).await.expect_err("no payer");
        assert_eq!(error.code(), ErrorCode::MethodNotFound.code());
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
        let api = PayerApiImpl::new(everest_provider(), client(&sequencer), passing_state(), true);

        let first = api.get_terms(terms_request(1)).await.unwrap();
        let second = api.get_terms(terms_request(2)).await.unwrap();

        assert_eq!(first, response);
        assert_terms(second);
        mock.assert_calls(1);
    }

    #[tokio::test]
    async fn refetches_terms_after_a_payment_rejection_and_serves_them_while_the_payer_is_unreachable()
     {
        let sequencer = MockServer::start();
        let mut terms = sequencer.mock(|when, then| {
            when.method(POST).body_includes("payer_getTerms");
            rpc_result(then, terms_response());
        });
        let api = PayerApiImpl::new(everest_provider(), client(&sequencer), passing_state(), true);
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
        assert_terms(api.get_terms(terms_request(1)).await.unwrap());
        unreachable.assert_calls(0);
    }
    #[tokio::test]
    async fn stops_serving_cached_terms_once_the_payer_rejects_get_terms() {
        let sequencer = MockServer::start();
        let mut terms = sequencer.mock(|when, then| {
            when.method(POST).body_includes("payer_getTerms");
            rpc_result(then, terms_response());
        });
        let api = PayerApiImpl::new(everest_provider(), client(&sequencer), passing_state(), true);
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
    async fn serves_cached_terms_while_a_gateway_reports_the_payer_down() {
        let sequencer = MockServer::start();
        let mut terms = sequencer.mock(|when, then| {
            when.method(POST).body_includes("payer_getTerms");
            rpc_result(then, terms_response());
        });
        let api = PayerApiImpl::new(everest_provider(), client(&sequencer), passing_state(), true);
        api.get_terms(terms_request(1)).await.unwrap();
        reject_send(&api, &sequencer, "PAYMENT_INSUFFICIENT").await;
        terms.delete();
        let payer_down = sequencer.mock(|when, then| {
            when.method(POST).body_includes("payer_getTerms");
            gateway_payer_down(then);
        });

        assert_terms(api.get_terms(terms_request(1)).await.unwrap());
        payer_down.assert_calls(1);
    }
    #[tokio::test]
    async fn requests_queued_behind_an_unreachable_payer_without_cached_terms_are_unavailable() {
        let sequencer = MockServer::start();
        let slow = sequencer.mock(|when, then| {
            when.method(POST).body_includes("payer_getTerms");
            rpc_result(then.delay(Duration::from_secs(1)), terms_response());
        });
        let client = SequencerClient::new_http_with_headers_and_timeout(
            sequencer.base_url(),
            vec![],
            Duration::from_millis(200),
        )
        .unwrap();
        let api = PayerApiImpl::new(everest_provider(), client, passing_state(), true);

        let (first, second, third) = tokio::join!(
            api.get_terms(terms_request(1)),
            api.get_terms(terms_request(2)),
            api.get_terms(terms_request(3)),
        );

        for served in [first, second, third] {
            assert_eq!(rejection_data(&served.unwrap_err())["code"], "TEMPORARILY_UNAVAILABLE");
        }
        slow.assert_calls(1);
    }
    #[tokio::test]
    async fn caches_terms_without_request_hints_and_orders_tokens_per_request() {
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
            when.method(POST)
                .body_includes("payer_getTerms")
                .body_excludes("preferredTokens")
                .body_excludes("kinds")
                .body_excludes("fiatCurrency");
            rpc_result(then, response.clone());
        });
        let api = PayerApiImpl::new(everest_provider(), client(&sequencer), passing_state(), true);

        let mut prefers_usdc = terms_request(1);
        prefers_usdc["preferredTokens"] = json!([usdc]);
        prefers_usdc["kinds"] = json!(["token"]);
        prefers_usdc["fiatCurrency"] = json!("EUR");
        let first = api.get_terms(prefers_usdc).await.unwrap();
        let no_preference = api.get_terms(terms_request(2)).await.unwrap();

        assert_eq!(first["options"][0]["tokens"][0]["symbol"], "USDC");
        assert_eq!(no_preference["options"][0]["tokens"][0]["symbol"], "ETH");
        mock.assert_calls(1);
    }
    #[tokio::test]
    async fn requests_queued_behind_an_unreachable_payer_reuse_its_failure() {
        let sequencer = MockServer::start();
        let mut terms = sequencer.mock(|when, then| {
            when.method(POST).body_includes("payer_getTerms");
            rpc_result(then, terms_response());
        });
        let client = SequencerClient::new_http_with_headers_and_timeout(
            sequencer.base_url(),
            vec![],
            Duration::from_millis(200),
        )
        .unwrap();
        let api = PayerApiImpl::new(everest_provider(), client, passing_state(), true);
        api.get_terms(terms_request(1)).await.unwrap();
        reject_send(&api, &sequencer, "PAYMENT_INSUFFICIENT").await;
        terms.delete();
        let slow = sequencer.mock(|when, then| {
            when.method(POST).body_includes("payer_getTerms");
            rpc_result(then.delay(Duration::from_secs(1)), terms_response());
        });

        let (first, second, third) = tokio::join!(
            api.get_terms(terms_request(1)),
            api.get_terms(terms_request(2)),
            api.get_terms(terms_request(3)),
        );

        for served in [first, second, third] {
            assert_terms(served.unwrap());
        }
        slow.assert_calls(1);
    }

    #[tokio::test]
    async fn rejects_a_transaction_without_a_payer_without_forwarding() {
        let sequencer = MockServer::start();
        let mock = sequencer.mock(|when, then| {
            when.method(POST);
            then.status(500);
        });
        let api = PayerApiImpl::new(
            everest_provider(),
            client(&sequencer),
            MockPayerChainState::new(),
            false,
        );

        let error = api.send_transaction(send_request_paid_by(None)).await.expect_err("no payer");

        assert_eq!(rejection_data(&error)["code"], "INVALID_TRANSACTION");
        mock.assert_calls(0);
    }

    #[tokio::test]
    async fn rejects_a_transaction_whose_nonce_was_used_without_forwarding() {
        let sequencer = MockServer::start();
        let mock = sequencer.mock(|when, then| {
            when.method(POST);
            then.status(500);
        });
        let mut state = MockPayerChainState::new();
        state.expect_nonce().returning(|_, _| Ok(U256::from(1)));
        let api = PayerApiImpl::new(everest_provider(), client(&sequencer), state, false);

        let error = api.send_transaction(send_request()).await.expect_err("replayed");

        let data = rejection_data(&error);
        assert_eq!(data["code"], "INVALID_TRANSACTION");
        assert_eq!(data["reason"], PayerTransactionError::NonceUsed.to_string());
        mock.assert_calls(0);
    }

    #[tokio::test]
    async fn forwards_a_transaction_whose_nonce_cannot_be_read() {
        let sequencer = MockServer::start();
        let mock = sequencer.mock(|when, then| {
            when.method(POST).body_includes("payer_sendTransaction");
            rpc_result(then, json!({ "transactionHash": format!("0x{}", "11".repeat(32)) }));
        });
        let mut state = MockPayerChainState::new();
        state.expect_nonce().returning(|_, _| {
            Err(state_error(ErrorCode::InternalError.code(), "state unavailable"))
        });
        let api = PayerApiImpl::new(everest_provider(), client(&sequencer), state, false);

        api.send_transaction(send_request()).await.unwrap();

        mock.assert_calls(1);
    }

    #[test]
    fn registers_only_the_enabled_payer_methods() {
        let client = SequencerClient::new_http_with_headers("http://127.0.0.1:9", vec![]).unwrap();
        let module =
            PayerApiImpl::new(everest_provider(), client, MockPayerChainState::new(), false)
                .into_rpc();
        let names: Vec<_> = module.method_names().collect();

        assert!(names.contains(&"payer_getTerms"));
        assert!(names.contains(&"payer_sendTransaction"));
        assert!(!names.contains(&"payer_signTransaction"));
        assert!(!names.contains(&"payer_getSponsorshipBalance"));
    }
}
