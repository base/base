//! `base_sendSwapTransaction`, the Swap Lane submission endpoint, and its runtime kill switch.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use alloy_consensus::BlockHeader;
use alloy_primitives::{Address, B256, Bytes, Signature, TxHash, U256};
use base_common_chains::Upgrades;
use base_execution_candidates::{
    CandidateEnvelope, CandidateError, CandidateGroup, CandidateRuntime, CandidateValidator,
    ExpiryWindow, GroupLedger, GroupState, GroupSummary, LedgerAdmission,
};
use base_execution_rpc::SequencerClient;
use base_execution_txpool::{
    BasePooledTransaction, ValidityOperator, ValidityPredicate, ValiditySignatureMetrics,
};
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
    /// Last block in which the transaction may land (inclusive). Required without a
    /// `selection`; with one it defaults to the envelope's `expiryBlock` and must equal it, even
    /// while selection is disabled and the selection is ignored.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub expiry_block: Option<u64>,
    /// Upper bound, in wei, on the Swap Lane access charge; zero when omitted. With a
    /// `selection` it defaults to the envelope's `maxAccessCharge` and must equal it, even while
    /// selection is disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_access_charge: Option<U256>,
}

/// `expiryBlock` and `maxAccessCharge` resolved for one submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapTerms {
    /// Last block in which the transaction may land (inclusive).
    pub expiry_block: u64,
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
            return Ok(SwapTerms {
                expiry_block: self.expiry_block.ok_or(CandidateError::ExpiryRequired)?,
                max_access_charge: self.max_access_charge.unwrap_or_default(),
            });
        };
        let envelope = &selection.envelope;
        if self.expiry_block.is_some_and(|expiry| expiry != envelope.expiry_block) {
            return Err(CandidateError::SelectionFieldMismatch { field: "expiryBlock" });
        }
        if self.max_access_charge.is_some_and(|charge| charge != envelope.max_access_charge) {
            return Err(CandidateError::SelectionFieldMismatch { field: "maxAccessCharge" });
        }
        Ok(SwapTerms {
            expiry_block: envelope.expiry_block,
            max_access_charge: envelope.max_access_charge,
        })
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
    /// Alternative hashes, in envelope order. Present only when selection applied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternative_hashes: Vec<TxHash>,
    /// `expiryBlock` in effect for a default transaction pooled alone. Present only when selection
    /// did not apply. Admission is idempotent by transaction hash, so an exact resubmission
    /// reports the inclusive last block of the transaction already pooled, whatever
    /// `expiryBlock` it carried, and omits it if that transaction has no block bound.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub expiry_block: Option<u64>,
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
            expiry_block: None,
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
    /// not verified, but `expiryBlock` and `maxAccessCharge` given alongside a selection must
    /// still match its envelope (`selection_mismatch`).
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

/// How long a first admission may spend in pool insertion, and how long an exact duplicate of a
/// group waits for it. Pool validation normally takes milliseconds.
pub const DEFAULT_CANDIDATE_ADMISSION_TIMEOUT: Duration = Duration::from_secs(2);

/// Pool insertions in flight, by `groupHash`. The value becomes `Some(inserted)` when the
/// first request finishes; exact duplicates wait on it instead of reporting acceptance early.
pub type InflightAdmissions = Arc<Mutex<HashMap<B256, watch::Receiver<Option<bool>>>>>;

