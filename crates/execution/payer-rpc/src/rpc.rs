//! `payer_getTerms` and `payer_sendTransaction` proxy.

use std::{fmt, sync::Arc, time::Instant};

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

use crate::{PayerChainState, PayerRejection, PayerRequestFilter, PayerTransaction, TermsCache};

/// Rejection returned when `payer_*` is called before Everest activates EIP-8430.
pub const PAYER_PRE_EVEREST_RPC_ERROR: &str = "payer methods require EIP-8430, which is gated \
     behind Everest; they are not accepted before Everest is active";

/// Error for a `payer_getTerms` that queued behind a refetch which could not reach the payer.
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
    /// Creates a proxy that checks each `payer_sendTransaction` against `state`.
    /// `cache_get_terms` stores one shared `payer_getTerms` response.
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
            ErrorObjectOwned::owned(
                ErrorCode::InternalError.code(),
                "failed to resolve latest header",
                None::<()>,
            )
        })?;
        header
            .map(|header| header.timestamp())
            .filter(|timestamp| {
                self.provider.chain_spec().is_everest_active_at_timestamp(*timestamp)
            })
            .ok_or_else(|| invalid_params(PAYER_PRE_EVEREST_RPC_ERROR))
    }

    /// Rejects a transaction the payer would reject, without forwarding it: one that does not
    /// decode, does not leave a payer to co-sign, lacks a valid sender signature, reuses a
    /// nonce, or whose phase 0 fails at the latest block.
    ///
    /// A state read that fails passes the transaction on, since the payer checks it too.
    async fn check_transaction(&self, raw: &[u8], timestamp: u64) -> RpcResult<()> {
        let keystore = self.provider.chain_spec().is_zenith_active_at_timestamp(timestamp);
        let transaction =
            PayerTransaction::decode(raw, self.chain_id(), keystore).map_err(|error| {
                debug!(error = %error, "rejected payer_sendTransaction transaction before forward");
                PayerRejection::invalid_transaction(error)
            })?;
        if let Some((sender, nonce_key)) = transaction.nonce_channel() {
            match self.state.nonce(sender, nonce_key).await {
                Ok(next) => transaction.check_nonce(next).map_err(|error| {
                    debug!(error = %error, "rejected payer_sendTransaction nonce before forward");
                    PayerRejection::invalid_transaction(error)
                })?,
                Err(error) => {
                    warn!(error = %error, "payer_sendTransaction nonce could not be read");
                }
            }
        }
        let Err(error) = self.state.simulate(transaction.phase_zero_request()).await else {
            return Ok(());
        };
        let Some(rejection) = PayerRejection::for_phase_zero(&error) else {
            warn!(error = %error, "payer_sendTransaction phase 0 could not be simulated");
            return Ok(());
        };
        debug!(error = %error, "payer_sendTransaction phase 0 failed before forward");
        Err(rejection)
    }

    async fn forward(&self, method: &'static str, request: &Value) -> RpcResult<Value> {
        self.client.request(method, (request,)).await.map_err(ErrorObjectOwned::from)
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
                    ErrorObjectOwned::owned(
                        ErrorCode::InternalError.code(),
                        PAYER_UNREACHABLE_RPC_ERROR,
                        None::<()>,
                    )
                });
        }
        // The cached body is shared, so it must carry the payer's own token order rather than
        // the order this caller's `preferredTokens` would produce.
        let mut shared_request = request.clone();
        if let Some(object) = shared_request.as_object_mut() {
            object.remove("preferredTokens");
        }
        let result = self.client.request::<_, Value>("payer_getTerms", (&shared_request,)).await;
        *unreachable_at = None;
        match result {
            Ok(response) => {
                cache.insert(&response);
                Ok(TermsCache::order_for(request, response))
            }
            Err(error) => {
                let SequencerClientError::HttpError(inner) = &error;
                if inner.is_error_resp() {
                    cache.clear();
                    return Err(error.into());
                }
                *unreachable_at = Some(Instant::now());
                if let Some(stale) = cache.get_stale() {
                    warn!(error = %error, "payer unreachable; serving cached payer_getTerms");
                    return Ok(TermsCache::order_for(request, stale));
                }
                Err(error.into())
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
    use alloy_rpc_types_eth::error::EthRpcErrorCode;
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_chains::ChainConfig;
    use base_common_consensus::{BasePrimitives, BaseTxEnvelope, Call, Eip8130Signed, TxEip8130};
    use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
    use base_execution_rpc::SequencerClient;
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

    fn client(sequencer: &MockServer) -> SequencerClient {
        SequencerClient::new_http_with_headers(sequencer.base_url(), vec![]).unwrap()
    }

    fn state_error(code: i32, message: &'static str) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(code, message, Some("0x"))
    }

    /// State under which every nonce is unused and every phase 0 succeeds.
    fn passing_state() -> MockPayerChainState {
        let mut state = MockPayerChainState::new();
        state.expect_nonce().returning(|_, _| Ok(U256::ZERO));
        state.expect_simulate().returning(|_| Ok(()));
        state
    }

    /// State under which every phase 0 fails with `code`.
    fn failing_simulation(code: i32, message: &'static str) -> MockPayerChainState {
        let mut state = MockPayerChainState::new();
        state.expect_nonce().returning(|_, _| Ok(U256::ZERO));
        state.expect_simulate().returning(move |_| Err(state_error(code, message)));
        state
    }

    /// A `payer_sendTransaction` request for an EOA-signed transaction paid by `payer`.
    fn send_request(payer: Option<Address>) -> Value {
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

    fn sponsored_request() -> Value {
        send_request(Some(Address::repeat_byte(0xaa)))
    }

    fn rejection_data(error: &ErrorObjectOwned) -> Value {
        assert_eq!(error.code(), PayerRejection::CODE);
        serde_json::from_str(error.data().expect("rejection data").get()).unwrap()
    }

    fn pre_everest_provider() -> MockEthProvider<BasePrimitives, Arc<BaseChainSpec>> {
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

    /// Has the payer reject one `payer_sendTransaction` with `code`.
    async fn reject_send(api: &TestApi, sequencer: &MockServer, code: &str) {
        let mut mock = sequencer.mock(|when, then| {
            when.method(POST).body_includes("payer_sendTransaction");
            rpc_error(then, code);
        });
        api.send_transaction(sponsored_request()).await.expect_err("rejected");
        mock.assert_calls(1);
        mock.delete();
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
            when.method(POST).body_includes("payer_getTerms").body_excludes("preferredTokens");
            rpc_result(then, response.clone());
        });
        let api = PayerApiImpl::new(everest_provider(), client(&sequencer), passing_state(), true);

        let mut prefers_usdc = terms_request(1);
        prefers_usdc["preferredTokens"] = json!([usdc]);
        let first = api.get_terms(prefers_usdc).await.unwrap();
        let no_preference = api.get_terms(terms_request(2)).await.unwrap();

        assert_eq!(first["options"][0]["tokens"][0]["symbol"], "USDC");
        assert_eq!(no_preference["options"][0]["tokens"][0]["symbol"], "ETH");
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
        let api = PayerApiImpl::new(everest_provider(), client(&sequencer), passing_state(), false);

        api.get_terms(terms_request(1)).await.unwrap();
        api.get_terms(terms_request(2)).await.unwrap();

        mock.assert_calls(2);
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
            assert_eq!(served.unwrap(), terms_response());
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

        let error = api.send_transaction(send_request(None)).await.expect_err("no payer");

        assert_eq!(rejection_data(&error)["code"], "INVALID_TRANSACTION");
        mock.assert_calls(0);
    }

    #[tokio::test]
    async fn rejects_a_transaction_whose_first_phase_reverts_without_forwarding() {
        let sequencer = MockServer::start();
        let mock = sequencer.mock(|when, then| {
            when.method(POST);
            then.status(500);
        });
        let state =
            failing_simulation(EthRpcErrorCode::ExecutionError.code(), "execution reverted");
        let api = PayerApiImpl::new(everest_provider(), client(&sequencer), state, false);

        let error = api.send_transaction(sponsored_request()).await.expect_err("reverted");

        let data = rejection_data(&error);
        assert_eq!(data["code"], "EXECUTION_REVERTED");
        assert_eq!(data["revert"], json!({ "phase": 0, "data": "0x" }));
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

        let error = api.send_transaction(sponsored_request()).await.expect_err("replayed");

        let data = rejection_data(&error);
        assert_eq!(data["code"], "INVALID_TRANSACTION");
        assert_eq!(data["reason"], PayerTransactionError::NonceUsed.to_string());
        mock.assert_calls(0);
    }

    #[tokio::test]
    async fn forwards_a_transaction_whose_state_cannot_be_read() {
        let sequencer = MockServer::start();
        let mock = sequencer.mock(|when, then| {
            when.method(POST).body_includes("payer_sendTransaction");
            rpc_result(then, json!({ "transactionHash": format!("0x{}", "11".repeat(32)) }));
        });
        let unavailable = || state_error(ErrorCode::InternalError.code(), "state unavailable");
        let mut state = MockPayerChainState::new();
        state.expect_nonce().returning(move |_, _| Err(unavailable()));
        state.expect_simulate().returning(move |_| Err(unavailable()));
        let api = PayerApiImpl::new(everest_provider(), client(&sequencer), state, false);

        api.send_transaction(sponsored_request()).await.unwrap();

        mock.assert_calls(1);
    }

    #[tokio::test]
    async fn proxies_send_transaction_without_caching() {
        let sequencer = MockServer::start();
        let request = sponsored_request();
        let mock = sequencer.mock(|when, then| {
            when.method(POST).path("/");
            then.status(200).header("content-type", "application/json").json_body(json!({
                "jsonrpc": "2.0",
                "id": 0,
                "result": { "transactionHash": "0x1111111111111111111111111111111111111111111111111111111111111111" }
            }));
        });
        let api = PayerApiImpl::new(everest_provider(), client(&sequencer), passing_state(), true);

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
