//! `base_sendRawTransactionCandidates` ingress and its runtime kill switch.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use alloy_consensus::BlockHeader;
use alloy_primitives::{B256, Bytes, Signature, TxHash};
use base_common_chains::Upgrades;
use base_execution_candidates::{
    CandidateEnvelope, CandidateError, CandidateGroup, CandidateRuntime, CandidateValidator,
    ExpiryWindow, GroupLedger, GroupState, GroupSummary, LedgerAdmission,
};
use base_execution_rpc::SequencerClient;
use base_execution_txpool::BasePooledTransaction;
use base_observability_events::{
    TransactionEventProducer, TransactionEventType, transaction_event,
};
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
use tokio::sync::watch;
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

/// How long a group's first admission may spend in pool insertion, and how long an exact
/// duplicate waits for it. Pool validation normally takes milliseconds.
pub const DEFAULT_CANDIDATE_ADMISSION_TIMEOUT: Duration = Duration::from_secs(2);

/// Pool insertions in flight, by `groupHash`. The value becomes `Some(inserted)` when the
/// first request finishes; exact duplicates wait on it instead of reporting acceptance early.
pub type InflightAdmissions = Arc<Mutex<HashMap<B256, watch::Receiver<Option<bool>>>>>;

/// Local ingress for candidate groups.
#[derive(Debug, Clone)]
pub struct SendRawTransactionCandidatesApiImpl<Provider, Pool> {
    provider: Provider,
    pool: Pool,
    runtime: Arc<CandidateRuntime>,
    chain_id: u64,
    max_expiry_secs: u64,
    sequencer_client: Option<SequencerClient>,
    inflight: InflightAdmissions,
    admission_timeout: Duration,
}

/// Owns a first admission's `Pending` ledger entry and in-flight receiver until it settles.
///
/// Dropping it unsettled, because the request was cancelled, settles the admission from pool
/// state: confirmed if the representative is pooled, aborted otherwise. Either way the
/// in-flight entry is removed and waiting duplicates learn the outcome, so no `Pending` entry
/// outlives its request.
#[derive(Debug)]
pub struct PendingAdmission<Pool: TransactionPool> {
    /// The ingress in-flight map holding this admission's receiver.
    pub inflight: InflightAdmissions,
    /// The shared candidate runtime holding the `Pending` ledger entry.
    pub runtime: Arc<CandidateRuntime>,
    /// The pool the representative is inserted into.
    pub pool: Pool,
    /// The admitted group.
    pub group_hash: B256,
    /// The representative being inserted.
    pub representative: TxHash,
    /// Publishes the outcome to waiting duplicates; `None` once settled.
    pub outcome: Option<watch::Sender<Option<bool>>>,
}

impl<Pool: TransactionPool> PendingAdmission<Pool> {
    /// Confirms or aborts the ledger entry, removes the in-flight receiver and wakes waiting
    /// duplicates. Later calls do nothing and return `false`.
    ///
    /// Returns whether the group was accepted. An inserted representative is not accepted when
    /// the ledger ended the group at settlement because a canonical block consumed its nonce
    /// after validation; that stale representative is removed from the pool.
    pub fn settle(&mut self, inserted: bool) -> bool {
        let Some(outcome) = self.outcome.take() else { return false };
        let stale = {
            let mut inflight = self.inflight.lock().unwrap_or_else(PoisonError::into_inner);
            inflight.remove(&self.group_hash);
            if inserted {
                self.runtime.ledger.confirm_pooled(self.group_hash, |hash| self.pool.contains(hash))
            } else {
                self.runtime.ledger.abort(self.group_hash);
                None
            }
        };
        if let Some(stale) = stale {
            debug!(group_hash = %self.group_hash, representative = %stale, "removing stale candidate representative");
            self.pool.remove_transactions(vec![stale]);
        }
        let accepted = inserted && stale.is_none();
        outcome.send_replace(Some(accepted));
        accepted
    }
}

impl<Pool: TransactionPool> Drop for PendingAdmission<Pool> {
    fn drop(&mut self) {
        if self.outcome.is_some() {
            debug!(group_hash = %self.group_hash, "settling abandoned candidate admission");
            let inserted = self.pool.contains(&self.representative);
            self.settle(inserted);
        }
    }
}

