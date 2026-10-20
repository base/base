//! `base_sendSwapTransaction`, the Swap Lane submission endpoint, and its runtime kill switch.

use std::sync::Arc;

use alloy_consensus::BlockHeader;
use alloy_primitives::{B256, Bytes, Signature, TxHash, U256};
use base_common_chains::Upgrades;
use base_execution_candidates::{
    AccessDenied, CandidateEnvelope, CandidateError, CandidateGroup, CandidateRuntime,
    CandidateValidator, GroupLedger, LedgerError,
};
use base_execution_rpc::SequencerClient;
use base_execution_txpool::{AdmissionError, AdmissionService, BasePooledTransaction};
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
use reth_transaction_pool::TransactionPool;
use serde::{Deserialize, Deserializer, Serialize};
use tracing::{debug, info};

use crate::BlockCadence;

/// The Swap Lane submission method.
pub const SEND_SWAP_TRANSACTION_METHOD: &str = "base_sendSwapTransaction";

/// Signed EIP-2718 alternatives to a swap's default transaction.
///
/// Deserialization stops before allocating past
/// [`base_execution_candidates::MAX_SIBLINGS_CEILING`]; the configured limit, which counts the
/// default transaction too, is enforced by [`CandidateValidator`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct SwapAlternatives(pub Vec<Bytes>);

impl SwapAlternatives {
    /// Returns `true` when the request carries no alternatives.
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<'de> Deserialize<'de> for SwapAlternatives {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        CandidateEnvelope::deserialize_bounded(deserializer).map(Self)
    }
}

/// Signed selection over a swap's default transaction and its alternatives.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SwapSelection {
    /// Signed `CandidateSet` listing the default transaction and every alternative. The
    /// default transaction must be its representative: `siblingHashes[defaultIndex]` under
    /// `DEFAULT_SIBLING`, `siblingHashes[0]` under `DROP`.
    pub envelope: CandidateEnvelope,
    /// Sender's EIP-712 signature over the envelope.
    pub signature: Signature,
}

/// Options for `base_sendSwapTransaction`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SwapTransactionOptions {
    /// Signed alternatives to the default transaction, with its sender and nonce.
    #[serde(default, skip_serializing_if = "SwapAlternatives::is_empty")]
    pub alternatives: SwapAlternatives,
    /// Required exactly when `alternatives` is non-empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<SwapSelection>,
    /// Upper bound, in wei, on the Swap Lane access charge; zero when omitted. With a
    /// `selection` it defaults to the envelope's `maxAccessCharge` and must equal it, even while
    /// selection is disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_access_charge: Option<U256>,
}

/// `maxAccessCharge` resolved for one submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapTerms {
    /// Upper bound, in wei, on the access charge.
    pub max_access_charge: U256,
}

impl SwapTransactionOptions {
    /// Checks that alternatives and a selection come together.
    pub const fn check_shape(&self) -> Result<(), CandidateError> {
        match (self.alternatives.is_empty(), self.selection.is_some()) {
            (false, false) => Err(CandidateError::SelectionRequired),
            (true, true) => Err(CandidateError::SelectionWithoutAlternatives),
            _ => Ok(()),
        }
    }

    /// Resolves the submission's terms. A selection envelope supplies any value the request
    /// omits; a value given in both places must match.
    ///
    /// The check runs whether or not the selection is applied, so a request is valid or invalid
    /// the same way under either kill-switch state: with selection disabled, a mismatch is still
    /// `selection_mismatch` although the envelope's signature is not verified.
    pub fn terms(&self) -> Result<SwapTerms, CandidateError> {
        let Some(selection) = &self.selection else {
            return Ok(SwapTerms { max_access_charge: self.max_access_charge.unwrap_or_default() });
        };
        let envelope = &selection.envelope;
        if self.max_access_charge.is_some_and(|charge| charge != envelope.max_access_charge) {
            return Err(CandidateError::SelectionFieldMismatch { field: "maxAccessCharge" });
        }
        Ok(SwapTerms { max_access_charge: envelope.max_access_charge })
    }
}

/// Response for an accepted (or exact-duplicate) swap transaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapTransactionResponse {
    /// Hash of the default transaction. With selection applied it is the group's pool and
    /// iterator identity, not necessarily the transaction that lands.
    pub transaction_hash: TxHash,
    /// EIP-712 signing hash of the selection envelope, the group's identity. Present only when
    /// selection applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_hash: Option<B256>,
    /// The envelope's sibling hashes other than `transactionHash`, in envelope order. Present
    /// only when selection applied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternative_hashes: Vec<TxHash>,
    /// The request carried alternatives, but selection is disabled, so only the default
    /// transaction was admitted. Omitted when `false`.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub alternatives_ignored: bool,
}

impl From<&CandidateGroup> for SwapTransactionResponse {
    fn from(group: &CandidateGroup) -> Self {
        let default = group.representative().hash();
        Self {
            transaction_hash: default,
            group_hash: Some(group.group_hash()),
            alternative_hashes: group
                .envelope()
                .sibling_hashes
                .iter()
                .copied()
                .filter(|hash| *hash != default)
                .collect(),
            alternatives_ignored: false,
        }
    }
}

/// RPC API for the Swap Lane submission endpoint (design §2).
#[rpc(server, namespace = "base")]
pub trait SendSwapTransactionApi {
    /// Submits a signed default transaction and, optionally, signed alternatives from which the
    /// builder selects at most one to include instead.
    ///
    /// While selection is disabled, alternatives are ignored and the selection's signature is
    /// not verified, but a `maxAccessCharge` given alongside a selection must still match its
    /// envelope (`selection_mismatch`). Accepted transactions are pooled with Private origin on
    /// the node that received them; see the crate README for how they reach the sequencer.
    #[method(name = "sendSwapTransaction")]
    async fn send_swap_transaction(
        &self,
        transaction: Bytes,
        options: Option<SwapTransactionOptions>,
    ) -> RpcResult<SwapTransactionResponse>;
}

/// Per-process runtime toggle for candidate selection (design §8.4).
///
/// A single-process call is not a cluster kill switch.
#[rpc(server, namespace = "admin")]
pub trait AdminCandidateSelectionApi {
    /// Enables or disables candidate selection. Returns the previous state. While disabled,
    /// `base_sendSwapTransaction` admits only default transactions.
    #[method(name = "setCandidateSelectionEnabled")]
    async fn set_candidate_selection_enabled(&self, enabled: bool) -> RpcResult<bool>;
}

/// Local ingress for swap transactions over the process-shared [`AdmissionService`].
#[derive(Debug, Clone)]
pub struct SendSwapTransactionApiImpl<Provider, Pool> {
    admission: Arc<AdmissionService<Provider, Pool>>,
    chain_id: u64,
    sequencer_client: Option<SequencerClient>,
}

