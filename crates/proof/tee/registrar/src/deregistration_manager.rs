//! Signer deregistration management for orphaned prover signers.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::{Arc, Mutex},
};

use alloy_primitives::{Address, Bytes};
use alloy_sol_types::SolCall;
use base_proof_contracts::ITEEProverRegistry;
use base_tx_manager::{TxCandidate, TxManager};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{RegistrarMetrics, RegistryClient, Result};

/// Manages orphan signer cleanup and `TEEProverRegistry.deregisterSigner` transactions.
pub struct DeregistrationManager<'a, R: ?Sized, T: ?Sized> {
    registry_address: Address,
    registry: &'a R,
    tx_manager: &'a T,
    signer_history: &'a Arc<Mutex<HashMap<Address, String>>>,
}

impl<R: ?Sized, T: ?Sized> fmt::Debug for DeregistrationManager<'_, R, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeregistrationManager")
            .field("registry_address", &self.registry_address)
            .finish_non_exhaustive()
    }
}

impl<'a, R: ?Sized, T: ?Sized> DeregistrationManager<'a, R, T> {
    /// Creates a manager for orphan signer cleanup.
    pub const fn new(
        registry_address: Address,
        registry: &'a R,
        tx_manager: &'a T,
        signer_history: &'a Arc<Mutex<HashMap<Address, String>>>,
    ) -> Self {
        Self { registry_address, registry, tx_manager, signer_history }
    }
}