/// Local ingress for swap transactions.
#[derive(Debug, Clone)]
pub struct SendSwapTransactionApiImpl<Provider, Pool> {
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

impl<Provider, Pool> SendSwapTransactionApiImpl<Provider, Pool> {
    /// Creates local swap ingress over the shared candidate runtime.
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
        response: SwapTransactionResponse,
    ) -> RpcResult<SwapTransactionResponse> {
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

impl<Provider, Pool> SendSwapTransactionApiImpl<Provider, Pool>
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

    /// Rejects EIP-7702-delegated senders of swap transactions (design §4.2), with or without
    /// alternatives.
    pub fn ensure_sender_not_delegated(&self, sender: Address) -> RpcResult<()> {
        let state = self
            .provider
            .latest()
            .map_err(|error| Self::internal(format!("failed to open latest state: {error}")))?;
        let code = state
            .account_code(&sender)
            .map_err(|error| Self::internal(format!("failed to read sender code: {error}")))?;
        if code.is_some_and(|code| code.is_eip7702()) {
            return Err(Self::reject(&CandidateError::SenderDelegated));
        }
        Ok(())
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
        trace.journal(&self.runtime.ledger, &result);
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
            Some(selection) if self.runtime.switch.is_enabled() => {
                self.submit_group(transaction, options.alternatives, selection, trace).await
            }
            selection => {
                trace.alternatives_ignored = selection.is_some();
                self.submit_single(transaction, terms, trace).await
            }
        }
    }

    /// Admits a default transaction alone (design §5.1 without selection).
    ///
    /// It is pooled with Private origin, so it is never propagated, under a `block_number <=
    /// expiryBlock` validity predicate: the pool evicts it and the builder skips it after
    /// `expiryBlock`. No ledger group is created. Its sender must still be an allowlisted
    /// principal that is not EIP-7702-delegated; v0 prices no work for it, so `maxAccessCharge`
    /// is recorded but not charged.
    ///
    /// The synthesized predicate is unsigned: the sender did not sign it, ingress derives it
    /// from `expiryBlock`, so it is exempt from `ValiditySignatureMode`. A node whose builder
    /// ingress requires signed validity sidecars would refuse it if it were forwarded there.
    ///
    /// Admission is idempotent by transaction hash: an exact resubmission returns success with
    /// the pooled transaction's expiry and changes nothing.
    pub async fn submit_single(
        &self,
        transaction: Bytes,
        terms: SwapTerms,
        trace: &mut SwapAdmissionTrace,
    ) -> RpcResult<SwapTransactionResponse> {
        let window = self.expiry_window()?;
        let validator = CandidateValidator { limits: self.runtime.limits, chain_id: self.chain_id };
        let default = validator.validate_default(&transaction).map_err(|error| {
            debug!(error = %error, label = error.as_label(), "rejected swap transaction");
            Self::reject(&error)
        })?;
        let hash = default.hash();
        trace.transaction_hash = Some(hash);
        trace.expiry_block = Some(terms.expiry_block);
        if window.is_some_and(|window| !window.contains(terms.expiry_block)) {
            return Err(Self::reject(&CandidateError::ExpiryOutOfWindow));
        }
        self.ensure_sender_not_delegated(default.transaction.signer())?;
        // Allowlist membership only: pricing and quotas are per-group until a Swap Lane admission
        // policy exists.
        let principal = self
            .runtime
            .policy
            .principal(default.transaction.signer())
            .ok_or_else(|| Self::reject(&CandidateError::PrincipalNotAllowed))?;
        // A sibling the ledger tracks belongs to a group; it cannot also stand alone.
        if self.runtime.ledger.suppresses_plain_reinsertion(&hash) {
            return Err(Self::reject(&CandidateError::RepresentativeConflict));
        }
        let mut response = SwapTransactionResponse {
            transaction_hash: hash,
            group_hash: None,
            alternative_hashes: Vec::new(),
            expiry_block: Some(terms.expiry_block),
            alternatives_ignored: trace.alternatives_ignored,
        };
        if let Some(pooled) = self.pool.get(&hash) {
            trace.duplicate = true;
            response.expiry_block =
                ValidityPredicate::block_expiry_bound(pooled.transaction.validity_predicates());
            return Ok(response);
        }

        let expiry = ValidityPredicate::BlockNumber {
            op: ValidityOperator::LessThanOrEqual,
            value: U256::from(terms.expiry_block),
        };
        let pooled = BasePooledTransaction::new(default.transaction, default.encoded.len())
            .with_validity_predicates(vec![expiry]);
        let inserted = tokio::time::timeout(
            self.admission_timeout,
            self.pool.add_transaction(TransactionOrigin::Private, pooled),
        )
        .await;
        match inserted {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                trace.pool_rejected = true;
                return Err(ErrorObjectOwned::from(EthApiError::from(error)));
            }
            Err(_) if self.pool.contains(&hash) => {}
            Err(_) => {
                debug!(tx_hash = %hash, "swap transaction admission timed out");
                return Err(Self::reject(&CandidateError::AdmissionTimedOut));
            }
        }
        ValiditySignatureMetrics::accepted("ingress", "unsigned").increment(1);
        info!(
            tx_hash = %hash,
            expiry_block = terms.expiry_block,
            max_access_charge = %terms.max_access_charge,
            alternatives_ignored = trace.alternatives_ignored,
            principal = principal.0,
            "accepted swap transaction"
        );
        Ok(response)
    }

    /// Admits a default transaction and its alternatives as a candidate group (design §5.1).
    pub async fn submit_group(
        &self,
        transaction: Bytes,
        alternatives: SwapAlternatives,
        selection: SwapSelection,
        trace: &mut SwapAdmissionTrace,
    ) -> RpcResult<SwapTransactionResponse> {
        // Steps 3-5: bounds, decode, uniformity, envelope, expiry and signature.
        let window = self.expiry_window()?;
        let validator = CandidateValidator { limits: self.runtime.limits, chain_id: self.chain_id };
        let group = validator
            .validate_selection(
                &transaction,
                &alternatives.0,
                selection.envelope,
                selection.signature,
                window,
            )
            .map_err(|error| {
                debug!(error = %error, label = error.as_label(), "rejected candidate group");
                Self::reject(&error)
            })?;
        trace.group_hash = Some(group.group_hash());
        trace.transaction_hash = Some(group.representative().hash());
        self.ensure_sender_not_delegated(group.sender())?;
        let group = Arc::new(group);
        let response = SwapTransactionResponse::from(group.as_ref());
        let group_hash = group.group_hash();

        // Step 6: an exact duplicate returns the original response before any other check,
        // but only once the group is actually pooled.
        let in_flight = {
            let inflight = self.lock_inflight();
            match self.runtime.ledger.classify(&group_hash, |hash| self.pool.contains(hash)) {
                None => None,
                Some(state) if state.is_terminal() => {
                    return Err(Self::reject(&CandidateError::GroupTerminal));
                }
                Some(GroupState::Pending) => Some(inflight.get(&group_hash).cloned()),
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
            group_hash,
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
                LedgerAdmission::Pending => Err(inflight.get(&group_hash).cloned()),
                LedgerAdmission::Created => {
                    let (sender, receiver) = watch::channel(None);
                    inflight.insert(group_hash, receiver);
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
            group_hash,
            representative: response.transaction_hash,
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
                let pooled = self.pool.contains(&response.transaction_hash);
                let accepted = admission.settle(pooled);
                if !pooled {
                    debug!(group_hash = %group_hash, "candidate admission timed out");
                    return Err(Self::reject(&CandidateError::AdmissionTimedOut));
                }
                if !accepted {
                    return Err(Self::reject(&CandidateError::GroupTerminal));
                }
            }
        }
        info!(
            group_hash = %group_hash,
            representative = %response.transaction_hash,
            alternatives = response.alternative_hashes.len(),
            principal = principal.0,
            "accepted candidate group"
        );
        Ok(response)
    }
}