impl<Provider, Pool> SendSwapTransactionApiImpl<Provider, Pool> {
    /// Creates local swap ingress over the shared admission service.
    pub const fn new(admission: Arc<AdmissionService<Provider, Pool>>, chain_id: u64) -> Self {
        Self { admission, chain_id, sequencer_client: None }
    }

    /// Proxies submissions unchanged to one upstream node instead of the local pool.
    pub fn with_sequencer_client(mut self, client: SequencerClient) -> Self {
        self.sequencer_client = Some(client);
        self
    }

    /// Returns the shared candidate runtime.
    pub fn runtime(&self) -> &CandidateRuntime {
        self.admission.runtime()
    }

    /// Maps a rejection to its `-32602` error with the stable reason label.
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

    /// Returns the stable label vocabulary entry for an admission refusal, or `None` for a
    /// pool refusal or a state read failure, which have no label of their own.
    pub const fn candidate_error(error: &AdmissionError) -> Option<CandidateError> {
        Some(match error {
            AdmissionError::SenderDelegated => CandidateError::SenderDelegated,
            AdmissionError::PrincipalNotAllowed
            | AdmissionError::AccessDenied(AccessDenied::NotAllowed) => {
                CandidateError::PrincipalNotAllowed
            }
            AdmissionError::AccessDenied(AccessDenied::AccessChargeTooLow) => {
                CandidateError::AccessChargeTooLow
            }
            AdmissionError::AccessDenied(AccessDenied::QuotaExhausted) => {
                CandidateError::PrincipalQuotaExhausted
            }
            AdmissionError::Ledger(LedgerError::GroupTerminal) => CandidateError::GroupTerminal,
            AdmissionError::Ledger(LedgerError::RepresentativeConflict) => {
                CandidateError::RepresentativeConflict
            }
            AdmissionError::Ledger(LedgerError::SenderGroupLimit) => {
                CandidateError::SenderGroupLimit
            }
            AdmissionError::Ledger(LedgerError::PoolGroupLimit) => CandidateError::PoolGroupLimit,
            AdmissionError::ConcurrentSubmissionFailed => {
                CandidateError::ConcurrentSubmissionFailed
            }
            AdmissionError::TimedOut => CandidateError::AdmissionTimedOut,
            AdmissionError::Pool(_) | AdmissionError::State(_) => return None,
        })
    }

    /// Maps an admission refusal to its JSON-RPC error, recording a pool refusal in `trace`.
    pub fn admission_error(
        error: AdmissionError,
        trace: &mut SwapAdmissionTrace,
    ) -> ErrorObjectOwned {
        if let Some(label) = Self::candidate_error(&error) {
            return Self::reject(&label);
        }
        match error {
            AdmissionError::Pool(error) => {
                trace.pool_rejected = true;
                ErrorObjectOwned::from(EthApiError::from(error))
            }
            error => Self::internal(error.to_string()),
        }
    }
}

impl<Provider, Pool> SendSwapTransactionApiImpl<Provider, Pool>
where
    Provider: BlockReaderIdExt + ChainSpecProvider<ChainSpec: Upgrades>,
{
    /// Returns the node-assigned inclusive expiry block for a group admitted now: the
    /// configured group lifetime past the next block, at the active full-block cadence.
    pub fn group_expiry_block(&self) -> RpcResult<u64> {
        let provider = self.admission.provider();
        let header = provider
            .latest_header()
            .map_err(|error| Self::internal(format!("failed to resolve latest header: {error}")))?;
        let (number, timestamp) =
            header.map_or((0, 0), |header| (header.number(), header.timestamp()));
        let interval = BlockCadence::full_block_interval_millis(&*provider.chain_spec(), timestamp);
        Ok(self.admission.group_expiry_block(number.saturating_add(1), interval))
    }
}

#[async_trait]
impl<Provider, Pool> SendSwapTransactionApiServer for SendSwapTransactionApiImpl<Provider, Pool>
where
    Provider: BlockReaderIdExt
        + ChainSpecProvider<ChainSpec: Upgrades>
        + StateProviderFactory
        + Clone
        + 'static,
    Pool: TransactionPool<Transaction = BasePooledTransaction> + Clone + 'static,
{
    async fn send_swap_transaction(
        &self,
        transaction: Bytes,
        options: Option<SwapTransactionOptions>,
    ) -> RpcResult<SwapTransactionResponse> {
        // Proxies forward unchanged and keep no copy; the sequencer journals the request.
        if let Some(client) = &self.sequencer_client {
            return client
                .request(SEND_SWAP_TRANSACTION_METHOD, (transaction, options))
                .await
                .map_err(ErrorObjectOwned::from);
        }
        let mut trace = SwapAdmissionTrace::default();
        let result = self.submit(transaction, options.unwrap_or_default(), &mut trace).await;
        trace.journal(&self.runtime().ledger, &result);
        result
    }
}

