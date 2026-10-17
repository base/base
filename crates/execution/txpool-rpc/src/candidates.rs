//! `base_sendRawTransactionCandidates` ingress and its runtime kill switch.

use std::sync::Arc;

use alloy_consensus::BlockHeader;
use alloy_primitives::{B256, Bytes, Signature, TxHash};
use base_common_chains::Upgrades;
use base_execution_candidates::{
    CandidateEnvelope, CandidateError, CandidateGroup, CandidateRuntime, CandidateValidator,
    ExpiryWindow, GroupSummary, LedgerAdmission,
};
use base_execution_rpc::SequencerClient;
use base_execution_txpool::BasePooledTransaction;
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
    types::{ErrorCode, ErrorObjectOwned},
};
use reth_chainspec::ChainSpecProvider;
use reth_rpc_eth_types::EthApiError;
use reth_storage_api::{BlockReaderIdExt, StateProviderFactory};
use reth_transaction_pool::{TransactionOrigin, TransactionPool};
use serde::{Deserialize, Deserializer, Serialize};
use tracing::{debug, info};

use crate::BlockCadence;

/// Signed EIP-2718 siblings, in `envelope.siblingHashes` order.
///
/// Deserialization stops before allocating past
/// [`base_execution_candidates::MAX_SIBLINGS_CEILING`]; the configured limit is enforced by
/// [`CandidateValidator`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct CandidateSiblings(pub Vec<Bytes>);

impl<'de> Deserialize<'de> for CandidateSiblings {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        CandidateEnvelope::deserialize_bounded(deserializer).map(Self)
    }
}

/// Options for `base_sendRawTransactionCandidates`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SendRawTransactionCandidatesOptions {
    /// Signed `CandidateSet`.
    pub envelope: CandidateEnvelope,
    /// Sender's EIP-712 signature over the envelope.
    pub envelope_signature: Signature,
}

/// Response for an accepted (or exact-duplicate) candidate group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateSubmissionResponse {
    /// EIP-712 signing hash of the envelope; the group's identity.
    pub group_hash: B256,
    /// Pool and iterator lifecycle identity. Not the winner's identity.
    pub representative_hash: TxHash,
    /// Every sibling hash, in envelope order.
    pub sibling_hashes: Vec<TxHash>,
}

impl From<&CandidateGroup> for CandidateSubmissionResponse {
    fn from(group: &CandidateGroup) -> Self {
        Self {
            group_hash: group.group_hash(),
            representative_hash: group.representative().hash(),
            sibling_hashes: group.envelope().sibling_hashes.clone(),
        }
    }
}

/// RPC API for submitting a candidate group (design §2).
#[rpc(server, namespace = "base")]
pub trait SendRawTransactionCandidatesApi {
    /// Submits sibling transactions from which the builder selects at most one to include.
    #[method(name = "sendRawTransactionCandidates")]
    async fn send_raw_transaction_candidates(
        &self,
        siblings: CandidateSiblings,
        options: SendRawTransactionCandidatesOptions,
    ) -> RpcResult<CandidateSubmissionResponse>;
}

/// Per-process runtime toggle for candidate selection (design §8.4).
///
/// A single-process call is not a cluster kill switch.
#[rpc(server, namespace = "admin")]
pub trait AdminCandidateSelectionApi {
    /// Enables or disables candidate admission and selection. Returns the previous state.
    #[method(name = "setCandidateSelectionEnabled")]
    async fn set_candidate_selection_enabled(&self, enabled: bool) -> RpcResult<bool>;
}

/// Local ingress for candidate groups.
#[derive(Debug, Clone)]
pub struct SendRawTransactionCandidatesApiImpl<Provider, Pool> {
    provider: Provider,
    pool: Pool,
    runtime: Arc<CandidateRuntime>,
    chain_id: u64,
    max_expiry_secs: u64,
    sequencer_client: Option<SequencerClient>,
}