/// Join keys and outcome of one local `base_sendSwapTransaction` request, journaled when it
/// returns. A request whose future is dropped journals nothing; [`PendingAdmission`] still settles
/// its ledger entry.
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
    /// `expiryBlock` of a default transaction admitted alone, once resolved.
    pub expiry_block: Option<u64>,
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
    /// An admission is keyed by the default transaction's hash (and a group's hash), so a
    /// replayed event deduplicates, and duplicate submissions journal nothing. Each rejection
    /// gets a never-reused identity from `ledger`: a request may be rejected repeatedly, and a
    /// malformed one has no hash at all.
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
                    "expiry_block" => self.expiry_block,
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
    use alloy_primitives::{Address, U256, keccak256};
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
    ) -> SendSwapTransactionApiImpl<TestProvider, P> {
        SendSwapTransactionApiImpl::new(
            provider,
            pool,
            Arc::clone(runtime),
            chain_id(),
            max_expiry_secs,
        )
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

    /// A default transaction alone, expiring at block 100.
    fn single(fixture: &CandidateFixture, nonce: u64) -> (Bytes, SwapTransactionOptions) {
        let default =
            fixture.encode(fixture.sibling_tx(nonce, Address::repeat_byte(1), U256::ZERO));
        (default, SwapTransactionOptions { expiry_block: Some(100), ..Default::default() })
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
        let (default, options) = request(&fixture);

        let first = tokio::spawn({
            let api = api.clone();
            async move { api.send_swap_transaction(default, Some(options)).await }
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
        assert!(api.lock_inflight().is_empty());
        assert_eq!(runtime.ledger.counts(), (0, 0));
    }

    #[tokio::test]
    async fn failed_first_admission_fails_its_waiting_duplicates() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let api = gated_rpc(GatedValidator { gate: Arc::clone(&gate), accept: false }, &runtime);
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
        assert!(api.lock_inflight().is_empty());
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
            .validate_selection(&default, &options.alternatives.0, envelope, signature, None)
            .unwrap();
        // A first request created the entry; its insertion has not settled.
        let (sender, receiver) = watch::channel(None);
        api.lock_inflight().insert(group.group_hash(), receiver);
        runtime.ledger.create(Arc::new(group), PRINCIPAL, 1, |_| false).unwrap();

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
                None,
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
                expiry_block: Some(selection(&options).envelope.expiry_block),
                alternatives_ignored: true,
            }
        );
        let pooled = pool.get(&default_hash).expect("default pooled");
        assert!(BasePooledTx::candidate_group(&pooled.transaction).is_none());
        assert_eq!(runtime.ledger.counts(), (0, 0));
        assert_eq!(pool.pool_size().total, 1, "alternatives are not pooled");
    }

    #[tokio::test]
    async fn default_alone_is_pooled_privately_until_its_expiry() {
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
        assert_eq!(
            pooled.transaction.validity_predicates(),
            [ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(100),
            }]
        );
        assert_eq!(runtime.ledger.counts(), (0, 0), "no ledger group");

        // An exact resubmission returns the same response, and reports the pooled expiry even
        // when it asks for another one.
        assert_eq!(response.expiry_block, Some(100));
        assert_eq!(
            api.send_swap_transaction(default.clone(), Some(options)).await.unwrap(),
            response
        );
        let later = SwapTransactionOptions { expiry_block: Some(101), ..Default::default() };
        assert_eq!(api.send_swap_transaction(default, Some(later)).await.unwrap(), response);
        assert_eq!(pool.pool_size().total, 1);
    }

    #[tokio::test]
    async fn default_alone_still_requires_principal_expiry_and_bounds() {
        let fixture = fixture();
        let other = runtime(&[Address::repeat_byte(0x99)]);
        let runtime = runtime(&[fixture.address()]);
        let api = rpc(provider(), pool(), &runtime, 1);

        let (default, mut options) = single(&fixture, 0);
        options.expiry_block = None;
        let error = api.send_swap_transaction(default.clone(), Some(options)).await.unwrap_err();
        assert_eq!(label(&error), "expiry_required");
        let error = api.send_swap_transaction(default.clone(), None).await.unwrap_err();
        assert_eq!(label(&error), "expiry_required");

        // One second is at most ten native blocks; the request expires at block 100.
        let (_, options) = single(&fixture, 0);
        let error = api.send_swap_transaction(default, Some(options)).await.unwrap_err();
        assert_eq!(label(&error), "expiry_out_of_window");

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
        options.expiry_block = Some(selection(&options).envelope.expiry_block + 1);
        let error = api.send_swap_transaction(default, Some(options)).await.unwrap_err();
        assert_eq!(label(&error), "selection_mismatch");

        let (default, mut options) = request(&fixture);
        options.max_access_charge =
            Some(selection(&options).envelope.max_access_charge + U256::from(1));
        let error = api.send_swap_transaction(default, Some(options)).await.unwrap_err();
        assert_eq!(label(&error), "selection_mismatch");

        // Matching values are redundant but accepted.
        let (default, mut options) = request(&fixture);
        options.expiry_block = Some(selection(&options).envelope.expiry_block);
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
    async fn expiry_past_the_configured_window_is_rejected() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        // One second is at most ten native blocks; the fixture expires at block 100.
        let api = rpc(provider(), pool(), &runtime, 1);
        let (default, options) = request(&fixture);

        let error = api.send_swap_transaction(default, Some(options)).await.unwrap_err();
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
        let (default, options) = request(&fixture);

        let error = api.send_swap_transaction(default, Some(options)).await.unwrap_err();
        assert_eq!(label(&error), "sender_delegated");

        // A default transaction alone gets the same check.
        let (default, options) = single(&fixture, 0);
        let error = api.send_swap_transaction(default, Some(options)).await.unwrap_err();
        assert_eq!(label(&error), "sender_delegated");
        assert_eq!(api.pool.pool_size().total, 0);
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
            expiry_block: None,
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
        assert!(params[1].get("expiryBlock").is_none(), "unset terms are omitted");

        let mut padded = params[1].clone();
        padded["extra"] = serde_json::json!(true);
        assert!(serde_json::from_value::<SwapTransactionOptions>(padded).is_err());

        let single = serde_json::json!({ "expiryBlock": "0x64", "maxAccessCharge": "0x0" });
        let single: SwapTransactionOptions = serde_json::from_value(single).unwrap();
        assert_eq!((single.expiry_block, single.max_access_charge), (Some(100), Some(U256::ZERO)));
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
            expiry_block: None,
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
        let api = gated_rpc(GatedValidator { gate: Arc::clone(&gate), accept: true }, &runtime);
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
        assert_eq!(admissions[0].2["expiry_block"], 100);
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

    #[tokio::test]
    async fn resubmission_reports_the_tightest_pooled_block_bound() {
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let validator = CandidateValidator { limits: runtime.limits, chain_id: chain_id() };
        let block =
            |op, value: u64| ValidityPredicate::BlockNumber { op, value: U256::from(value) };
        let cases = [
            (0, vec![block(ValidityOperator::LessThan, 101)], 100),
            (
                1,
                vec![
                    block(ValidityOperator::LessThanOrEqual, 100),
                    block(ValidityOperator::LessThanOrEqual, 50),
                ],
                50,
            ),
        ];
        for (nonce, predicates, expected) in cases {
            // The validity ingress on the same node pooled it first.
            let pool = pool();
            let (default, options) = single(&fixture, nonce);
            let pooled = validator.validate_default(&default).unwrap();
            let pooled = BasePooledTransaction::new(pooled.transaction, pooled.encoded.len())
                .with_validity_predicates(predicates);
            pool.add_transaction(TransactionOrigin::Private, pooled).await.unwrap();

            let api = rpc(provider(), pool, &runtime, MAX_EXPIRY_SECS);
            let response = api.send_swap_transaction(default, Some(options)).await.unwrap();
            assert_eq!(response.expiry_block, Some(expected));
        }
    }

    #[tokio::test]
    async fn fresh_single_admission_counts_one_unsigned_validity_sidecar() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let fixture = fixture();
        let runtime = runtime(&[fixture.address()]);
        let api = rpc(provider(), pool(), &runtime, MAX_EXPIRY_SECS);
        let (default, options) = single(&fixture, 0);

        api.send_swap_transaction(default.clone(), Some(options.clone())).await.unwrap();
        api.send_swap_transaction(default, Some(options)).await.unwrap();

        let unsigned: Vec<_> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(key, _, _, _)| {
                let key = key.key();
                key.name() == "txpool.validity_signature.accepted"
                    && key.labels().any(|l| l.key() == "site" && l.value() == "ingress")
                    && key.labels().any(|l| l.key() == "signature" && l.value() == "unsigned")
            })
            .map(|(_, _, _, value)| value)
            .collect();
        assert_eq!(
            unsigned,
            [metrics_util::debugging::DebugValue::Counter(1)],
            "the resubmission is not counted again"
        );
    }
}