impl<Provider, Pool> SendSwapTransactionApiImpl<Provider, Pool>
where
    Provider: BlockReaderIdExt
        + ChainSpecProvider<ChainSpec: Upgrades>
        + StateProviderFactory
        + Clone
        + 'static,
    Pool: TransactionPool<Transaction = BasePooledTransaction> + Clone + 'static,
{
    /// Routes a submission: a candidate group when it carries alternatives and selection is
    /// enabled, otherwise the default transaction alone. Fills `trace` as the join keys become
    /// known.
    ///
    /// Shape and term checks run before routing, so with selection disabled an ignored
    /// selection is not verified but its terms must still match (see
    /// [`SwapTransactionOptions::terms`]).
    pub async fn submit(
        &self,
        transaction: Bytes,
        options: SwapTransactionOptions,
        trace: &mut SwapAdmissionTrace,
    ) -> RpcResult<SwapTransactionResponse> {
        trace.alternatives = options.alternatives.0.len();
        options.check_shape().map_err(|error| Self::reject(&error))?;
        let terms = options.terms().map_err(|error| Self::reject(&error))?;
        match options.selection {
            Some(selection) if self.runtime().switch.is_enabled() => {
                self.submit_group(transaction, options.alternatives, selection, trace).await
            }
            selection => {
                trace.alternatives_ignored = selection.is_some();
                self.submit_single(transaction, terms, trace).await
            }
        }
    }

    /// Admits a default transaction alone (design §5.1 without selection) through
    /// [`AdmissionService::admit_single`].
    ///
    /// The transaction keeps the per-transaction size, type and gas caps and carries no
    /// validity predicate. v0 prices no work for it, so `maxAccessCharge` is recorded but not
    /// charged. An exact resubmission returns the same response and changes nothing.
    pub async fn submit_single(
        &self,
        transaction: Bytes,
        terms: SwapTerms,
        trace: &mut SwapAdmissionTrace,
    ) -> RpcResult<SwapTransactionResponse> {
        let validator =
            CandidateValidator { limits: self.runtime().limits, chain_id: self.chain_id };
        let default = validator.validate_default(&transaction).map_err(|error| {
            debug!(error = %error, label = error.as_label(), "rejected swap transaction");
            Self::reject(&error)
        })?;
        trace.transaction_hash = Some(default.hash());
        let receipt = self
            .admission
            .admit_single(default)
            .await
            .map_err(|error| Self::admission_error(error, trace))?;
        trace.duplicate = receipt.is_duplicate();
        if let Some(authorization) = receipt.authorization {
            info!(
                tx_hash = %receipt.transaction_hash,
                max_access_charge = %terms.max_access_charge,
                alternatives_ignored = trace.alternatives_ignored,
                principal = authorization.principal.0,
                "accepted swap transaction"
            );
        }
        Ok(SwapTransactionResponse {
            transaction_hash: receipt.transaction_hash,
            group_hash: None,
            alternative_hashes: Vec::new(),
            alternatives_ignored: trace.alternatives_ignored,
        })
    }

    /// Admits a default transaction and its alternatives as a candidate group (design §5.1)
    /// through [`AdmissionService::admit_group`], with the node-assigned expiry block.
    pub async fn submit_group(
        &self,
        transaction: Bytes,
        alternatives: SwapAlternatives,
        selection: SwapSelection,
        trace: &mut SwapAdmissionTrace,
    ) -> RpcResult<SwapTransactionResponse> {
        // Steps 3-5: bounds, decode, uniformity, envelope and signature.
        let expiry_block = self.group_expiry_block()?;
        let validator =
            CandidateValidator { limits: self.runtime().limits, chain_id: self.chain_id };
        let group = validator
            .validate_selection(
                &transaction,
                &alternatives.0,
                selection.envelope,
                selection.signature,
                expiry_block,
            )
            .map_err(|error| {
                debug!(error = %error, label = error.as_label(), "rejected candidate group");
                Self::reject(&error)
            })?;
        trace.group_hash = Some(group.group_hash());
        trace.transaction_hash = Some(group.representative().hash());
        let group = Arc::new(group);
        let response = SwapTransactionResponse::from(group.as_ref());
        let receipt = self
            .admission
            .admit_group(Arc::clone(&group))
            .await
            .map_err(|error| Self::admission_error(error, trace))?;
        trace.duplicate = receipt.is_duplicate();
        if let Some(authorization) = receipt.authorization {
            info!(
                group_hash = %group.group_hash(),
                representative = %response.transaction_hash,
                alternatives = response.alternative_hashes.len(),
                expiry_block = expiry_block,
                principal = authorization.principal.0,
                "accepted candidate group"
            );
        }
        Ok(response)
    }
}

/// Join keys and outcome of one local `base_sendSwapTransaction` request, journaled when it
/// returns. A request whose future is dropped journals nothing;
/// [`base_execution_txpool::PendingAdmission`] still settles its ledger entry.
#[derive(Debug, Default, Clone, Copy)]
pub struct SwapAdmissionTrace {
    /// Group identity, once a selection validated.
    pub group_hash: Option<B256>,
    /// Default transaction hash, once it decoded.
    pub transaction_hash: Option<TxHash>,
    /// Number of alternatives the request carried.
    pub alternatives: usize,
    /// The request carried alternatives, but selection was disabled.
    pub alternatives_ignored: bool,
    /// The request returned, or waited on, an existing admission of the same transaction or
    /// group.
    pub duplicate: bool,
    /// The pool refused the default transaction, so an unlabeled error is `pool_rejected`.
    pub pool_rejected: bool,
}

impl SwapAdmissionTrace {
    /// Journals `TXPOOL_SEND_SWAP_TRANSACTION` once per admission, or
    /// `TXPOOL_SEND_SWAP_TRANSACTION_REJECTED` with the error's stable label.
    ///
    /// An admission's event ID is the default transaction's hash (plus the group hash for a
    /// group), so a replayed event deduplicates; a duplicate submission returns before
    /// journaling. Each rejection's ID includes a never-reused [`GroupLedger::next_sequence`],
    /// because a request may be rejected repeatedly and a malformed one has no hash at all.
    pub fn journal(&self, ledger: &GroupLedger, result: &RpcResult<SwapTransactionResponse>) {
        let instance = format!("{:016x}", ledger.instance());
        let selection = self.selection_label();
        let _ = match result {
            Ok(_) if self.duplicate => return,
            Ok(response @ SwapTransactionResponse { group_hash: Some(group_hash), .. }) => {
                transaction_event!(
                    producer: TransactionEventProducer::BaseRethNode,
                    event_type: TransactionEventType::TxpoolSendSwapTransaction,
                    tx_hash: response.transaction_hash,
                    id: { "group_hash" => format!("{group_hash:#x}") },
                    data: {
                        "rpc_method" => SEND_SWAP_TRANSACTION_METHOD,
                        "selection" => selection,
                        "group_hash" => format!("{group_hash:#x}"),
                        "alternative_hashes" => &response.alternative_hashes,
                        "candidate_instance" => instance,
                    },
                )
            }
            Ok(response) => transaction_event!(
                producer: TransactionEventProducer::BaseRethNode,
                event_type: TransactionEventType::TxpoolSendSwapTransaction,
                tx_hash: response.transaction_hash,
                data: {
                    "rpc_method" => SEND_SWAP_TRANSACTION_METHOD,
                    "selection" => selection,
                    "alternatives" => self.alternatives,
                    "candidate_instance" => instance,
                },
            ),
            Err(error) => transaction_event!(
                producer: TransactionEventProducer::BaseRethNode,
                event_type: TransactionEventType::TxpoolSendSwapTransactionRejected,
                maybe_tx_hash: self.transaction_hash,
                id: {
                    "candidate_instance" => &instance,
                    "rejection_sequence" => ledger.next_sequence(),
                },
                data: {
                    "rpc_method" => SEND_SWAP_TRANSACTION_METHOD,
                    "selection" => selection,
                    "group_hash" => self.group_hash.map(|hash| format!("{hash:#x}")),
                    "alternatives" => self.alternatives,
                    "rejection_reason" => self.rejection_label(error),
                    "error_code" => error.code(),
                    "candidate_instance" => instance,
                },
            ),
        };
    }