impl<Provider, Pool> SendRawTransactionCandidatesApiImpl<Provider, Pool> {
    /// Creates local candidate ingress over the shared candidate runtime.
    ///
    /// `max_expiry_secs` bounds how far past the next block `expiryBlock` may be, converted to
    /// blocks at the active full-block cadence.
    pub const fn new(
        provider: Provider,
        pool: Pool,
        runtime: Arc<CandidateRuntime>,
        chain_id: u64,
        max_expiry_secs: u64,
    ) -> Self {
        Self { provider, pool, runtime, chain_id, max_expiry_secs, sequencer_client: None }
    }

    /// Proxies submissions unchanged to the sequencer instead of the local pool.
    pub fn with_sequencer_client(mut self, client: SequencerClient) -> Self {
        self.sequencer_client = Some(client);
        self
    }

    fn reject(error: &CandidateError) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(
            ErrorCode::InvalidParams.code(),
            error.to_string(),
            Some(error.as_label()),
        )
    }

    fn internal(message: String) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(ErrorCode::InternalError.code(), message, None::<()>)
    }
}

impl<Provider, Pool> SendRawTransactionCandidatesApiImpl<Provider, Pool>
where
    Provider: BlockReaderIdExt + ChainSpecProvider<ChainSpec: Upgrades> + StateProviderFactory,
{
    /// Returns the inclusive window `expiryBlock` must fall in, or `None` before genesis.
    fn expiry_window(&self) -> RpcResult<Option<ExpiryWindow>> {
        let header = self
            .provider
            .latest_header()
            .map_err(|error| Self::internal(format!("failed to resolve latest header: {error}")))?;
        Ok(header.map(|header| {
            let interval = BlockCadence::full_block_interval_millis(
                &*self.provider.chain_spec(),
                header.timestamp(),
            );
            ExpiryWindow {
                next_block: header.number().saturating_add(1),
                max_blocks: self.max_expiry_secs.saturating_mul(1_000) / interval.max(1),
            }
        }))
    }

    /// Rejects EIP-7702-delegated senders (design §4.2).
    fn ensure_sender_not_delegated(&self, group: &CandidateGroup) -> RpcResult<()> {
        let state = self
            .provider
            .latest()
            .map_err(|error| Self::internal(format!("failed to open latest state: {error}")))?;
        let code = state
            .account_code(&group.sender())
            .map_err(|error| Self::internal(format!("failed to read sender code: {error}")))?;
        if code.is_some_and(|code| code.is_eip7702()) {
            return Err(Self::reject(&CandidateError::SenderDelegated));
        }
        Ok(())
    }
}

#[async_trait]
impl<Provider, Pool> SendRawTransactionCandidatesApiServer
    for SendRawTransactionCandidatesApiImpl<Provider, Pool>
