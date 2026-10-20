//! Process-shared admission of Swap Lane submissions into the pool.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use alloy_primitives::{Address, B256, TxHash};
use base_execution_candidates::{
    AccessDenied, BlockWindow, CandidateGroup, CandidateRuntime, CandidateSibling, Capability,
    GroupState, GroupSummary, LedgerAdmission, LedgerError, PrincipalId,
};
use reth_storage_api::{StateProviderFactory, errors::ProviderError};
use reth_transaction_pool::{TransactionOrigin, TransactionPool, error::PoolError};
use tokio::sync::watch;
use tracing::debug;

use crate::BasePooledTransaction;

/// How long a first admission may spend in pool insertion, and how long an exact duplicate of a
/// group waits for it. Pool validation normally takes milliseconds.
pub const DEFAULT_ADMISSION_TIMEOUT: Duration = Duration::from_secs(2);

/// Pool insertions in flight, by `groupHash`. The value becomes `Some(inserted)` when the
/// first request finishes; exact duplicates wait on it instead of reporting acceptance early.
pub type InflightAdmissions = Arc<Mutex<HashMap<B256, watch::Receiver<Option<bool>>>>>;

/// What authorized an admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationBasis {
    /// The sender's principal holds the capability; no work is priced or reserved.
    Allowlist,
    /// The access policy at `policy_version` admitted the group's declared work.
    Policy {
        /// Version of the principals file that admitted the group.
        policy_version: u64,
    },
}

/// The principal and capability an admission was authorized under, and on what basis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Authorization {
    /// Principal the sender acts for.
    pub principal: PrincipalId,
    /// Capability the admission used.
    pub capability: Capability,
    /// Why the principal was allowed.
    pub basis: AuthorizationBasis,
}

/// Outcome of an accepted admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionReceipt {
    /// Hash of the pooled transaction: the single transaction or the group's representative.
    pub transaction_hash: TxHash,
    /// The group's identity, for a candidate group.
    pub group_hash: Option<B256>,
    /// Authorization of a fresh admission; `None` for an exact duplicate, which returns the
    /// original admission's outcome before any policy check.
    pub authorization: Option<Authorization>,
}

impl AdmissionReceipt {
    /// Returns `true` when the submission matched an existing admission.
    pub const fn is_duplicate(&self) -> bool {
        self.authorization.is_none()
    }
}