    /// `applied` when the request's alternatives went to selection, `ignored` when selection
    /// was disabled, and `none` without alternatives.
    pub const fn selection_label(&self) -> &'static str {
        match (self.alternatives, self.alternatives_ignored) {
            (0, _) => "none",
            (_, true) => "ignored",
            _ => "applied",
        }
    }

    /// The stable reason label of a rejection. An error without one is `pool_rejected` when
    /// the pool refused the default transaction and `internal_error` otherwise, such as a
    /// failed state read.
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
    use std::time::Duration;

    use alloy_primitives::{Address, U256, keccak256};
    use base_common_chains::ChainConfig;
    use base_common_consensus::BasePrimitives;
    use base_execution_candidates::{
        CandidateLimits, CandidateSwitch, Capability, EvictionReason, GroupLedger, GroupState,
        PrincipalEntry, PrincipalId, PrincipalsFile, StaticAccessPolicy,
        test_utils::{CandidateFixture, FIXTURE_EXPIRY_BLOCK},
    };
    use base_execution_chainspec::BaseChainSpec;
    use base_execution_txpool::{BasePooledTx, DEFAULT_ADMISSION_TIMEOUT};
    use base_observability_events::TransactionEventCapture;
    use base_test_utils::build_test_genesis_everest;
    use httpmock::{Method::POST, MockServer};
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_transaction_pool::{
        CoinbaseTipOrdering, Pool, PoolConfig, PoolTransaction, TransactionOrigin,
        TransactionValidationOutcome, blobstore::InMemoryBlobStore, noop::NoopTransactionPool,
        test_utils::OkValidator,
    };
    use tokio::sync::watch;

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
            principals: vec![PrincipalEntry {
                id: PRINCIPAL,
                senders: allowed.to_vec(),
                capabilities: Capability::swap_lane(),
            }],
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
        group_lifetime_secs: u64,
    ) -> SendSwapTransactionApiImpl<TestProvider, P> {
        let admission =
            AdmissionService::new(provider, pool, Arc::clone(runtime), group_lifetime_secs);
        SendSwapTransactionApiImpl::new(Arc::new(admission), chain_id())
    }

    fn request(fixture: &CandidateFixture) -> (Bytes, SwapTransactionOptions) {
        let (raw, envelope, signature) = fixture.submission(3, 1);
        swap(&raw, envelope, signature)
    }

    /// Splits signed siblings into a default transaction and swap options selecting over them.
    fn swap(
        raw: &[Bytes],
        envelope: CandidateEnvelope,
        signature: Signature,
    ) -> (Bytes, SwapTransactionOptions) {
        let (default, alternatives) = CandidateFixture::split_default(raw, &envelope);
        let options = SwapTransactionOptions {
            alternatives: SwapAlternatives(alternatives),
            selection: Some(SwapSelection { envelope, signature }),
            ..Default::default()
        };
        (default, options)
    }

    /// A default transaction alone.
    fn single(fixture: &CandidateFixture, nonce: u64) -> (Bytes, SwapTransactionOptions) {
        let default =
            fixture.encode(fixture.sibling_tx(nonce, Address::repeat_byte(1), U256::ZERO));
        (default, SwapTransactionOptions::default())
    }

    fn selection(options: &SwapTransactionOptions) -> &SwapSelection {
        options.selection.as_ref().expect("request carries a selection")
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
        let (default, options) = request(&fixture);
        let expected_group = selection(&options).envelope.signing_hash(chain_id());
        let expected_siblings = selection(&options).envelope.sibling_hashes.clone();

        let response = api.send_swap_transaction(default, Some(options)).await.unwrap();

        assert_eq!(response.group_hash, Some(expected_group));
        // DEFAULT_SIBLING with defaultIndex 1: the default transaction is sibling 1.
        assert_eq!(response.transaction_hash, expected_siblings[1]);
        assert_eq!(response.alternative_hashes, [expected_siblings[0], expected_siblings[2]]);
        assert!(!response.alternatives_ignored);
        let pooled = pool.get(&response.transaction_hash).expect("representative pooled");
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
        let (default, options) = request(&fixture);

        let first =
            api.send_swap_transaction(default.clone(), Some(options.clone())).await.unwrap();
        let second = api.send_swap_transaction(default, Some(options)).await.unwrap();

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
        timeout: Duration,
    ) -> SendSwapTransactionApiImpl<
        TestProvider,
        Pool<GatedValidator, CoinbaseTipOrdering<BasePooledTransaction>, InMemoryBlobStore>,
    > {
        let pool = Pool::new(
            validator,
            CoinbaseTipOrdering::default(),
            InMemoryBlobStore::default(),
            PoolConfig::default(),
        );
        let admission =
            AdmissionService::new(provider(), pool, Arc::clone(runtime), MAX_EXPIRY_SECS)
                .with_timeout(timeout);
        SendSwapTransactionApiImpl::new(Arc::new(admission), chain_id())
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
        let api = gated_rpc(GatedValidator::stalled(), &runtime, DEFAULT_ADMISSION_TIMEOUT);
        let (default, options) = request(&fixture);

        let first = tokio::spawn({
            let api = api.clone();
            async move { api.send_swap_transaction(default, Some(options)).await }
        });
        until_pending(&runtime).await;
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());

        assert!(api.admission.lock_inflight().is_empty(), "no orphaned in-flight receiver");
        assert_eq!(runtime.ledger.counts(), (0, 0), "no orphaned Pending entry");
    }

    #[tokio::test]
    async fn stalled_admission_and_its_duplicates_time_out() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let api = gated_rpc(GatedValidator::stalled(), &runtime, Duration::from_millis(50));
        let (default, options) = request(&fixture);

        let first = tokio::spawn({
            let api = api.clone();
            let (default, options) = (default.clone(), options.clone());
            async move { api.send_swap_transaction(default, Some(options)).await }
        });
        until_pending(&runtime).await;
        let duplicate = tokio::time::timeout(
            Duration::from_secs(5),
            api.send_swap_transaction(default, Some(options)),
        )
        .await
        .expect("the duplicate wait is bounded");

        let first = first.await.unwrap().expect_err("a stalled insertion times out");
        assert_eq!(first.data().map(|data| data.get()), Some("\"admission_timeout\""));
        assert!(duplicate.is_err(), "a duplicate never reports acceptance of a stalled group");
        assert!(api.admission.lock_inflight().is_empty());
        assert_eq!(runtime.ledger.counts(), (0, 0));
    }

    #[tokio::test]
    async fn adapters_sharing_one_admission_service_share_inflight_admissions() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let pool = Pool::new(
            GatedValidator { gate: Arc::clone(&gate), accept: false },
            CoinbaseTipOrdering::default(),
            InMemoryBlobStore::default(),
            PoolConfig::default(),
        );
        let admission = Arc::new(AdmissionService::new(
            provider(),
            pool,
            Arc::clone(&runtime),
            MAX_EXPIRY_SECS,
        ));
        let first = SendSwapTransactionApiImpl::new(Arc::clone(&admission), chain_id());
        let second = SendSwapTransactionApiImpl::new(Arc::clone(&admission), chain_id());
        let (default, options) = request(&fixture);

        let pending = tokio::spawn({
            let (default, options) = (default.clone(), options.clone());
            async move { first.send_swap_transaction(default, Some(options)).await }
        });
        until_pending(&runtime).await;
        // The second adapter finds the first one's admission in flight and waits for it,
        // instead of creating its own entry or reporting acceptance early.
        let duplicate =
            tokio::spawn(async move { second.send_swap_transaction(default, Some(options)).await });
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(!duplicate.is_finished(), "the duplicate waits for the first admission");
        gate.add_permits(1);

        assert!(pending.await.unwrap().is_err());
        let duplicate = duplicate.await.unwrap().expect_err("the duplicate mirrors the failure");
        assert_eq!(label(&duplicate), "concurrent_submission_failed");
        assert!(admission.lock_inflight().is_empty());
        assert_eq!(runtime.ledger.counts(), (0, 0));
    }

    #[tokio::test]
    async fn failed_first_admission_fails_its_waiting_duplicates() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let api = gated_rpc(
            GatedValidator { gate: Arc::clone(&gate), accept: false },
            &runtime,
            DEFAULT_ADMISSION_TIMEOUT,
        );
        let (default, options) = request(&fixture);

        let first = tokio::spawn({
            let api = api.clone();
            let (default, options) = (default.clone(), options.clone());
            async move { api.send_swap_transaction(default, Some(options)).await }
        });
        until_pending(&runtime).await;
        let duplicate = tokio::spawn({
            let api = api.clone();
            async move { api.send_swap_transaction(default, Some(options)).await }
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
        assert!(api.admission.lock_inflight().is_empty());
        assert_eq!(runtime.ledger.counts(), (0, 0));
    }

    #[tokio::test]
    async fn pending_duplicate_waits_instead_of_reporting_acceptance() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let api = rpc(provider(), pool(), &runtime, MAX_EXPIRY_SECS);
        let (default, options) = request(&fixture);
        let (envelope, signature) =
            (selection(&options).envelope.clone(), selection(&options).signature);
        let group = CandidateValidator { limits: runtime.limits, chain_id: chain_id() }
            .validate_selection(
                &default,
                &options.alternatives.0,
                envelope,
                signature,
                FIXTURE_EXPIRY_BLOCK,
            )
            .unwrap();
        // A first request created the entry; its insertion has not settled.
        let (sender, receiver) = watch::channel(None);
        api.admission.lock_inflight().insert(group.group_hash(), receiver);
        runtime.ledger.create(Arc::clone(group.descriptor()), PRINCIPAL, 1, |_| false).unwrap();

        let duplicate = tokio::spawn({
            let api = api.clone();
            async move { api.send_swap_transaction(default, Some(options)).await }
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
        let (default, options) = request(&fixture);
        let response =
            api.send_swap_transaction(default.clone(), Some(options.clone())).await.unwrap();
        pool.remove_transactions(vec![response.transaction_hash]);

        let error = api.send_swap_transaction(default, Some(options)).await.unwrap_err();
        assert_eq!(label(&error), "group_terminal");
    }

    #[tokio::test]
    async fn plain_pooled_representative_is_a_representative_conflict() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let pool = pool();
        let api = rpc(provider(), pool.clone(), &runtime, MAX_EXPIRY_SECS);
        let (default, options) = request(&fixture);
        let group = CandidateValidator { limits: runtime.limits, chain_id: chain_id() }
            .validate_selection(
                &default,
                &options.alternatives.0,
                selection(&options).envelope.clone(),
                selection(&options).signature,
                FIXTURE_EXPIRY_BLOCK,
            )
            .unwrap();
        let representative = group.representative();
        let plain = BasePooledTransaction::new(
            representative.transaction.clone(),
            representative.encoded.len(),
        );
        pool.add_transaction(TransactionOrigin::External, plain).await.unwrap();

        let error = api.send_swap_transaction(default, Some(options)).await.unwrap_err();
        assert_eq!(label(&error), "representative_conflict");
        assert_eq!(runtime.ledger.counts(), (0, 0));
    }

    #[tokio::test]
    async fn terminal_group_is_rejected() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let api = rpc(provider(), pool(), &runtime, MAX_EXPIRY_SECS);
        let (default, options) = request(&fixture);
        let response =
            api.send_swap_transaction(default.clone(), Some(options.clone())).await.unwrap();
        runtime.ledger.evict(response.group_hash.unwrap(), EvictionReason::BudgetExhausted);

        let error = api.send_swap_transaction(default, Some(options)).await.unwrap_err();
        assert_eq!(label(&error), "group_terminal");
    }

    #[tokio::test]
    async fn disabled_selection_admits_the_default_and_ignores_alternatives() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let pool = pool();
        let api = rpc(provider(), pool.clone(), &runtime, MAX_EXPIRY_SECS);
        let admin = AdminCandidateSelectionApiImpl::new(Arc::clone(&runtime));

        assert!(admin.set_candidate_selection_enabled(false).await.unwrap());
        let (default, options) = request(&fixture);
        let default_hash = selection(&options).envelope.sibling_hashes[1];
        let response =
            api.send_swap_transaction(default.clone(), Some(options.clone())).await.unwrap();
        assert_eq!(
            response,
            SwapTransactionResponse {
                transaction_hash: default_hash,
                group_hash: None,
                alternative_hashes: vec![],
                alternatives_ignored: true,
            }
        );
        let pooled = pool.get(&default_hash).expect("default pooled");
        assert!(BasePooledTx::candidate_group(&pooled.transaction).is_none());
        assert_eq!(runtime.ledger.counts(), (0, 0));
        assert_eq!(pool.pool_size().total, 1, "alternatives are not pooled");
    }

    #[tokio::test]
    async fn default_alone_is_pooled_privately_without_a_validity_predicate() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let pool = pool();
        let api = rpc(provider(), pool.clone(), &runtime, MAX_EXPIRY_SECS);
        let (default, options) = single(&fixture, 0);
        let hash = keccak256(&default);

        let response = api.send_swap_transaction(default.clone(), Some(options.clone())).await;
        let response = response.unwrap();
        assert_eq!(response.transaction_hash, hash);
        assert_eq!(response.group_hash, None);
        assert!(response.alternative_hashes.is_empty() && !response.alternatives_ignored);

        let pooled = pool.get(&hash).expect("default pooled");
        assert_eq!(pooled.origin, TransactionOrigin::Private);
        assert!(BasePooledTx::candidate_group(&pooled.transaction).is_none());
        assert!(pooled.transaction.validity_predicates().is_empty(), "no node-made predicate");
        assert_eq!(runtime.ledger.counts(), (0, 0), "no ledger group");

        // An exact resubmission returns the same response and changes nothing.
        assert_eq!(api.send_swap_transaction(default, Some(options)).await.unwrap(), response);
        assert_eq!(pool.pool_size().total, 1);
    }

    #[tokio::test]
    async fn default_alone_still_requires_principal_and_bounds() {
        let fixture = fixture();
        let other = runtime(&[Address::repeat_byte(0x99)]);
        let runtime = runtime(&[fixture.address()]);

        let api = rpc(provider(), pool(), &runtime, MAX_EXPIRY_SECS);
        let (default, options) = single(&fixture, 0);
        let unregistered = rpc(provider(), pool(), &other, MAX_EXPIRY_SECS);
        let error =
            unregistered.send_swap_transaction(default, Some(options.clone())).await.unwrap_err();
        assert_eq!(label(&error), "principal_not_allowed");

        let error =
            api.send_swap_transaction(fixture.legacy_sibling(0), Some(options)).await.unwrap_err();
        assert_eq!(label(&error), "sibling_type_unsupported");
    }

    #[tokio::test]
    async fn single_admission_needs_its_own_capability() {
        let fixture = fixture();
        let policy = StaticAccessPolicy::new(PrincipalsFile {
            policy_version: 1,
            work_gas_quota: None,
            principals: vec![PrincipalEntry {
                id: PRINCIPAL,
                senders: vec![fixture.address()],
                capabilities: vec![Capability::CandidateSelection],
            }],
        });
        let runtime = Arc::new(CandidateRuntime {
            ledger: GroupLedger::default(),
            policy: Arc::new(policy),
            switch: CandidateSwitch::default(),
            limits: CandidateLimits::default(),
        });
        let api = rpc(provider(), pool(), &runtime, MAX_EXPIRY_SECS);

        let (default, options) = single(&fixture, 0);
        let error = api.send_swap_transaction(default, Some(options)).await.unwrap_err();
        assert_eq!(label(&error), "principal_not_allowed");
        let (default, options) = request(&fixture);
        api.send_swap_transaction(default, Some(options)).await.unwrap();
    }

    #[tokio::test]
    async fn alternatives_and_selection_must_come_together() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let api = rpc(provider(), pool(), &runtime, MAX_EXPIRY_SECS);

        let (default, mut options) = request(&fixture);
        options.selection = None;
        let error = api.send_swap_transaction(default, Some(options)).await.unwrap_err();
        assert_eq!(label(&error), "selection_required");

        let (default, mut options) = request(&fixture);
        options.alternatives = SwapAlternatives::default();
        let error = api.send_swap_transaction(default, Some(options)).await.unwrap_err();
        assert_eq!(label(&error), "selection_without_alternatives");
        assert_eq!(runtime.ledger.counts(), (0, 0));
    }

    #[tokio::test]
    async fn top_level_terms_must_match_the_selection() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let api = rpc(provider(), pool(), &runtime, MAX_EXPIRY_SECS);

        let (default, mut options) = request(&fixture);
        options.max_access_charge =
            Some(selection(&options).envelope.max_access_charge + U256::from(1));
        let error = api.send_swap_transaction(default, Some(options)).await.unwrap_err();
        assert_eq!(label(&error), "selection_mismatch");

        // Matching values are redundant but accepted.
        let (default, mut options) = request(&fixture);
        options.max_access_charge = Some(selection(&options).envelope.max_access_charge);
        api.send_swap_transaction(default, Some(options)).await.unwrap();
    }

    #[tokio::test]
    async fn selection_default_must_be_the_envelope_representative() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let api = rpc(provider(), pool(), &runtime, MAX_EXPIRY_SECS);
        let (default, mut options) = request(&fixture);
        // Swap the default with an alternative.
        let alternative = std::mem::replace(&mut options.alternatives.0[0], default);
        let error = api.send_swap_transaction(alternative, Some(options)).await.unwrap_err();
        assert_eq!(label(&error), "default_mismatch");
    }

    #[tokio::test]
    async fn sibling_of_a_group_cannot_stand_alone() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let api = rpc(provider(), pool(), &runtime, MAX_EXPIRY_SECS);
        let (default, options) = request(&fixture);
        let alternative = options.alternatives.0[0].clone();
        api.send_swap_transaction(default, Some(options)).await.unwrap();

        let (_, single) = single(&fixture, 0);
        let error = api.send_swap_transaction(alternative, Some(single)).await.unwrap_err();
        assert_eq!(label(&error), "representative_conflict");
    }

    #[tokio::test]
    async fn unregistered_principal_is_rejected() {
        let fixture = fixture();
        let runtime = runtime(&[Address::repeat_byte(0x99)]);
        let api = rpc(provider(), pool(), &runtime, MAX_EXPIRY_SECS);
        let (default, options) = request(&fixture);

        let error = api.send_swap_transaction(default, Some(options)).await.unwrap_err();
        assert_eq!(label(&error), "principal_not_allowed");
        assert_eq!(runtime.ledger.counts(), (0, 0));
    }

    #[tokio::test]
    async fn validation_failures_carry_stable_labels() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let api = rpc(provider(), pool(), &runtime, MAX_EXPIRY_SECS);

        let (default, mut options) = request(&fixture);
        options.alternatives.0.truncate(1);
        let error = api.send_swap_transaction(default, Some(options)).await.unwrap_err();
        assert_eq!(label(&error), "envelope_hash_mismatch");

        let (default, mut options) = request(&fixture);
        options.selection.as_mut().unwrap().envelope.rule.min_delta = U256::from(1);
        let error = api.send_swap_transaction(default, Some(options)).await.unwrap_err();
        assert_eq!(label(&error), "envelope_signer_not_sender");
    }

    #[tokio::test]
    async fn group_expiry_is_the_configured_lifetime_rounded_up_to_whole_blocks() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let pool = pool();
        // On the genesis head at Denim's 200 ms blocks, 7 s is 35 blocks past block 1.
        let api = rpc(provider(), pool.clone(), &runtime, 7);
        let (default, options) = request(&fixture);

        let response = api.send_swap_transaction(default, Some(options)).await.unwrap();

        let pooled = pool.get(&response.transaction_hash).expect("representative pooled");
        let group = BasePooledTx::candidate_group(&pooled.transaction).expect("group sidecar");
        assert_eq!(group.expiry_block(), 36);
        let entry = runtime.ledger.lookup(&response.group_hash.unwrap()).expect("ledger entry");
        assert_eq!(entry.descriptor.expiry_block, 36);
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
        let (default, options) = request(&fixture);

        let error = api.send_swap_transaction(default, Some(options)).await.unwrap_err();
        assert_eq!(label(&error), "sender_delegated");

        // A default transaction alone gets the same check.
        let (default, options) = single(&fixture, 0);
        let error = api.send_swap_transaction(default, Some(options)).await.unwrap_err();
        assert_eq!(label(&error), "sender_delegated");
        assert_eq!(api.admission.pool().pool_size().total, 0);
    }

    #[tokio::test]
    async fn disabled_selection_ignores_a_selection_signed_by_another_key() {
        let capture = TransactionEventCapture::install();
        let fixture = CandidateFixture::with_key(chain_id(), 0x78);
        let runtime = runtime(&[fixture.address()]);
        runtime.switch.set(false);
        let pool = pool();
        let api = rpc(provider(), pool.clone(), &runtime, MAX_EXPIRY_SECS);
        let (default, mut options) = request(&fixture);
        let other_key = CandidateFixture::with_key(chain_id(), 0x79);
        let selection = options.selection.as_mut().unwrap();
        selection.signature = other_key.sign(&selection.envelope);

        let response = api.send_swap_transaction(default, Some(options)).await.unwrap();
        assert!(response.alternatives_ignored);
        assert_eq!(response.group_hash, None);
        assert!(pool.contains(&response.transaction_hash));
        assert_eq!(runtime.ledger.counts(), (0, 0));

        let instance = serde_json::json!(format!("{:016x}", runtime.ledger.instance()));
        let admissions: Vec<_> = capture
            .events()
            .into_iter()
            .filter(|event| event.data.get("candidate_instance") == Some(&instance))
            .collect();
        assert_eq!(admissions.len(), 1, "{admissions:?}");
        assert_eq!(admissions[0].event_type, TransactionEventType::TxpoolSendSwapTransaction);
        assert_eq!(admissions[0].data["selection"], "ignored");
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
        let (default, options) = request(&fixture);
        let sibling_hash = selection(&options).envelope.sibling_hashes[0];

        api.send_swap_transaction(default, Some(options)).await.unwrap_err();

        assert_eq!(runtime.ledger.counts(), (0, 0));
        assert!(!runtime.ledger.suppresses_plain_reinsertion(&sibling_hash));
    }

    #[tokio::test]
    async fn proxy_forwards_unchanged_without_local_admission() {
        let fixture = fixture();
        // No principal and an empty pool: the proxy must not evaluate or insert anything.
        let runtime = runtime(&[]);
        let (default, options) = request(&fixture);
        let expected = SwapTransactionResponse {
            transaction_hash: TxHash::repeat_byte(0x22),
            group_hash: Some(B256::repeat_byte(0x11)),
            alternative_hashes: vec![TxHash::repeat_byte(0x33)],
            alternatives_ignored: false,
        };
        let sequencer = MockServer::start();
        let mock = sequencer.mock(|when, then| {
            when.method(POST).path("/").json_body(serde_json::json!({
                "jsonrpc": "2.0", "id": 0, "method": "base_sendSwapTransaction",
                "params": [&default, &options],
            }));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({"jsonrpc": "2.0", "id": 0, "result": &expected}));
        });
        let client = SequencerClient::new_http_with_headers(sequencer.base_url(), vec![]).unwrap();
        let api = rpc(provider(), pool(), &runtime, MAX_EXPIRY_SECS).with_sequencer_client(client);

        let response = api.send_swap_transaction(default, Some(options)).await.unwrap();

        assert_eq!(response, expected);
        assert_eq!(runtime.ledger.counts(), (0, 0));
        mock.assert();
    }

    #[test]
    fn request_uses_positional_params_and_rejects_unknown_option_fields() {
        let fixture = fixture();
        let (default, options) = request(&fixture);
        let params = serde_json::to_value((&default, &options)).unwrap();
        let (decoded_default, decoded_options): (Bytes, Option<SwapTransactionOptions>) =
            serde_json::from_value(params.clone()).unwrap();
        assert_eq!(decoded_default, default);
        assert_eq!(decoded_options, Some(options));
        assert!(params[1]["selection"].get("signature").is_some());
        assert!(params[1].get("maxAccessCharge").is_none(), "unset terms are omitted");

        let mut padded = params[1].clone();
        padded["extra"] = serde_json::json!(true);
        assert!(serde_json::from_value::<SwapTransactionOptions>(padded).is_err());

        let single = serde_json::json!({ "maxAccessCharge": "0x0" });
        let single: SwapTransactionOptions = serde_json::from_value(single).unwrap();
        assert_eq!(single.max_access_charge, Some(U256::ZERO));
        // The node assigns lifetimes; a client expiry is an unknown field, top level or signed.
        let expiring = serde_json::json!({ "expiryBlock": "0x64" });
        assert!(serde_json::from_value::<SwapTransactionOptions>(expiring).is_err());
        let mut signed = params[1].clone();
        signed["selection"]["envelope"]["expiryBlock"] = serde_json::json!("0x64");
        assert!(serde_json::from_value::<SwapTransactionOptions>(signed).is_err());
    }

    #[test]
    fn wire_shapes_are_stable() {
        let options = serde_json::json!({
            "alternatives": ["0x01"],
            "selection": {
                "envelope": {
                    "sender": Address::repeat_byte(1),
                    "nonce": "0x0",
                    "siblingHashes": [TxHash::repeat_byte(2), TxHash::repeat_byte(3)],
                    "rule": {
                        "kind": "0x0",
                        "scoreVersion": "0x1",
                        "token": Address::repeat_byte(4),
                        "recipient": Address::repeat_byte(5),
                        "minDelta": "0x1"
                    },
                    "fallbackMode": "0x0",
                    "defaultIndex": "0x0",
                    "maxWorkGas": "0xf4240",
                    "maxEvaluations": "0x4",
                    "maxAccessCharge": "0x0",
                    "disclosure": "0x0"
                },
                "signature": {
                    "r": "0x1", "s": "0x2", "yParity": "0x0", "v": "0x0"
                }
            },
            "maxAccessCharge": "0x0"
        });
        let decoded: SwapTransactionOptions = serde_json::from_value(options.clone()).unwrap();
        assert_eq!(serde_json::to_value(&decoded).unwrap(), options);

        let response = SwapTransactionResponse {
            transaction_hash: TxHash::repeat_byte(2),
            group_hash: Some(B256::repeat_byte(9)),
            alternative_hashes: vec![TxHash::repeat_byte(3)],
            alternatives_ignored: false,
        };
        assert_eq!(
            serde_json::to_value(&response).unwrap(),
            serde_json::json!({
                "transactionHash": TxHash::repeat_byte(2),
                "groupHash": B256::repeat_byte(9),
                "alternativeHashes": [TxHash::repeat_byte(3)]
            })
        );

        let error =
            SendSwapTransactionApiImpl::<(), ()>::reject(&CandidateError::SelectionFieldMismatch {
                field: "maxAccessCharge",
            });
        assert_eq!(
            serde_json::to_value(&error).unwrap(),
            serde_json::json!({
                "code": -32602,
                "message": error.message(),
                "data": "selection_mismatch"
            })
        );
    }

    #[test]
    fn alternative_list_deserialization_is_bounded() {
        let at_ceiling = vec!["0x00"; base_execution_candidates::MAX_SIBLINGS_CEILING];
        assert!(serde_json::from_value::<SwapAlternatives>(serde_json::json!(at_ceiling)).is_ok());
        let past_ceiling = vec!["0x00"; base_execution_candidates::MAX_SIBLINGS_CEILING + 1];
        assert!(
            serde_json::from_value::<SwapAlternatives>(serde_json::json!(past_ceiling)).is_err()
        );
    }

    #[test]
    fn response_omits_selection_fields_that_do_not_apply() {
        let response = SwapTransactionResponse {
            transaction_hash: TxHash::repeat_byte(0x22),
            group_hash: None,
            alternative_hashes: vec![],
            alternatives_ignored: false,
        };
        assert_eq!(
            serde_json::to_value(&response).unwrap(),
            serde_json::json!({ "transactionHash": TxHash::repeat_byte(0x22) })
        );
        let ignored = SwapTransactionResponse { alternatives_ignored: true, ..response };
        assert_eq!(serde_json::to_value(&ignored).unwrap()["alternativesIgnored"], true);
    }

    #[test]
    fn methods_register_under_their_namespaces() {
        let runtime = runtime(&[]);
        let api = rpc(provider(), pool(), &runtime, MAX_EXPIRY_SECS).into_rpc();
        assert!(api.method_names().any(|name| name == "base_sendSwapTransaction"));
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

        let (default, options) = request(&fixture);
        let response = api.send_swap_transaction(default, Some(options)).await.unwrap();
        let (default, options) = request(&fixture);
        api.send_swap_transaction(default, Some(options)).await.unwrap();

        let events = group_events(&capture, response.group_hash.unwrap());
        assert_eq!(events.len(), 1, "a duplicate journals no second admission: {events:?}");
        assert_eq!(
            (events[0].0, events[0].1),
            (TransactionEventType::TxpoolSendSwapTransaction, Some(response.transaction_hash))
        );
        assert_eq!(events[0].2["selection"], "applied");
        assert_eq!(
            events[0].2["alternative_hashes"],
            serde_json::json!(response.alternative_hashes)
        );
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
        let submit = |raw: Vec<Bytes>, envelope, signature| {
            let api = api.clone();
            let (default, options) = swap(&raw, envelope, signature);
            tokio::spawn(async move { api.send_swap_transaction(default, Some(options)).await })
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
        let api = gated_rpc(
            GatedValidator { gate: Arc::clone(&gate), accept: true },
            &runtime,
            DEFAULT_ADMISSION_TIMEOUT,
        );
        let (raw, envelope, signature) = fixture.submission(3, 1);
        let group = fixture.group(&raw, envelope.clone());
        let (default, options) = swap(&raw, envelope, signature);

        let first = tokio::spawn({
            let api = api.clone();
            async move { api.send_swap_transaction(default, Some(options)).await }
        });
        until_pending(&runtime).await;
        // A canonical block consumes the nonce while the insertion is in flight, and the pool
        // still accepts the outcome validated before it.
        runtime.ledger.record_nonces([(group.sender(), group.nonce() + 1)]);
        gate.add_permits(1);

        let error = first.await.unwrap().expect_err("the group ended at settlement");
        assert_eq!(label(&error), "group_terminal");
        assert!(
            !api.admission.pool().contains(&group.representative().hash()),
            "stale representative removed"
        );
        assert_eq!(runtime.ledger.counts(), (0, 1), "the sender's slot is free");
        assert!(api.admission.lock_inflight().is_empty());
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
            let (default, options) = request(&fixture);
            group_hash = selection(&options).envelope.signing_hash(chain_id());
            api.send_swap_transaction(default, Some(options)).await.unwrap_err();
        }
        // A request that fails before validation has no hash, yet keeps its own identity.
        for _ in 0..2 {
            let (_, mut options) = request(&fixture);
            options.alternatives = SwapAlternatives::default();
            api.send_swap_transaction(Bytes::new(), Some(options)).await.unwrap_err();
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
        let trace = SwapAdmissionTrace::default();
        assert_eq!(trace.rejection_label(&error), "internal_error");
        let trace = SwapAdmissionTrace { pool_rejected: true, ..trace };
        assert_eq!(trace.rejection_label(&error), "pool_rejected");
        let labeled =
            SendSwapTransactionApiImpl::<(), ()>::reject(&CandidateError::PrincipalNotAllowed);
        assert_eq!(trace.rejection_label(&labeled), "principal_not_allowed");
    }

    #[tokio::test]
    async fn defaults_admitted_alone_journal_whether_alternatives_were_ignored() {
        let capture = TransactionEventCapture::install();
        let fixture = CandidateFixture::with_key(chain_id(), 0x77);
        let runtime = runtime(&[fixture.address()]);
        runtime.switch.set(false);
        let api = rpc(provider(), pool(), &runtime, MAX_EXPIRY_SECS);
        let (default, options) = request(&fixture);
        let ignored = api.send_swap_transaction(default, Some(options)).await.unwrap();
        let (default, options) = single(&fixture, 1);
        let alone = api.send_swap_transaction(default, Some(options)).await.unwrap();

        let instance = serde_json::json!(format!("{:016x}", runtime.ledger.instance()));
        let admissions: Vec<_> = capture
            .events()
            .into_iter()
            .filter(|event| event.data.get("candidate_instance") == Some(&instance))
            .map(|event| (event.event_type, event.tx_hash, event.data))
            .collect();
        assert_eq!(admissions.len(), 2, "{admissions:?}");
        let admitted = TransactionEventType::TxpoolSendSwapTransaction;
        assert_eq!((admissions[0].0, admissions[0].1), (admitted, Some(ignored.transaction_hash)));
        assert_eq!(admissions[0].2["selection"], "ignored");
        assert_eq!(admissions[0].2["alternatives"], 2);
        assert!(admissions[0].2.get("expiry_block").is_none());
        assert!(admissions[0].2.get("group_hash").is_none());
        assert_eq!((admissions[1].0, admissions[1].1), (admitted, Some(alone.transaction_hash)));
        assert_eq!(admissions[1].2["selection"], "none");
    }

    #[tokio::test]
    async fn rejected_groups_journal_their_reason_under_the_representative() {
        let capture = TransactionEventCapture::install();
        let rejected = TransactionEventType::TxpoolSendSwapTransactionRejected;

        let fixture = CandidateFixture::with_key(chain_id(), 0x72);
        let unregistered = runtime(&[Address::repeat_byte(0x99)]);
        let api = rpc(provider(), pool(), &unregistered, MAX_EXPIRY_SECS);
        let (default, options) = request(&fixture);
        let group_hash = selection(&options).envelope.signing_hash(chain_id());
        let representative = selection(&options).envelope.sibling_hashes[1];
        api.send_swap_transaction(default, Some(options)).await.unwrap_err();
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
        let (default, options) = request(&fixture);
        let group_hash = selection(&options).envelope.signing_hash(chain_id());
        api.send_swap_transaction(default, Some(options)).await.unwrap_err();
        let events = group_events(&capture, group_hash);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].0, rejected);
        assert_eq!(events[0].2["rejection_reason"], "pool_rejected");
    }
}