where
    Provider: BlockReaderIdExt
        + ChainSpecProvider<ChainSpec: Upgrades>
        + StateProviderFactory
        + Clone
        + 'static,
    Pool: TransactionPool<Transaction = BasePooledTransaction> + Clone + 'static,
{
    async fn send_raw_transaction_candidates(
        &self,
        siblings: CandidateSiblings,
        options: SendRawTransactionCandidatesOptions,
    ) -> RpcResult<CandidateSubmissionResponse> {
        // Step 1: a runtime-disabled service admits no new group, whatever its fallback.
        if !self.runtime.switch.is_enabled() {
            return Err(Self::reject(&CandidateError::Disabled));
        }
        // Step 2: proxies forward unchanged and keep no copy.
        if let Some(client) = &self.sequencer_client {
            return client
                .request("base_sendRawTransactionCandidates", (siblings, options))
                .await
                .map_err(ErrorObjectOwned::from);
        }

        // Steps 3-5: bounds, decode, uniformity, envelope, expiry and signature.
        let window = self.expiry_window()?;
        let validator = CandidateValidator { limits: self.runtime.limits, chain_id: self.chain_id };
        let group = validator
            .validate(&siblings.0, options.envelope, options.envelope_signature, window)
            .map_err(|error| {
                debug!(error = %error, label = error.as_label(), "rejected candidate group");
                Self::reject(&error)
            })?;
        self.ensure_sender_not_delegated(&group)?;
        let group = Arc::new(group);
        let response = CandidateSubmissionResponse::from(group.as_ref());

        // Step 6: an exact duplicate returns the original response before any other check.
        if let Some(entry) = self.runtime.ledger.lookup(&response.group_hash) {
            if entry.state.is_terminal() {
                return Err(Self::reject(&CandidateError::GroupTerminal));
            }
            return Ok(response);
        }

        // Step 7: access policy.
        let policy = &self.runtime.policy;
        let principal = policy
            .principal(group.sender())
            .ok_or_else(|| Self::reject(&CandidateError::PrincipalNotAllowed))?;
        let envelope = group.envelope();
        let summary = GroupSummary {
            group_hash: response.group_hash,
            sender: group.sender(),
            max_work_gas: envelope.max_work_gas,
            max_evaluations: envelope.max_evaluations,
            max_access_charge: envelope.max_access_charge,
        };
        policy
            .admit(principal, &summary)
            .map_err(|denied| Self::reject(&CandidateError::from(denied)))?;

        // Step 8: ledger entry, then pool insertion; confirm or abort atomically per group.
        let admission = self
            .runtime
            .ledger
            .create(Arc::clone(&group), principal, policy.policy_version(), |hash| {
                self.pool.contains(hash)
            })
            .map_err(|error| Self::reject(&error))?;
        if admission == LedgerAdmission::Duplicate {
            return Ok(response);
        }
        let representative = group.representative();
        let transaction = BasePooledTransaction::new(
            representative.transaction.clone(),
            representative.encoded.len(),
        )
        .with_candidate_group(Arc::clone(&group));
        if let Err(error) = self.pool.add_transaction(TransactionOrigin::Private, transaction).await
        {
            self.runtime.ledger.abort(response.group_hash);
            return Err(ErrorObjectOwned::from(EthApiError::from(error)));
        }
        self.runtime.ledger.confirm_pooled(response.group_hash);
        info!(
            group_hash = %response.group_hash,
            representative = %response.representative_hash,
            siblings = response.sibling_hashes.len(),
            principal = principal.0,
            "accepted candidate group"
        );
        Ok(response)
    }
}

/// Admin toggle over the shared candidate runtime.
#[derive(Debug, Clone)]
pub struct AdminCandidateSelectionApiImpl {
    runtime: Arc<CandidateRuntime>,
}