impl<R, T> DeregistrationManager<'_, R, T>
where
    R: RegistryClient + ?Sized,
    T: TxManager + ?Sized,
{
    /// Loads registered signers and deregisters unprotected ones.
    pub async fn run_orphan_dereg(
        &self,
        protected_signers: &HashSet<Address>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let registered_signers = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                debug!("cancelled before loading registered signers for orphan dereg");
                return Ok(());
            }
            res = self.registry.get_registered_signers() => res?,
        };
        self.deregister_orphans(protected_signers, &registered_signers, cancel).await
    }

    /// Builds the transaction candidate for `TEEProverRegistry.deregisterSigner`.
    pub fn candidate(&self, signer: Address) -> TxCandidate {
        let calldata =
            Bytes::from(ITEEProverRegistry::deregisterSignerCall { signer }.abi_encode());

        TxCandidate { tx_data: calldata, to: Some(self.registry_address), ..Default::default() }
    }

    /// Submits a `deregisterSigner` transaction and returns whether it succeeded.
    pub async fn submit_deregistration(&self, signer: Address) -> bool {
        let candidate = self.candidate(signer);
        let last_known_instance = {
            let history = self.signer_history.lock().unwrap_or_else(|e| e.into_inner());
            history.get(&signer).cloned()
        };
        info!(
            signer = %signer,
            last_known_instance = ?last_known_instance,
            registry = %self.registry_address,
            calldata_len = candidate.tx_data.len(),
            "Deregistering signer"
        );

        info!(
            tx = ?candidate,
            "Sending tx candidate",
        );

        match self.tx_manager.send(candidate).await {
            Ok(receipt) => {
                if !receipt.inner.status() {
                    warn!(
                        signer = %signer,
                        tx_hash = %receipt.transaction_hash,
                        "deregistration transaction reverted onchain",
                    );
                    RegistrarMetrics::processing_errors_total().increment(1);
                    return false;
                }
                info!(
                    signer = %signer,
                    tx_hash = %receipt.transaction_hash,
                    "signer deregistered"
                );
                true
            }
            Err(e) => {
                warn!(error = %e, signer = %signer, "failed to deregister signer");
                RegistrarMetrics::processing_errors_total().increment(1);
                false
            }
        }
    }

    /// Deregisters registered signers that are not currently protected.
    ///
    /// Each candidate is checked against `isRegisteredSigner` before submitting a
    /// transaction so stale `getRegisteredSigners()` entries do not loop forever.
    pub async fn deregister_orphans(
        &self,
        protected_signers: &HashSet<Address>,
        registered_signers: &[Address],
        cancel: &CancellationToken,
    ) -> Result<()> {
        let orphans: Vec<_> = registered_signers
            .iter()
            .copied()
            .filter(|addr| !protected_signers.contains(addr))
            .collect();

        if orphans.is_empty() {
            return Ok(());
        }

        info!(count = orphans.len(), "deregistering orphan signers");

        let mut deregistered = 0usize;

        for signer in orphans {
            if cancel.is_cancelled() {
                debug!("shutdown requested, stopping orphan deregistration");
                break;
            }

            match self.registry.is_registered(signer).await {
                Ok(false) => {
                    warn!(
                        signer = %signer,
                        "signer appears in getRegisteredSigners but isRegisteredSigner is false, \
                         skipping (possible EnumerableSet ghost entry)"
                    );
                    continue;
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        signer = %signer,
                        "failed to verify signer registration status, skipping deregistration"
                    );
                    continue;
                }
                Ok(true) => {}
            }

            if self.submit_deregistration(signer).await {
                RegistrarMetrics::deregistrations_total().increment(1);
                deregistered += 1;
            }
        }

        if deregistered > 0 {
            info!(count = deregistered, "orphan signers deregistered");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
    use alloy_primitives::{B256, Bloom};
    use alloy_rpc_types_eth::TransactionReceipt;
    use async_trait::async_trait;
    use base_tx_manager::SendHandle;
    use rstest::rstest;
    use tokio::sync::Notify;

    use super::*;

    const REGISTRY_ADDRESS: Address = Address::new([0x11; 20]);
    const SIGNER_A: Address = Address::new([0xAA; 20]);
    const SIGNER_B: Address = Address::new([0xBB; 20]);
    const SIGNER_C: Address = Address::new([0xCC; 20]);

    #[test]
    fn candidate_targets_registry_with_deregister_signer_calldata() {
        let registry = MockRegistry::with_signers(vec![SIGNER_A]);
        let tx_manager = MockTxManager::default();
        let history = Arc::new(Mutex::new(HashMap::new()));
        let manager =
            DeregistrationManager::new(REGISTRY_ADDRESS, &registry, &tx_manager, &history);

        let candidate = manager.candidate(SIGNER_A);

        assert_eq!(candidate.to, Some(REGISTRY_ADDRESS));
        assert_eq!(
            candidate.tx_data,
            Bytes::from(ITEEProverRegistry::deregisterSignerCall { signer: SIGNER_A }.abi_encode())
        );
        assert_eq!(candidate.gas_limit, 0);
        assert!(candidate.blobs.is_empty());
    }

    #[rstest]
    #[case::no_orphans(vec![SIGNER_A, SIGNER_B], vec![SIGNER_A, SIGNER_B], 0)]
    #[case::one_orphan(vec![SIGNER_A, SIGNER_B], vec![SIGNER_A], 1)]
    #[case::all_orphans(vec![SIGNER_A, SIGNER_B], vec![], 2)]
    #[tokio::test]
    async fn deregister_orphans_tx_count(
        #[case] registered_signers: Vec<Address>,
        #[case] protected_signers: Vec<Address>,
        #[case] expected_txs: usize,
    ) {
        let registry = MockRegistry::with_signers(registered_signers.clone());
        let tx_manager = MockTxManager::default();
        let history = Arc::new(Mutex::new(HashMap::new()));
        let manager =
            DeregistrationManager::new(REGISTRY_ADDRESS, &registry, &tx_manager, &history);
        let protected_signers: HashSet<Address> = protected_signers.into_iter().collect();

        manager
            .deregister_orphans(&protected_signers, &registered_signers, &CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(tx_manager.take_candidates().len(), expected_txs);
    }

    #[tokio::test]
    async fn deregister_orphans_submits_only_unprotected_signers() {
        let registry = MockRegistry::with_signers(vec![SIGNER_A, SIGNER_B]);
        let tx_manager = MockTxManager::default();
        let history = Arc::new(Mutex::new(HashMap::new()));
        let manager =
            DeregistrationManager::new(REGISTRY_ADDRESS, &registry, &tx_manager, &history);

        manager
            .deregister_orphans(
                &HashSet::from([SIGNER_A]),
                &[SIGNER_A, SIGNER_B],
                &CancellationToken::new(),
            )
            .await
            .unwrap();

        let sent = tx_manager.take_candidates();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].tx_data,
            Bytes::from(ITEEProverRegistry::deregisterSignerCall { signer: SIGNER_B }.abi_encode())
        );
    }

    #[tokio::test]
    async fn deregister_orphans_skips_ghost_entries() {
        let registry = MockRegistry::with_enumerated_and_true_signers(
            vec![SIGNER_A, SIGNER_B],
            vec![SIGNER_B],
        );
        let tx_manager = MockTxManager::default();
        let history = Arc::new(Mutex::new(HashMap::new()));
        let manager =
            DeregistrationManager::new(REGISTRY_ADDRESS, &registry, &tx_manager, &history);

        manager.run_orphan_dereg(&HashSet::new(), &CancellationToken::new()).await.unwrap();

        let sent = tx_manager.take_candidates();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].tx_data,
            Bytes::from(ITEEProverRegistry::deregisterSignerCall { signer: SIGNER_B }.abi_encode())
        );
    }

    #[tokio::test]
    async fn deregister_orphans_skips_all_ghosts_sends_nothing() {
        let registry = MockRegistry::with_enumerated_and_true_signers(
            vec![SIGNER_A, SIGNER_B, SIGNER_C],
            vec![],
        );
        let tx_manager = MockTxManager::default();
        let history = Arc::new(Mutex::new(HashMap::new()));
        let manager =
            DeregistrationManager::new(REGISTRY_ADDRESS, &registry, &tx_manager, &history);

        manager.run_orphan_dereg(&HashSet::new(), &CancellationToken::new()).await.unwrap();

        assert!(tx_manager.take_candidates().is_empty());
    }

    #[tokio::test]
    async fn deregister_orphans_respects_cancellation() {
        let registry = MockRegistry::with_signers(vec![SIGNER_A]);
        let tx_manager = MockTxManager::default();
        let history = Arc::new(Mutex::new(HashMap::new()));
        let manager =
            DeregistrationManager::new(REGISTRY_ADDRESS, &registry, &tx_manager, &history);
        let cancel = CancellationToken::new();
        cancel.cancel();

        manager.deregister_orphans(&HashSet::new(), &[SIGNER_A], &cancel).await.unwrap();

        assert!(tx_manager.take_candidates().is_empty());
    }

    #[tokio::test]
    async fn run_orphan_dereg_respects_cancellation_while_loading_signers() {
        let registry = MockRegistry::stalling_get_registered_signers();
        let get_registered_signers_started = registry.get_registered_signers_started();
        let tx_manager = MockTxManager::default();
        let history = Arc::new(Mutex::new(HashMap::new()));
        let manager =
            DeregistrationManager::new(REGISTRY_ADDRESS, &registry, &tx_manager, &history);
        let cancel = CancellationToken::new();
        let protected_signers = HashSet::new();

        let run = manager.run_orphan_dereg(&protected_signers, &cancel);
        tokio::pin!(run);

        let notified = get_registered_signers_started.notified();
        tokio::pin!(notified);
        tokio::select! {
            () = &mut notified => {}
            result = &mut run => panic!("run_orphan_dereg completed before cancellation: {result:?}"),
        }

        cancel.cancel();

        tokio::time::timeout(Duration::from_secs(1), run)
            .await
            .expect("run_orphan_dereg should stop promptly after cancellation")
            .unwrap();
        assert!(tx_manager.take_candidates().is_empty());
    }

    #[derive(Debug)]
    struct MockRegistry {
        signers: Vec<Address>,
        true_signers: HashSet<Address>,
        get_registered_signers_started: Arc<Notify>,
        stall_get_registered_signers: bool,
    }

    impl MockRegistry {
        fn with_signers(signers: Vec<Address>) -> Self {
            Self::with_enumerated_and_true_signers(signers.clone(), signers)
        }

        fn with_enumerated_and_true_signers(
            enumerated_signers: Vec<Address>,
            true_signers: Vec<Address>,
        ) -> Self {
            Self {
                signers: enumerated_signers,
                true_signers: true_signers.into_iter().collect(),
                get_registered_signers_started: Arc::new(Notify::new()),
                stall_get_registered_signers: false,
            }
        }

        fn stalling_get_registered_signers() -> Self {
            Self {
                signers: vec![],
                true_signers: HashSet::new(),
                get_registered_signers_started: Arc::new(Notify::new()),
                stall_get_registered_signers: true,
            }
        }

        fn get_registered_signers_started(&self) -> Arc<Notify> {
            Arc::clone(&self.get_registered_signers_started)
        }
    }

    #[async_trait]
    impl RegistryClient for MockRegistry {
        async fn is_registered(&self, signer: Address) -> Result<bool> {
            Ok(self.true_signers.contains(&signer))
        }

        async fn get_registered_signers(&self) -> Result<Vec<Address>> {
            self.get_registered_signers_started.notify_waiters();
            if self.stall_get_registered_signers {
                future::pending::<()>().await;
            }
            Ok(self.signers.clone())
        }
    }

    #[derive(Debug, Default, Clone)]
    struct MockTxManager {
        sent_candidates: Arc<Mutex<Vec<TxCandidate>>>,
    }

    impl MockTxManager {
        fn take_candidates(&self) -> Vec<TxCandidate> {
            std::mem::take(&mut *self.sent_candidates.lock().unwrap())
        }
    }

    impl TxManager for MockTxManager {
        async fn send(&self, candidate: TxCandidate) -> base_tx_manager::SendResponse {
            self.sent_candidates.lock().unwrap().push(candidate);
            Ok(stub_receipt())
        }

        async fn send_async(&self, _candidate: TxCandidate) -> SendHandle {
            unreachable!("deregistration manager tests use synchronous send")
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    fn stub_receipt() -> TransactionReceipt {
        let inner = ReceiptEnvelope::Legacy(ReceiptWithBloom {
            receipt: Receipt {
                status: Eip658Value::Eip658(true),
                cumulative_gas_used: 21_000,
                logs: vec![],
            },
            logs_bloom: Bloom::ZERO,
        });
        TransactionReceipt {
            inner,
            transaction_hash: B256::ZERO,
            transaction_index: Some(0),
            block_hash: Some(B256::ZERO),
            block_number: Some(1),
            gas_used: 21_000,
            effective_gas_price: 1_000_000_000,
            blob_gas_used: None,
            blob_gas_price: None,
            from: Address::ZERO,
            to: Some(Address::ZERO),
            contract_address: None,
        }
    }
}