/// Why an admission was refused. Ingress maps each to its stable reason label.
#[derive(Debug, thiserror::Error)]
pub enum AdmissionError {
    /// The sender is EIP-7702-delegated.
    #[error("sender is EIP-7702 delegated")]
    SenderDelegated,
    /// No principal holds the capability for the sender.
    #[error("sender is not an allowed principal")]
    PrincipalNotAllowed,
    /// The access policy refused the group.
    #[error(transparent)]
    AccessDenied(#[from] AccessDenied),
    /// The ledger refused or ended the admission.
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    /// The concurrent admission this one waited on failed.
    #[error("concurrent submission of the same group failed")]
    ConcurrentSubmissionFailed,
    /// Pool insertion, or the wait for a concurrent one, exceeded the admission timeout.
    #[error("admission timed out")]
    TimedOut,
    /// The pool refused the transaction.
    #[error(transparent)]
    Pool(#[from] PoolError),
    /// The sender's state could not be read.
    #[error("failed to read sender state: {0}")]
    State(#[from] ProviderError),
}

/// Owns a first admission's `Pending` ledger entry and in-flight receiver until it settles.
///
/// Dropping it unsettled, because the request was cancelled, settles the admission from pool
/// state: confirmed if the representative is pooled, aborted otherwise. Either way the
/// in-flight entry is removed and waiting duplicates learn the outcome, so no `Pending` entry
/// outlives its request.
#[derive(Debug)]
pub struct PendingAdmission<Pool: TransactionPool> {
    /// The in-flight map holding this admission's receiver.
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

/// Admits verified Swap Lane submissions into the pool.
///
/// One instance serves the whole process: every ingress adapter that admits candidate groups
/// must share it, so concurrent submissions of one group through different adapters see the
/// same in-flight admission. It owns the in-flight map, bounded waits, cancellation settlement
/// and removal of a representative whose nonce a canonical block consumed during insertion.
/// Adapters own wire parsing, decoding and envelope verification, and hand it verified values.
///
/// Every admission requires a non-delegated sender whose principal holds the capability used.
/// Admitted transactions are pooled with Private origin, so devp2p never gossips them. Private
/// origin does not stop transaction forwarding: a forwarding node would send an admitted
/// single on to its builders, but forwarding skips candidate groups, so a group stays on the
/// node that admitted it.
#[derive(Debug)]
pub struct AdmissionService<Provider, Pool> {
    provider: Provider,
    pool: Pool,
    runtime: Arc<CandidateRuntime>,
    group_lifetime_secs: u64,
    inflight: InflightAdmissions,
    timeout: Duration,
}

impl<Provider, Pool> AdmissionService<Provider, Pool> {
    /// Creates the service over the shared candidate runtime. `group_lifetime_secs` is how long
    /// past the next block an admitted group lives, converted to blocks rounding up.
    pub fn new(
        provider: Provider,
        pool: Pool,
        runtime: Arc<CandidateRuntime>,
        group_lifetime_secs: u64,
    ) -> Self {
        Self {
            provider,
            pool,
            runtime,
            group_lifetime_secs,
            inflight: InflightAdmissions::default(),
            timeout: DEFAULT_ADMISSION_TIMEOUT,
        }
    }

    /// Overrides [`DEFAULT_ADMISSION_TIMEOUT`].
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Returns the shared candidate runtime.
    pub const fn runtime(&self) -> &Arc<CandidateRuntime> {
        &self.runtime
    }

    /// Returns the pool admissions insert into.
    pub const fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Returns the provider sender state is read from.
    pub const fn provider(&self) -> &Provider {
        &self.provider
    }

    /// Returns the inclusive expiry block of a group admitted on a head whose next block is
    /// `next_block`, at a full-block cadence of `block_interval_millis`.
    pub const fn group_expiry_block(&self, next_block: u64, block_interval_millis: u64) -> u64 {
        BlockWindow::from_secs(next_block, self.group_lifetime_secs, block_interval_millis)
            .last_block()
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
        group: &CandidateGroup,
    ) -> Result<AdmissionReceipt, AdmissionError> {
        let Some(mut receiver) = receiver else {
            return Err(AdmissionError::ConcurrentSubmissionFailed);
        };
        match tokio::time::timeout(self.timeout, receiver.wait_for(Option::is_some)).await {
            Err(_) => Err(AdmissionError::TimedOut),
            Ok(Ok(outcome)) if *outcome == Some(true) => Ok(Self::duplicate(group)),
            Ok(_) => Err(AdmissionError::ConcurrentSubmissionFailed),
        }
    }

    /// The receipt of an exact duplicate of `group`.
    pub fn duplicate(group: &CandidateGroup) -> AdmissionReceipt {
        let descriptor = group.descriptor();
        AdmissionReceipt {
            transaction_hash: descriptor.representative,
            group_hash: Some(descriptor.group_hash),
            authorization: None,
        }
    }
}

impl<Provider, Pool> AdmissionService<Provider, Pool>
where
    Provider: StateProviderFactory,
    Pool: TransactionPool<Transaction = BasePooledTransaction> + Clone,
{
    /// Rejects EIP-7702-delegated senders (design §4.2).
    pub fn ensure_sender_not_delegated(&self, sender: Address) -> Result<(), AdmissionError> {
        let code = self.provider.latest()?.account_code(&sender)?;
        if code.is_some_and(|code| code.is_eip7702()) {
            return Err(AdmissionError::SenderDelegated);
        }
        Ok(())
    }

    /// Returns the principal holding `capability` for `sender`, on allowlist membership alone.
    pub fn allowlisted(
        &self,
        sender: Address,
        capability: Capability,
    ) -> Result<Authorization, AdmissionError> {
        let principal = self
            .runtime
            .policy
            .principal(sender, capability)
            .ok_or(AdmissionError::PrincipalNotAllowed)?;
        Ok(Authorization { principal, capability, basis: AuthorizationBasis::Allowlist })
    }

    /// Admits a default transaction alone under [`Capability::SingleAdmission`].
    ///
    /// No ledger group is created and no work is priced: the sender must only be an allowlisted,
    /// non-delegated principal. The transaction carries no validity predicate, so it lives in
    /// the pool like any other Private transaction: until it lands, is replaced, its nonce is
    /// consumed, or pool limits evict it. A transaction the ledger tracks as a group sibling
    /// cannot also stand alone.
    ///
    /// Admission is idempotent by transaction hash: an exact resubmission is a duplicate and
    /// changes nothing.
    pub async fn admit_single(
        &self,
        transaction: CandidateSibling,
    ) -> Result<AdmissionReceipt, AdmissionError> {
        let hash = transaction.hash();
        let sender = transaction.transaction.signer();
        self.ensure_sender_not_delegated(sender)?;
        let authorization = self.allowlisted(sender, Capability::SingleAdmission)?;
        if self.runtime.ledger.suppresses_plain_reinsertion(&hash) {
            return Err(LedgerError::RepresentativeConflict.into());
        }
        let duplicate =
            AdmissionReceipt { transaction_hash: hash, group_hash: None, authorization: None };
        if self.pool.contains(&hash) {
            return Ok(duplicate);
        }
        let pooled = BasePooledTransaction::new(transaction.transaction, transaction.encoded.len());
        let inserted = tokio::time::timeout(
            self.timeout,
            self.pool.add_transaction(TransactionOrigin::Private, pooled),
        )
        .await;
        match inserted {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => return Err(error.into()),
            Err(_) if self.pool.contains(&hash) => {}
            Err(_) => {
                debug!(tx_hash = %hash, "single admission timed out");
                return Err(AdmissionError::TimedOut);
            }
        }
        Ok(AdmissionReceipt { authorization: Some(authorization), ..duplicate })
    }

    /// Admits a verified candidate group under [`Capability::CandidateSelection`] (design §5.1,
    /// steps 6-8).
    ///
    /// An exact duplicate returns before any policy check, but only once the group is pooled;
    /// while the first admission is in flight it waits, at most the admission timeout, for its
    /// outcome. A fresh group needs the access policy's admission, then gets a ledger entry and
    /// pool insertion that confirm or abort together.
    pub async fn admit_group(
        &self,
        group: Arc<CandidateGroup>,
    ) -> Result<AdmissionReceipt, AdmissionError> {
        self.ensure_sender_not_delegated(group.sender())?;
        let group_hash = group.group_hash();

        let in_flight = {
            let inflight = self.lock_inflight();
            match self.runtime.ledger.classify(&group_hash, |hash| self.pool.contains(hash)) {
                None => None,
                Some(state) if state.is_terminal() => return Err(LedgerError::GroupTerminal.into()),
                Some(GroupState::Pending) => Some(inflight.get(&group_hash).cloned()),
                Some(_) => return Ok(Self::duplicate(&group)),
            }
        };
        if let Some(receiver) = in_flight {
            return self.await_inflight(receiver, &group).await;
        }

        let policy = &self.runtime.policy;
        let capability = Capability::CandidateSelection;
        let principal = policy
            .principal(group.sender(), capability)
            .ok_or(AdmissionError::PrincipalNotAllowed)?;
        policy.admit(
            principal,
            &GroupSummary::of(group.descriptor(), group.envelope().max_access_charge),
        )?;
        let policy_version = policy.policy_version();
        let authorization = Authorization {
            principal,
            capability,
            basis: AuthorizationBasis::Policy { policy_version },
        };

        let created = {
            let mut inflight = self.lock_inflight();
            let admission = self.runtime.ledger.create(
                Arc::clone(group.descriptor()),
                principal,
                policy_version,
                |hash| self.pool.contains(hash),
            )?;
            match admission {
                LedgerAdmission::Duplicate => return Ok(Self::duplicate(&group)),
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
            Err(receiver) => return self.await_inflight(receiver, &group).await,
        };
        let representative = group.representative();
        let representative_hash = representative.hash();
        // From here the guard owns the Pending entry: cancelling this request settles it.
        let mut admission = PendingAdmission {
            inflight: Arc::clone(&self.inflight),
            runtime: Arc::clone(&self.runtime),
            pool: self.pool.clone(),
            group_hash,
            representative: representative_hash,
            outcome: Some(outcome),
        };
        let transaction = BasePooledTransaction::new(
            representative.transaction.clone(),
            representative.encoded.len(),
        )
        .with_candidate_group(Arc::clone(&group));
        let inserted = tokio::time::timeout(
            self.timeout,
            self.pool.add_transaction(TransactionOrigin::Private, transaction),
        )
        .await;
        let accepted = match inserted {
            Ok(Ok(_)) => admission.settle(true),
            Ok(Err(error)) => {
                admission.settle(false);
                return Err(error.into());
            }
            Err(_) => {
                let pooled = self.pool.contains(&representative_hash);
                let accepted = admission.settle(pooled);
                if !pooled {
                    debug!(group_hash = %group_hash, "candidate admission timed out");
                    return Err(AdmissionError::TimedOut);
                }
                accepted
            }
        };
        if !accepted {
            return Err(LedgerError::GroupTerminal.into());
        }
        Ok(AdmissionReceipt {
            transaction_hash: representative_hash,
            group_hash: Some(group_hash),
            authorization: Some(authorization),
        })
    }
}