impl<Provider, Pool> SendRawTransactionCandidatesApiImpl<Provider, Pool> {
    /// Creates local candidate ingress over the shared candidate runtime.
    ///
    /// `max_expiry_secs` bounds how far past the next block `expiryBlock` may be, converted to
    /// blocks at the active full-block cadence.
    pub fn new(
        provider: Provider,
        pool: Pool,
        runtime: Arc<CandidateRuntime>,
        chain_id: u64,
        max_expiry_secs: u64,
    ) -> Self {
        Self {
            provider,
            pool,
            runtime,
            chain_id,
            max_expiry_secs,
            sequencer_client: None,
            inflight: InflightAdmissions::default(),
            admission_timeout: DEFAULT_CANDIDATE_ADMISSION_TIMEOUT,
        }
    }

    /// Overrides [`DEFAULT_CANDIDATE_ADMISSION_TIMEOUT`].
    pub const fn with_admission_timeout(mut self, timeout: Duration) -> Self {
        self.admission_timeout = timeout;
        self
    }

    /// Proxies submissions unchanged to the sequencer instead of the local pool.
    pub fn with_sequencer_client(mut self, client: SequencerClient) -> Self {
        self.sequencer_client = Some(client);
        self
    }

    /// Maps a candidate rejection to its `-32602` error with the stable reason label.
    pub fn reject(error: &CandidateError) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(
            ErrorCode::InvalidParams.code(),
            error.to_string(),
            Some(error.as_label()),
        )
    }

    /// Returns a `-32603` internal error.
    pub fn internal(message: String) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(ErrorCode::InternalError.code(), message, None::<()>)
    }

    /// Locks the in-flight map. Ledger transitions between `Pending` and a settled state happen
    /// under this lock, so a `Pending` entry always has a registered receiver.
    pub fn lock_inflight(&self) -> MutexGuard<'_, HashMap<B256, watch::Receiver<Option<bool>>>> {
        self.inflight.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits, at most the admission timeout, for another request's insertion of the same group
    /// and mirrors its outcome.
    pub async fn await_inflight(
        &self,
        receiver: Option<watch::Receiver<Option<bool>>>,
        response: CandidateSubmissionResponse,
    ) -> RpcResult<CandidateSubmissionResponse> {
        let Some(mut receiver) = receiver else {
            return Err(Self::reject(&CandidateError::ConcurrentSubmissionFailed));
        };
        let waited =
            tokio::time::timeout(self.admission_timeout, receiver.wait_for(Option::is_some)).await;
        match waited {
            Err(_) => Err(Self::reject(&CandidateError::AdmissionTimedOut)),
            Ok(Ok(outcome)) if *outcome == Some(true) => Ok(response),
            Ok(_) => Err(Self::reject(&CandidateError::ConcurrentSubmissionFailed)),
        }
    }
}