impl AdminCandidateSelectionApiImpl {
    /// Creates the admin toggle.
    pub const fn new(runtime: Arc<CandidateRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl AdminCandidateSelectionApiServer for AdminCandidateSelectionApiImpl {
    async fn set_candidate_selection_enabled(&self, enabled: bool) -> RpcResult<bool> {
        let previous = self.runtime.switch.set(enabled);
        info!(enabled = enabled, previous = previous, "candidate selection toggled");
        Ok(previous)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use base_common_chains::ChainConfig;
    use base_common_consensus::BasePrimitives;
    use base_execution_candidates::{
        CandidateLimits, CandidateSwitch, EvictionReason, GroupLedger, GroupState, PrincipalEntry,
        PrincipalId, PrincipalsFile, StaticAccessPolicy, test_utils::CandidateFixture,
    };
    use base_execution_chainspec::BaseChainSpec;
    use base_execution_txpool::BasePooledTx;
    use base_test_utils::build_test_genesis_everest;
    use httpmock::{Method::POST, MockServer};
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_transaction_pool::{
        CoinbaseTipOrdering, Pool, PoolConfig, blobstore::InMemoryBlobStore,
        noop::NoopTransactionPool, test_utils::OkValidator,
    };

    use super::*;

    type TestProvider = MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>;
    type TestPool = Pool<
        OkValidator<BasePooledTransaction>,
        CoinbaseTipOrdering<BasePooledTransaction>,
        InMemoryBlobStore,
    >;

    const PRINCIPAL: PrincipalId = PrincipalId(7);
    const MAX_EXPIRY_SECS: u64 = 3_600;

    fn chain_id() -> u64 {
        ChainConfig::mainnet().chain_id
    }

    fn fixture() -> CandidateFixture {
        CandidateFixture::new(chain_id())
    }

    fn provider() -> TestProvider {
        let mut genesis = build_test_genesis_everest();
        genesis.config.chain_id = chain_id();
        MockEthProvider::<BasePrimitives>::new()
            .with_chain_spec(Arc::new(BaseChainSpec::from_genesis(genesis)))
            .with_genesis_block()
    }

    fn pool() -> TestPool {
        Pool::new(
            OkValidator::default(),
            CoinbaseTipOrdering::default(),
            InMemoryBlobStore::default(),
            PoolConfig::default(),
        )
    }

    fn runtime(allowed: &[Address]) -> Arc<CandidateRuntime> {
        let policy = StaticAccessPolicy::new(PrincipalsFile {
            policy_version: 1,
            work_gas_quota: None,
            principals: vec![PrincipalEntry { id: PRINCIPAL, senders: allowed.to_vec() }],
        });
        Arc::new(CandidateRuntime {
            ledger: GroupLedger::default(),
            policy: Arc::new(policy),
            switch: CandidateSwitch::default(),
            limits: CandidateLimits::default(),
        })
    }

    fn rpc<P>(
        provider: TestProvider,
        pool: P,
        runtime: &Arc<CandidateRuntime>,
        max_expiry_secs: u64,
    ) -> SendRawTransactionCandidatesApiImpl<TestProvider, P> {
        SendRawTransactionCandidatesApiImpl::new(
            provider,
            pool,
            Arc::clone(runtime),
            chain_id(),
            max_expiry_secs,
        )
    }

    fn request(
        fixture: &CandidateFixture,
    ) -> (CandidateSiblings, SendRawTransactionCandidatesOptions) {
        let (raw, envelope, envelope_signature) = fixture.submission(3, 1);
        (
            CandidateSiblings(raw),
            SendRawTransactionCandidatesOptions { envelope, envelope_signature },
        )
    }

    fn label(error: &ErrorObjectOwned) -> String {
        assert_eq!(error.code(), ErrorCode::InvalidParams.code(), "{error:?}");
        serde_json::from_str(error.data().expect("label").get()).expect("string label")
    }

    #[tokio::test]
    async fn accepted_group_is_pooled_under_its_representative() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let pool = pool();
        let api = rpc(provider(), pool.clone(), &runtime, MAX_EXPIRY_SECS);
        let (siblings, options) = request(&fixture);
        let expected_group = options.envelope.signing_hash(chain_id());
        let expected_siblings = options.envelope.sibling_hashes.clone();

        let response = api.send_raw_transaction_candidates(siblings, options).await.unwrap();

        assert_eq!(response.group_hash, expected_group);
        assert_eq!(response.sibling_hashes, expected_siblings);
        // DEFAULT_SIBLING with defaultIndex 1: the representative is sibling 1.
        assert_eq!(response.representative_hash, expected_siblings[1]);
        let pooled = pool.get(&response.representative_hash).expect("representative pooled");
        assert_eq!(
            BasePooledTx::candidate_group(&pooled.transaction).map(|group| group.group_hash()),
            Some(expected_group)
        );
        let entry = runtime.ledger.lookup(&expected_group).expect("ledger entry");
        assert_eq!(entry.state, GroupState::Pooled);
        assert_eq!(entry.principal, PRINCIPAL);
    }

    #[tokio::test]
    async fn exact_duplicate_returns_the_original_response_without_a_new_slot() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let pool = pool();
        let api = rpc(provider(), pool.clone(), &runtime, MAX_EXPIRY_SECS);
        let (siblings, options) = request(&fixture);

        let first =
            api.send_raw_transaction_candidates(siblings.clone(), options.clone()).await.unwrap();
        let second = api.send_raw_transaction_candidates(siblings, options).await.unwrap();

        assert_eq!(first, second);
        assert_eq!(pool.pool_size().total, 1);
        assert_eq!(runtime.ledger.counts(), (1, 1));
    }

    #[tokio::test]
    async fn terminal_group_is_rejected() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let api = rpc(provider(), pool(), &runtime, MAX_EXPIRY_SECS);
        let (siblings, options) = request(&fixture);
        let response =
            api.send_raw_transaction_candidates(siblings.clone(), options.clone()).await.unwrap();
        runtime.ledger.evict(response.group_hash, EvictionReason::BudgetExhausted);

        let error = api.send_raw_transaction_candidates(siblings, options).await.unwrap_err();
        assert_eq!(label(&error), "group_terminal");
    }