impl<Provider, Pool> SendRawTransactionCandidatesApiImpl<Provider, Pool>
where
    Provider: BlockReaderIdExt + ChainSpecProvider<ChainSpec: Upgrades> + StateProviderFactory,
{
    /// Returns the inclusive window `expiryBlock` must fall in, or `None` before genesis.
    pub fn expiry_window(&self) -> RpcResult<Option<ExpiryWindow>> {
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
    pub fn ensure_sender_not_delegated(&self, group: &CandidateGroup) -> RpcResult<()> {
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
        let mut trace = CandidateAdmissionTrace::default();
        let result = self.submit(siblings, options, &mut trace).await;
        trace.journal(&self.runtime.ledger, &result);
        result
    }
}

impl<Provider, Pool> SendRawTransactionCandidatesApiImpl<Provider, Pool>
where
    Provider: BlockReaderIdExt
        + ChainSpecProvider<ChainSpec: Upgrades>
        + StateProviderFactory
        + Clone
        + 'static,
    Pool: TransactionPool<Transaction = BasePooledTransaction> + Clone + 'static,
{
    /// Runs admission (design §5.1), filling `trace` with the join keys as they become known.
    pub async fn submit(
        &self,
        siblings: CandidateSiblings,
        options: SendRawTransactionCandidatesOptions,
        trace: &mut CandidateAdmissionTrace,
    ) -> RpcResult<CandidateSubmissionResponse> {
        // Step 1: a runtime-disabled service admits no new group, whatever its fallback.
        if !self.runtime.switch.is_enabled() {
            trace.disabled = true;
            return Err(Self::reject(&CandidateError::Disabled));
        }
        // Step 2: proxies forward unchanged and keep no copy.
        if let Some(client) = &self.sequencer_client {
            trace.forwarded = true;
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
        trace.group_hash = Some(group.group_hash());
        trace.representative = Some(group.representative().hash());
        trace.siblings = group.siblings().len();
        self.ensure_sender_not_delegated(&group)?;
        let group = Arc::new(group);
        let response = CandidateSubmissionResponse::from(group.as_ref());

        // Step 6: an exact duplicate returns the original response before any other check,
        // but only once the group is actually pooled.
        let in_flight = {
            let inflight = self.lock_inflight();
            match self
                .runtime
                .ledger
                .classify(&response.group_hash, |hash| self.pool.contains(hash))
            {
                None => None,
                Some(state) if state.is_terminal() => {
                    return Err(Self::reject(&CandidateError::GroupTerminal));
                }
                Some(GroupState::Pending) => Some(inflight.get(&response.group_hash).cloned()),
                Some(_) => {
                    trace.duplicate = true;
                    return Ok(response);
                }
            }
        };
        if let Some(receiver) = in_flight {
            trace.duplicate = true;
            return self.await_inflight(receiver, response).await;
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
        let created = {
            let mut inflight = self.lock_inflight();
            let admission = self
                .runtime
                .ledger
                .create(Arc::clone(&group), principal, policy.policy_version(), |hash| {
                    self.pool.contains(hash)
                })
                .map_err(|error| Self::reject(&error))?;
            match admission {
                LedgerAdmission::Duplicate => {
                    trace.duplicate = true;
                    return Ok(response);
                }
                LedgerAdmission::Pending => Err(inflight.get(&response.group_hash).cloned()),
                LedgerAdmission::Created => {
                    let (sender, receiver) = watch::channel(None);
                    inflight.insert(response.group_hash, receiver);
                    Ok(sender)
                }
            }
        };
        let outcome = match created {
            Ok(sender) => sender,
            Err(receiver) => {
                trace.duplicate = true;
                return self.await_inflight(receiver, response).await;
            }
        };
        // From here the guard owns the Pending entry: cancelling this request settles it.
        let mut admission = PendingAdmission {
            inflight: Arc::clone(&self.inflight),
            runtime: Arc::clone(&self.runtime),
            pool: self.pool.clone(),
            group_hash: response.group_hash,
            representative: response.representative_hash,
            outcome: Some(outcome),
        };
        let representative = group.representative();
        let transaction = BasePooledTransaction::new(
            representative.transaction.clone(),
            representative.encoded.len(),
        )
        .with_candidate_group(Arc::clone(&group));
        let inserted = tokio::time::timeout(
            self.admission_timeout,
            self.pool.add_transaction(TransactionOrigin::Private, transaction),
        )
        .await;
        match inserted {
            Ok(Ok(_)) => {
                if !admission.settle(true) {
                    return Err(Self::reject(&CandidateError::GroupTerminal));
                }
            }
            Ok(Err(error)) => {
                admission.settle(false);
                trace.pool_rejected = true;
                return Err(ErrorObjectOwned::from(EthApiError::from(error)));
            }
            Err(_) => {
                let pooled = self.pool.contains(&response.representative_hash);
                let accepted = admission.settle(pooled);
                if !pooled {
                    debug!(group_hash = %response.group_hash, "candidate admission timed out");
                    return Err(Self::reject(&CandidateError::AdmissionTimedOut));
                }
                if !accepted {
                    return Err(Self::reject(&CandidateError::GroupTerminal));
                }
            }
        }
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

/// Join keys and outcome of one `base_sendRawTransactionCandidates` request, journaled when it
/// returns. A request whose future is dropped journals nothing; [`PendingAdmission`] still settles
/// its ledger entry.
#[derive(Debug, Default, Clone, Copy)]
pub struct CandidateAdmissionTrace {
    /// Group identity, once the group validated.
    pub group_hash: Option<B256>,
    /// Representative hash, once the group validated.
    pub representative: Option<TxHash>,
    /// Number of siblings, once the group validated.
    pub siblings: usize,
    /// The request returned, or waited on, an existing admission of the same group.
    pub duplicate: bool,
    /// A proxy forwarded the request unchanged; the sequencer journals it.
    pub forwarded: bool,
    /// Candidate selection was runtime-disabled; like a node started without candidates, the
    /// request journals nothing.
    pub disabled: bool,
    /// The pool refused the representative, so an unlabeled error is `pool_rejected`.
    pub pool_rejected: bool,
}

impl CandidateAdmissionTrace {
    /// Journals `TXPOOL_SEND_RAW_TRANSACTION_CANDIDATES` once per admitted group, or
    /// `TXPOOL_SEND_RAW_TRANSACTION_CANDIDATES_REJECTED` with the error's stable label.
    ///
    /// Admission is keyed by the group hash, so a replayed event deduplicates, and duplicate
    /// submissions journal nothing. Each rejection gets a never-reused identity from `ledger`:
    /// a group may be rejected repeatedly, and a malformed request has no hash at all.
    pub fn journal(&self, ledger: &GroupLedger, result: &RpcResult<CandidateSubmissionResponse>) {
        if self.forwarded || self.disabled {
            return;
        }
        let instance = format!("{:016x}", ledger.instance());
        let _ = match result {
            Ok(_) if self.duplicate => return,
            Ok(response) => transaction_event!(
                producer: TransactionEventProducer::BaseRethNode,
                event_type: TransactionEventType::TxpoolSendRawTransactionCandidates,
                tx_hash: response.representative_hash,
                id: { "group_hash" => format!("{:#x}", response.group_hash) },
                data: {
                    "rpc_method" => "base_sendRawTransactionCandidates",
                    "group_hash" => format!("{:#x}", response.group_hash),
                    "sibling_hashes" => &response.sibling_hashes,
                    "candidate_instance" => instance,
                },
            ),
            Err(error) => transaction_event!(
                producer: TransactionEventProducer::BaseRethNode,
                event_type: TransactionEventType::TxpoolSendRawTransactionCandidatesRejected,
                maybe_tx_hash: self.representative,
                id: {
                    "candidate_instance" => &instance,
                    "rejection_sequence" => ledger.next_sequence(),
                },
                data: {
                    "rpc_method" => "base_sendRawTransactionCandidates",
                    "group_hash" => self.group_hash.map(|hash| format!("{hash:#x}")),
                    "siblings" => self.siblings,
                    "rejection_reason" => self.rejection_label(error),
                    "error_code" => error.code(),
                    "candidate_instance" => instance,
                },
            ),
        };
    }

    /// The stable reason label of a candidate rejection. An error without one is
    /// `pool_rejected` when the pool refused the representative and `internal_error`
    /// otherwise, such as a failed state read.
    pub fn rejection_label(&self, error: &ErrorObjectOwned) -> String {
        error
            .data()
            .and_then(|data| serde_json::from_str::<String>(data.get()).ok())
            .unwrap_or_else(|| {
                if self.pool_rejected { "pool_rejected" } else { "internal_error" }.to_string()
            })
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
    use base_observability_events::TransactionEventCapture;
    use base_test_utils::build_test_genesis_everest;
    use httpmock::{Method::POST, MockServer};
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_transaction_pool::{
        CoinbaseTipOrdering, Pool, PoolConfig, PoolTransaction, TransactionValidationOutcome,
        blobstore::InMemoryBlobStore, noop::NoopTransactionPool, test_utils::OkValidator,
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

    /// Holds every validation until `gate` opens, then accepts or rejects it.
    #[derive(Debug, Clone)]
    struct GatedValidator {
        gate: Arc<tokio::sync::Semaphore>,
        accept: bool,
    }

    impl GatedValidator {
        /// A validator that never finishes.
        fn stalled() -> Self {
            Self { gate: Arc::new(tokio::sync::Semaphore::new(0)), accept: true }
        }
    }

    impl reth_transaction_pool::TransactionValidator for GatedValidator {
        type Transaction = BasePooledTransaction;
        type Block = <OkValidator<BasePooledTransaction> as reth_transaction_pool::TransactionValidator>::Block;

        async fn validate_transaction(
            &self,
            origin: TransactionOrigin,
            transaction: Self::Transaction,
        ) -> TransactionValidationOutcome<Self::Transaction> {
            let _permit = self.gate.acquire().await.expect("gate stays open");
            if self.accept {
                OkValidator::default().validate_transaction(origin, transaction).await
            } else {
                TransactionValidationOutcome::Error(
                    *transaction.hash(),
                    Box::new(std::io::Error::other("rejected by the test validator")),
                )
            }
        }
    }

    fn gated_rpc(
        validator: GatedValidator,
        runtime: &Arc<CandidateRuntime>,
    ) -> SendRawTransactionCandidatesApiImpl<
        TestProvider,
        Pool<GatedValidator, CoinbaseTipOrdering<BasePooledTransaction>, InMemoryBlobStore>,
    > {
        let pool = Pool::new(
            validator,
            CoinbaseTipOrdering::default(),
            InMemoryBlobStore::default(),
            PoolConfig::default(),
        );
        rpc(provider(), pool, runtime, MAX_EXPIRY_SECS)
    }

    /// Yields until the first admission has registered its `Pending` entry.
    async fn until_pending(runtime: &CandidateRuntime) {
        for _ in 0..100 {
            if runtime.ledger.counts() == (1, 1) {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("the first admission never reached pool insertion");
    }

    #[tokio::test]
    async fn cancelled_first_admission_releases_inflight_and_pending() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let api = gated_rpc(GatedValidator::stalled(), &runtime);
        let (siblings, options) = request(&fixture);

        let first = tokio::spawn({
            let api = api.clone();
            async move { api.send_raw_transaction_candidates(siblings, options).await }
        });
        until_pending(&runtime).await;
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());

        assert!(api.lock_inflight().is_empty(), "no orphaned in-flight receiver");
        assert_eq!(runtime.ledger.counts(), (0, 0), "no orphaned Pending entry");
    }

    #[tokio::test]
    async fn stalled_admission_and_its_duplicates_time_out() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let api = gated_rpc(GatedValidator::stalled(), &runtime)
            .with_admission_timeout(Duration::from_millis(50));
        let (siblings, options) = request(&fixture);

        let first = tokio::spawn({
            let api = api.clone();
            let (siblings, options) = (siblings.clone(), options.clone());
            async move { api.send_raw_transaction_candidates(siblings, options).await }
        });
        until_pending(&runtime).await;
        let duplicate = tokio::time::timeout(
            Duration::from_secs(5),
            api.send_raw_transaction_candidates(siblings, options),
        )
        .await
        .expect("the duplicate wait is bounded");

        let first = first.await.unwrap().expect_err("a stalled insertion times out");
        assert_eq!(first.data().map(|data| data.get()), Some("\"admission_timeout\""));
        assert!(duplicate.is_err(), "a duplicate never reports acceptance of a stalled group");
        assert!(api.lock_inflight().is_empty());
        assert_eq!(runtime.ledger.counts(), (0, 0));
    }

    #[tokio::test]
    async fn failed_first_admission_fails_its_waiting_duplicates() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let api = gated_rpc(GatedValidator { gate: Arc::clone(&gate), accept: false }, &runtime);
        let (siblings, options) = request(&fixture);

        let first = tokio::spawn({
            let api = api.clone();
            let (siblings, options) = (siblings.clone(), options.clone());
            async move { api.send_raw_transaction_candidates(siblings, options).await }
        });
        until_pending(&runtime).await;
        let duplicate = tokio::spawn({
            let api = api.clone();
            async move { api.send_raw_transaction_candidates(siblings, options).await }
        });
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        gate.add_permits(1);

        assert!(first.await.unwrap().is_err());
        let duplicate = duplicate.await.unwrap().expect_err("the duplicate mirrors the failure");
        assert_eq!(
            duplicate.data().map(|data| data.get()),
            Some("\"concurrent_submission_failed\"")
        );
        assert!(api.lock_inflight().is_empty());
        assert_eq!(runtime.ledger.counts(), (0, 0));
    }

    #[tokio::test]
    async fn pending_duplicate_waits_instead_of_reporting_acceptance() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let api = rpc(provider(), pool(), &runtime, MAX_EXPIRY_SECS);
        let (siblings, options) = request(&fixture);
        let (envelope, signature) = (options.envelope.clone(), options.envelope_signature);
        let group = CandidateValidator { limits: runtime.limits, chain_id: chain_id() }
            .validate(&siblings.0, envelope, signature, None)
            .unwrap();
        // A first request created the entry; its insertion has not settled.
        let (sender, receiver) = watch::channel(None);
        api.lock_inflight().insert(group.group_hash(), receiver);
        runtime.ledger.create(Arc::new(group), PRINCIPAL, 1, |_| false).unwrap();

        let duplicate = tokio::spawn({
            let api = api.clone();
            async move { api.send_raw_transaction_candidates(siblings, options).await }
        });
        tokio::task::yield_now().await;
        assert!(!duplicate.is_finished(), "duplicate must wait for the first insertion");
        sender.send_replace(Some(false));

        let error = duplicate.await.unwrap().unwrap_err();
        assert_eq!(label(&error), "concurrent_submission_failed");
    }

    #[tokio::test]
    async fn duplicate_after_admin_removal_is_terminal() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let pool = pool();
        let api = rpc(provider(), pool.clone(), &runtime, MAX_EXPIRY_SECS);
        let (siblings, options) = request(&fixture);
        let response =
            api.send_raw_transaction_candidates(siblings.clone(), options.clone()).await.unwrap();
        pool.remove_transactions(vec![response.representative_hash]);

        let error = api.send_raw_transaction_candidates(siblings, options).await.unwrap_err();
        assert_eq!(label(&error), "group_terminal");
    }

    #[tokio::test]
    async fn plain_pooled_representative_is_a_representative_conflict() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let pool = pool();
        let api = rpc(provider(), pool.clone(), &runtime, MAX_EXPIRY_SECS);
        let (siblings, options) = request(&fixture);
        let group = CandidateValidator { limits: runtime.limits, chain_id: chain_id() }
            .validate(&siblings.0, options.envelope.clone(), options.envelope_signature, None)
            .unwrap();
        let representative = group.representative();
        let plain = BasePooledTransaction::new(
            representative.transaction.clone(),
            representative.encoded.len(),
        );
        pool.add_transaction(TransactionOrigin::External, plain).await.unwrap();

        let error = api.send_raw_transaction_candidates(siblings, options).await.unwrap_err();
        assert_eq!(label(&error), "representative_conflict");
        assert_eq!(runtime.ledger.counts(), (0, 0));
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

    /// Events journaled for `group_hash`, as `(type, tx_hash, data)` in emission order.
    fn group_events(
        capture: &TransactionEventCapture,
        group_hash: B256,
    ) -> Vec<(TransactionEventType, Option<TxHash>, serde_json::Map<String, serde_json::Value>)>
    {
        let group_hash = serde_json::json!(format!("{group_hash:#x}"));
        capture
            .events()
            .into_iter()
            .filter(|event| event.data.get("group_hash") == Some(&group_hash))
            .map(|event| (event.event_type, event.tx_hash, event.data))
            .collect()
    }

    #[tokio::test]
    async fn admitted_group_and_its_duplicate_journal_admission() {
        let capture = TransactionEventCapture::install();
        let fixture = CandidateFixture::with_key(chain_id(), 0x71);
        let runtime = runtime(&[fixture.address()]);
        let api = rpc(provider(), pool(), &runtime, MAX_EXPIRY_SECS);

        let (siblings, options) = request(&fixture);
        let response = api.send_raw_transaction_candidates(siblings, options).await.unwrap();
        let (siblings, options) = request(&fixture);
        api.send_raw_transaction_candidates(siblings, options).await.unwrap();

        let events = group_events(&capture, response.group_hash);
        assert_eq!(events.len(), 1, "a duplicate journals no second admission: {events:?}");
        assert_eq!(
            (events[0].0, events[0].1),
            (
                TransactionEventType::TxpoolSendRawTransactionCandidates,
                Some(response.representative_hash)
            )
        );
        assert_eq!(events[0].2["sibling_hashes"], serde_json::json!(response.sibling_hashes));
    }

    /// Holds `first`'s validation on its own gate and every other one on `others`.
    #[derive(Debug, Clone)]
    struct PerHashGatedValidator {
        first: TxHash,
        first_gate: Arc<tokio::sync::Semaphore>,
        others: Arc<tokio::sync::Semaphore>,
    }

    impl reth_transaction_pool::TransactionValidator for PerHashGatedValidator {
        type Transaction = BasePooledTransaction;
        type Block = <OkValidator<BasePooledTransaction> as reth_transaction_pool::TransactionValidator>::Block;

        async fn validate_transaction(
            &self,
            origin: TransactionOrigin,
            transaction: Self::Transaction,
        ) -> TransactionValidationOutcome<Self::Transaction> {
            let gate =
                if *transaction.hash() == self.first { &self.first_gate } else { &self.others };
            let _permit = gate.acquire().await.expect("gate stays open");
            OkValidator::default().validate_transaction(origin, transaction).await
        }
    }

    #[tokio::test]
    async fn concurrent_same_nonce_admissions_journal_one_terminal_per_group() {
        let capture = TransactionEventCapture::install();
        let fixture = CandidateFixture::with_key(chain_id(), 0x76);
        let runtime = runtime(&[fixture.address()]);
        let (first_raw, first_envelope, first_signature) = fixture.submission(2, 0);
        let first = fixture.group(&first_raw, first_envelope.clone());
        // A fee-bumped group at the same nonce, so its insertion replaces the first.
        let second_raw: Vec<_> = (3..5)
            .map(|byte| {
                let mut tx = fixture.sibling_tx(0, Address::repeat_byte(byte), U256::ZERO);
                tx.max_fee_per_gas *= 2;
                tx.max_priority_fee_per_gas *= 2;
                fixture.encode(tx)
            })
            .collect();
        let (second_envelope, second_signature) = fixture.sign_for(&second_raw, 0);
        let second = fixture.group(&second_raw, second_envelope.clone());
        let validator = PerHashGatedValidator {
            first: first.representative().hash(),
            first_gate: Arc::new(tokio::sync::Semaphore::new(0)),
            others: Arc::new(tokio::sync::Semaphore::new(0)),
        };
        let pool = Pool::new(
            validator.clone(),
            CoinbaseTipOrdering::default(),
            InMemoryBlobStore::default(),
            PoolConfig::default(),
        );
        let api = rpc(provider(), pool.clone(), &runtime, MAX_EXPIRY_SECS);
        let submit = |raw, envelope, envelope_signature| {
            let api = api.clone();
            tokio::spawn(async move {
                api.send_raw_transaction_candidates(
                    CandidateSiblings(raw),
                    SendRawTransactionCandidatesOptions { envelope, envelope_signature },
                )
                .await
            })
        };
        let first_request = submit(first_raw, first_envelope, first_signature);
        let second_request = submit(second_raw, second_envelope, second_signature);
        let wait = std::time::Duration::from_secs(5);
        tokio::time::timeout(wait, async {
            while runtime.ledger.counts() != (2, 2) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both admissions reach pool insertion");

        validator.first_gate.add_permits(1);
        tokio::time::timeout(wait, first_request).await.unwrap().unwrap().unwrap();
        assert_eq!(
            runtime.ledger.lookup(&second.group_hash()).unwrap().state,
            GroupState::Pending,
            "an admission still in flight is not replaced"
        );
        validator.others.add_permits(1);
        tokio::time::timeout(wait, second_request).await.unwrap().unwrap().unwrap();
        assert!(!pool.contains(&first.representative().hash()), "the fee bump replaced it");
        runtime.ledger.record_landed(&[second.siblings()[0].hash()], 12, B256::repeat_byte(0xdd));

        let instance = serde_json::json!(format!("{:016x}", runtime.ledger.instance()));
        let terminals = |group: &CandidateGroup| -> Vec<_> {
            group_events(&capture, group.group_hash())
                .into_iter()
                .filter(|(kind, _, data)| {
                    data.get("candidate_instance") == Some(&instance)
                        && matches!(
                            kind,
                            TransactionEventType::TxpoolCandidateEvicted
                                | TransactionEventType::TxpoolCandidateLanded
                        )
                })
                .map(|(_, _, data)| data["terminal_reason"].clone())
                .collect()
        };
        assert_eq!(terminals(&first), [serde_json::json!("replaced")]);
        assert_eq!(terminals(&second), [serde_json::json!("landed")]);
    }

    #[tokio::test]
    async fn admission_whose_nonce_is_consumed_during_insertion_is_terminal() {
        let capture = TransactionEventCapture::install();
        let fixture = CandidateFixture::with_key(chain_id(), 0x75);
        let runtime = runtime(&[fixture.address()]);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let api = gated_rpc(GatedValidator { gate: Arc::clone(&gate), accept: true }, &runtime);
        let (siblings, options) = request(&fixture);
        let group = fixture.group(&siblings.0, options.envelope.clone());

        let first = tokio::spawn({
            let api = api.clone();
            async move { api.send_raw_transaction_candidates(siblings, options).await }
        });
        until_pending(&runtime).await;
        // A canonical block consumes the nonce while the insertion is in flight, and the pool
        // still accepts the outcome validated before it.
        runtime.ledger.record_nonces([(group.sender(), group.nonce() + 1)]);
        gate.add_permits(1);

        let error = first.await.unwrap().expect_err("the group ended at settlement");
        assert_eq!(label(&error), "group_terminal");
        assert!(!api.pool.contains(&group.representative().hash()), "stale representative removed");
        assert_eq!(runtime.ledger.counts(), (0, 1), "the sender's slot is free");
        assert!(api.lock_inflight().is_empty());
        let terminals: Vec<_> = group_events(&capture, group.group_hash())
            .into_iter()
            .filter(|(kind, _, _)| *kind == TransactionEventType::TxpoolCandidateEvicted)
            .map(|(_, _, data)| data["terminal_reason"].clone())
            .collect();
        assert_eq!(terminals, [serde_json::json!("nonce_consumed")]);
    }

    #[tokio::test]
    async fn repeated_rejections_of_one_group_keep_distinct_ids() {
        let capture = TransactionEventCapture::install();
        let fixture = CandidateFixture::with_key(chain_id(), 0x74);
        let unregistered = runtime(&[Address::repeat_byte(0x99)]);
        let api = rpc(provider(), pool(), &unregistered, MAX_EXPIRY_SECS);
        let mut group_hash = B256::ZERO;
        for _ in 0..2 {
            let (siblings, options) = request(&fixture);
            group_hash = options.envelope.signing_hash(chain_id());
            api.send_raw_transaction_candidates(siblings, options).await.unwrap_err();
        }
        // A request that fails before validation has no hash, yet keeps its own identity.
        for _ in 0..2 {
            let (_, options) = request(&fixture);
            api.send_raw_transaction_candidates(CandidateSiblings(vec![]), options)
                .await
                .unwrap_err();
        }

        let instance = serde_json::json!(format!("{:016x}", unregistered.ledger.instance()));
        let rejected: Vec<_> = capture
            .events()
            .into_iter()
            .filter(|event| event.data.get("candidate_instance") == Some(&instance))
            .collect();
        assert_eq!(rejected.len(), 4, "{rejected:?}");
        let ids: std::collections::HashSet<_> =
            rejected.iter().map(|event| event.event_id.clone()).collect();
        assert_eq!(ids.len(), 4, "no rejection deduplicates against another");
        assert_eq!(group_events(&capture, group_hash).len(), 2);
        assert!(rejected[2..].iter().all(|event| event.tx_hash.is_none()));
    }

    #[test]
    fn unlabeled_errors_are_pool_rejected_only_after_pool_refusal() {
        let error = ErrorObjectOwned::owned::<()>(-32603, "provider failure", None);
        let trace = CandidateAdmissionTrace::default();
        assert_eq!(trace.rejection_label(&error), "internal_error");
        let trace = CandidateAdmissionTrace { pool_rejected: true, ..trace };
        assert_eq!(trace.rejection_label(&error), "pool_rejected");
        let labeled = SendRawTransactionCandidatesApiImpl::<(), ()>::reject(
            &CandidateError::PrincipalNotAllowed,
        );
        assert_eq!(trace.rejection_label(&labeled), "principal_not_allowed");
    }

    #[tokio::test]
    async fn runtime_disabled_ingress_journals_nothing() {
        let capture = TransactionEventCapture::install();
        let fixture = CandidateFixture::with_key(chain_id(), 0x75);
        let runtime = runtime(&[fixture.address()]);
        runtime.switch.set(false);
        let api = rpc(provider(), pool(), &runtime, MAX_EXPIRY_SECS);
        let (siblings, options) = request(&fixture);
        api.send_raw_transaction_candidates(siblings, options).await.unwrap_err();

        let instance = serde_json::json!(format!("{:016x}", runtime.ledger.instance()));
        assert!(
            !capture
                .events()
                .iter()
                .any(|event| event.data.get("candidate_instance") == Some(&instance))
        );
    }

    #[tokio::test]
    async fn rejected_groups_journal_their_reason_under_the_representative() {
        let capture = TransactionEventCapture::install();
        let rejected = TransactionEventType::TxpoolSendRawTransactionCandidatesRejected;

        let fixture = CandidateFixture::with_key(chain_id(), 0x72);
        let unregistered = runtime(&[Address::repeat_byte(0x99)]);
        let api = rpc(provider(), pool(), &unregistered, MAX_EXPIRY_SECS);
        let (siblings, options) = request(&fixture);
        let group_hash = options.envelope.signing_hash(chain_id());
        let representative = options.envelope.sibling_hashes[1];
        api.send_raw_transaction_candidates(siblings, options).await.unwrap_err();
        let events = group_events(&capture, group_hash);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!((events[0].0, events[0].1), (rejected, Some(representative)));
        assert_eq!(events[0].2["rejection_reason"], "principal_not_allowed");

        // A pool error carries no candidate label.
        let fixture = CandidateFixture::with_key(chain_id(), 0x73);
        let runtime = runtime(&[fixture.address()]);
        let api = rpc(
            provider(),
            NoopTransactionPool::<BasePooledTransaction>::new(),
            &runtime,
            MAX_EXPIRY_SECS,
        );
        let (siblings, options) = request(&fixture);
        let group_hash = options.envelope.signing_hash(chain_id());
        api.send_raw_transaction_candidates(siblings, options).await.unwrap_err();
        let events = group_events(&capture, group_hash);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].0, rejected);
        assert_eq!(events[0].2["rejection_reason"], "pool_rejected");
    }
}