    #[tokio::test]
    async fn runtime_disabled_ingress_rejects_every_new_group() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let api = rpc(provider(), pool(), &runtime, MAX_EXPIRY_SECS);
        let admin = AdminCandidateSelectionApiImpl::new(Arc::clone(&runtime));

        assert!(admin.set_candidate_selection_enabled(false).await.unwrap());
        let (siblings, options) = request(&fixture);
        let error = api
            .send_raw_transaction_candidates(siblings.clone(), options.clone())
            .await
            .unwrap_err();
        assert_eq!(label(&error), "candidates_disabled");
        assert_eq!(runtime.ledger.counts(), (0, 0));

        assert!(!admin.set_candidate_selection_enabled(true).await.unwrap());
        api.send_raw_transaction_candidates(siblings, options).await.unwrap();
    }

    #[tokio::test]
    async fn unregistered_principal_is_rejected() {
        let fixture = fixture();
        let runtime = runtime(&[Address::repeat_byte(0x99)]);
        let api = rpc(provider(), pool(), &runtime, MAX_EXPIRY_SECS);
        let (siblings, options) = request(&fixture);

        let error = api.send_raw_transaction_candidates(siblings, options).await.unwrap_err();
        assert_eq!(label(&error), "principal_not_allowed");
        assert_eq!(runtime.ledger.counts(), (0, 0));
    }

    #[tokio::test]
    async fn validation_failures_carry_stable_labels() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let api = rpc(provider(), pool(), &runtime, MAX_EXPIRY_SECS);

        let (siblings, options) = request(&fixture);
        let single = CandidateSiblings(siblings.0[..1].to_vec());
        let error = api.send_raw_transaction_candidates(single, options).await.unwrap_err();
        assert_eq!(label(&error), "too_few_siblings");

        let (siblings, mut options) = request(&fixture);
        options.envelope.rule.min_delta = U256::from(1);
        let error = api.send_raw_transaction_candidates(siblings, options).await.unwrap_err();
        assert_eq!(label(&error), "envelope_signer_not_sender");
    }

    #[tokio::test]
    async fn expiry_past_the_configured_window_is_rejected() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        // One second is at most ten native blocks; the fixture expires at block 100.
        let api = rpc(provider(), pool(), &runtime, 1);
        let (siblings, options) = request(&fixture);

        let error = api.send_raw_transaction_candidates(siblings, options).await.unwrap_err();
        assert_eq!(label(&error), "expiry_out_of_window");
    }

    #[tokio::test]
    async fn delegated_sender_is_rejected() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let provider = provider();
        let mut delegation = vec![0xef, 0x01, 0x00];
        delegation.extend_from_slice(Address::repeat_byte(0xde).as_slice());
        provider.add_account(
            fixture.address(),
            ExtendedAccount::new(0, U256::from(1_000_000_000u64))
                .with_bytecode(Bytes::from(delegation)),
        );
        let api = rpc(provider, pool(), &runtime, MAX_EXPIRY_SECS);
        let (siblings, options) = request(&fixture);

        let error = api.send_raw_transaction_candidates(siblings, options).await.unwrap_err();
        assert_eq!(label(&error), "sender_delegated");
    }

    #[tokio::test]
    async fn failed_pool_insertion_releases_the_ledger_entry() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let api = rpc(
            provider(),
            NoopTransactionPool::<BasePooledTransaction>::new(),
            &runtime,
            MAX_EXPIRY_SECS,
        );
        let (siblings, options) = request(&fixture);
        let sibling_hash = options.envelope.sibling_hashes[0];

        api.send_raw_transaction_candidates(siblings, options).await.unwrap_err();

        assert_eq!(runtime.ledger.counts(), (0, 0));
        assert!(!runtime.ledger.suppresses_plain_reinsertion(&sibling_hash));
    }

    #[tokio::test]
    async fn proxy_forwards_unchanged_without_local_admission() {
        let fixture = fixture();
        // No principal and an empty pool: the proxy must not evaluate or insert anything.
        let runtime = runtime(&[]);
        let (siblings, options) = request(&fixture);
        let expected = CandidateSubmissionResponse {
            group_hash: B256::repeat_byte(0x11),
            representative_hash: TxHash::repeat_byte(0x22),
            sibling_hashes: options.envelope.sibling_hashes.clone(),
        };
        let sequencer = MockServer::start();
        let mock = sequencer.mock(|when, then| {
            when.method(POST).path("/").json_body(serde_json::json!({
                "jsonrpc": "2.0", "id": 0, "method": "base_sendRawTransactionCandidates",
                "params": [&siblings, &options],
            }));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({"jsonrpc": "2.0", "id": 0, "result": &expected}));
        });
        let client = SequencerClient::new_http_with_headers(sequencer.base_url(), vec![]).unwrap();
        let api = rpc(provider(), pool(), &runtime, MAX_EXPIRY_SECS).with_sequencer_client(client);

        let response = api.send_raw_transaction_candidates(siblings, options).await.unwrap();

        assert_eq!(response, expected);
        assert_eq!(runtime.ledger.counts(), (0, 0));
        mock.assert();
    }

    #[test]
    fn request_uses_positional_params_and_rejects_unknown_option_fields() {
        let fixture = fixture();
        let (siblings, options) = request(&fixture);
        let params = serde_json::to_value((&siblings, &options)).unwrap();
        let (decoded_siblings, decoded_options): (
            CandidateSiblings,
            SendRawTransactionCandidatesOptions,
        ) = serde_json::from_value(params.clone()).unwrap();
        assert_eq!(decoded_siblings, siblings);
        assert_eq!(decoded_options, options);
        assert!(params[1].get("envelopeSignature").is_some());

        let mut padded = params[1].clone();
        padded["extra"] = serde_json::json!(true);
        assert!(serde_json::from_value::<SendRawTransactionCandidatesOptions>(padded).is_err());
    }

    #[test]
    fn sibling_list_deserialization_is_bounded() {
        let at_ceiling = vec!["0x00"; base_execution_candidates::MAX_SIBLINGS_CEILING];
        assert!(serde_json::from_value::<CandidateSiblings>(serde_json::json!(at_ceiling)).is_ok());
        let past_ceiling = vec!["0x00"; base_execution_candidates::MAX_SIBLINGS_CEILING + 1];
        assert!(
            serde_json::from_value::<CandidateSiblings>(serde_json::json!(past_ceiling)).is_err()
        );
    }

    #[test]
    fn methods_register_under_their_namespaces() {
        let runtime = runtime(&[]);
        let api = rpc(provider(), pool(), &runtime, MAX_EXPIRY_SECS).into_rpc();
        assert!(api.method_names().any(|name| name == "base_sendRawTransactionCandidates"));
        let admin = AdminCandidateSelectionApiImpl::new(runtime).into_rpc();
        assert!(admin.method_names().any(|name| name == "admin_setCandidateSelectionEnabled"));
    }
}
