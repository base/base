//! B3-arm tier: the real hot-wallet key loader + real signer core + proof witness
//! + two-channel transport builders, all behind the `arm` Cargo feature.
//!
//! ## Red-line (the whole point)
//!
//! This module holds the FIRST real fund-submission machinery in the codebase,
//! but it is inert by construction:
//!
//! * The default and `--features phase-b` builds never compile any of it (the
//!   `arm` feature is off, so `#[cfg(feature="arm")] mod arm;` is absent).
//! * `--features arm` compiles the loader/signer/witness/transport *builders*, but
//!   NO real network egress: [`transport::ProdBackend`] (the only `reqwest` call
//!   site) is gated behind `arm-live-egress` + `not(test)`.
//! * A real send is reachable ONLY through the full linear typestate — a
//!   [`witness::AuthorizedCandidate`] issued against ALL five proofs
//!   ([`proofs`]) + the R9 [`base_mev_trader::VictimClaim`], signed by the
//!   custody loader ([`custody`]), assembled into a [`witness::PairedSubmission`],
//!   and passed through [`transport::send_gated`], which re-validates the ENTIRE
//!   freshness conjunction at the egress moment.
//! * The owner trust root ([`base_mev_trader::OWNER_ATTEST_ADDRESS`]) is `None`
//!   in every non-test build, so every proof `verify` fails closed: no proof can
//!   be minted, so `issue`/`send_gated` can never reach egress.

// All sub-modules are PRIVATE: the crate exposes ONLY the curated forward-B5
// surface re-exported below. Low-level constructors (arbitrary suppression paths,
// fixture source injection, request builders, custody loaders) are NOT reachable
// from outside the crate; fail_sink privately owns the process-lifetime poison anchor.
mod claim;
mod custody;
mod fail_sink;
pub use fail_sink::ArmedFailSink;
mod proofs;
mod providers;
mod request;
mod suppression;
mod transport;
mod witness;

// -- curated forward-B5 public surface (the ONLY items re-exported to the crate) --
pub use claim::try_claim_arm;
pub use proofs::{
    CodeHashProvider, DeploymentEvidence, DeploymentPayload, G7Attestation, G7Payload,
    LiveRunAttestation, LiveRunPayload, ProviderError, SubmitSuppressionClear,
};
pub use providers::{
    CommittedStateAuthority, DeploymentIdentityError, DrawdownAuthority, MAX_PROCESS_IMAGE_BYTES,
    MAX_RUNTIME_CODE_BYTES, ProcessBinaryIdentity, ProductionB5Runtime,
    ProductionB5RuntimeInstallError, ProductionCodeHashProvider,
    ProductionDeploymentIdentitySource, ProductionDrawdownSource,
};
pub use request::{Channel, RequestSpec};
pub use suppression::SuppressionRollbackError;
#[cfg(feature = "arm-provisioning")]
pub use suppression::provision_suppression_anchor;
#[cfg(all(feature = "arm-live-egress", not(test)))]
pub use transport::ProdBackend;
pub use transport::{
    AttributionRetryToken, EgressPlan, RawBackend, RawEgress, SubmissionAttempt, SubmitOutcome,
    send_gated,
};
pub use witness::{
    ArmRuntime, ArmRuntimeOpenError, AuthorizedCandidate, AuthorizedSignedSubmission,
    CHAIN_ID_BASE, CheckedCandidate, DeploymentIdentity, DeploymentIdentitySource, DrawdownSource,
    FreshnessSources, PairedSubmission, ProofBindings, ValidatedExecutionIdentity,
};

use base_mev_trader::KillReason;

/// Failure modes of the arm entrypoints.
#[derive(Debug)]
pub enum ArmError {
    /// A durable kill latch was engaged with this reason (fail-stop). The signing
    /// or claim path failed and the kill store persisted the engagement.
    KillReason(KillReason),
    /// The kill latch was requested but its durable persistence failed. The
    /// process is still poisoned (fail-stop); no submission may proceed.
    LatchPersistFailed,
    /// A prior latch poisoned this process; every entrypoint refuses fail-closed.
    Poisoned,
    /// An egress-moment freshness re-validation failed.
    Freshness,
    /// The victim was already claimed globally (a normal, non-latching refusal).
    AlreadyClaimed,
}

impl core::fmt::Display for ArmError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::KillReason(reason) => write!(formatter, "arm kill latched: {reason:?}"),
            Self::LatchPersistFailed => write!(formatter, "arm kill latch persistence failed"),
            Self::Poisoned => write!(formatter, "arm process is poisoned (fail-stop)"),
            Self::Freshness => write!(formatter, "arm egress freshness re-validation failed"),
            Self::AlreadyClaimed => write!(formatter, "victim already claimed globally"),
        }
    }
}

impl core::error::Error for ArmError {}

// ---------------------------------------------------------------------------
// Shared test kit (test-only; never compiled into any build without cfg(test)).
// ---------------------------------------------------------------------------
#[cfg(test)]
pub(crate) mod testkit {
    // test utilities (AGENTS.md exception): fs/heap builders, mostly non-const.

    use std::{
        io::Write,
        os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
    };

    use alloy_consensus::TxEip1559;
    use alloy_primitives::{Address, B256, Bytes, TxKind, U256, address, keccak256};
    use alloy_rpc_types_engine::PayloadId;
    use base_mev_trader::{
        ArmedCriteria, BackrunHop, BackrunPlan, BackrunPlanDigest, CampaignId, DrawdownInput,
        ExactProtocol, KillReason, KillState, KillStateStore, KillStoreError, LossProvenance,
        MeasurementContext, MeasurementEncoder, ResetAttestation, StartupError, StoreIdentity,
        VictimClaim, VictimClaimConfig, VictimClaimStore,
    };
    use k256::ecdsa::SigningKey;

    use crate::PriorityEconomicsAuthority;
    use crate::assembler::{
        AssembleInput, HopExecutionParams, ValidatedUnsignedAtomicTx, assemble_validated,
    };
    use crate::signer::{address_from_verifying_key, sign_ephemeral_atomic_tx};

    use super::ArmedFailSink;
    use super::proofs::{
        CodeHashProvider, DeploymentEvidence, DeploymentPayload, G7Attestation, G7Payload,
        LiveRunAttestation, LiveRunPayload, ProviderError,
    };
    use super::suppression::SuppressionEpochStore;
    use super::witness::{
        DeploymentIdentity, DeploymentIdentitySource, DrawdownSource, TimeSource,
    };

    pub(crate) const WETH: Address = address!("4200000000000000000000000000000000000006");
    pub(crate) const EXECUTOR: Address = address!("2000000000000000000000000000000000000002");
    pub(crate) const ADAPTER: Address = address!("00000000000000000000000000000000000000a1");
    pub(crate) const TOKEN: Address = address!("00000000000000000000000000000000000000c0");
    pub(crate) const POOL1: Address = address!("00000000000000000000000000000000000000f1");
    pub(crate) const POOL2: Address = address!("00000000000000000000000000000000000000f2");
    pub(crate) const CHAIN_ID: u64 = 8453;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    #[derive(Clone)]
    pub(crate) struct MutableKillStateStore {
        state: Arc<std::sync::Mutex<KillState>>,
        fail_engage: Arc<std::sync::atomic::AtomicBool>,
    }

    impl MutableKillStateStore {
        pub(crate) fn clear() -> Self {
            Self {
                state: Arc::new(std::sync::Mutex::new(KillState::Clear { verified_at: 0 })),
                fail_engage: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }
        }

        pub(crate) fn set(&self, state: KillState) {
            *self.state.lock().expect("mutable kill state lock") = state;
        }

        pub(crate) fn fail_engage(&self) {
            self.fail_engage.store(true, Ordering::SeqCst);
        }
    }

    impl KillStateStore for MutableKillStateStore {
        fn load(&self) -> KillState {
            *self.state.lock().expect("mutable kill state lock")
        }

        fn engage(&self, reason: KillReason) -> Result<(), KillStoreError> {
            if self.fail_engage.load(Ordering::SeqCst) {
                return Err(KillStoreError::Io);
            }
            self.set(KillState::Engaged { reason });
            Ok(())
        }

        fn owner_reset(&self, _attestation: &ResetAttestation) -> Result<(), KillStoreError> {
            Err(KillStoreError::OwnerSignatureMismatch)
        }
    }

    /// A self-cleaning private (0700) temp directory.
    pub(crate) struct TempDir {
        pub path: PathBuf,
    }

    impl TempDir {
        pub(crate) fn new(tag: &str) -> Self {
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut path = std::env::temp_dir();
            path.push(format!("b3arm-{tag}-{}-{unique}", std::process::id()));
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&path).expect("tmpdir");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    // -- owner test key + EIP-191 signing ------------------------------------

    pub(crate) fn owner_key() -> SigningKey {
        SigningKey::from_slice(&[0x11u8; 32]).expect("owner key")
    }

    pub(crate) fn owner_address() -> Address {
        address_from_verifying_key(owner_key().verifying_key())
    }

    pub(crate) fn eip191_hash(message: &[u8]) -> B256 {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(b"\x19Ethereum Signed Message:\n");
        buffer.extend_from_slice(message.len().to_string().as_bytes());
        buffer.extend_from_slice(message);
        keccak256(buffer)
    }

    pub(crate) fn eip191_sign(message: &[u8], key: &SigningKey) -> [u8; 65] {
        let hash = eip191_hash(message);
        let (signature, recovery_id) = key.sign_prehash_recoverable(hash.as_slice()).expect("sign");
        let bytes = signature.to_bytes();
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&bytes);
        out[64] = recovery_id.to_byte();
        out
    }

    // -- fake freshness sources ----------------------------------------------

    pub(crate) struct FakeProvider {
        pub code_hash: B256,
        pub block: u64,
        pub fail: bool,
    }

    impl CodeHashProvider for FakeProvider {
        fn code_hash_at_latest_committed(&self, _addr: Address) -> Result<B256, ProviderError> {
            if self.fail {
                Err(ProviderError::Unavailable("test".to_string()))
            } else {
                Ok(self.code_hash)
            }
        }
        fn current_block(&self) -> Result<u64, ProviderError> {
            if self.fail {
                Err(ProviderError::Unavailable("test".to_string()))
            } else {
                Ok(self.block)
            }
        }
    }

    pub(crate) struct FakeDrawdown(pub DrawdownInput);
    impl DrawdownSource for FakeDrawdown {
        fn load(&self) -> DrawdownInput {
            self.0
        }
    }

    pub(crate) fn complete_zero_drawdown() -> FakeDrawdown {
        FakeDrawdown(DrawdownInput::Complete {
            cumulative_realized_loss_wei: U256::ZERO,
            provenance: LossProvenance::OnchainRealized,
        })
    }

    pub(crate) struct FakeDeploymentIdentity(pub Option<DeploymentIdentity>);
    impl DeploymentIdentitySource for FakeDeploymentIdentity {
        fn current(&self) -> Option<DeploymentIdentity> {
            self.0
        }
    }

    pub(crate) struct FakeClock(pub Option<u64>);
    impl TimeSource for FakeClock {
        fn now_unix(&self) -> Option<u64> {
            self.0
        }
    }

    pub(crate) fn unarmed_criteria() -> ArmedCriteria {
        ArmedCriteria::load_optional(None)
    }

    pub(crate) fn sink(_dir: &Path) -> Arc<ArmedFailSink> {
        Arc::new(
            ArmedFailSink::new(Box::new(MutableKillStateStore::clear())).expect("clear test sink"),
        )
    }

    pub(crate) fn mutable_sink() -> (Arc<ArmedFailSink>, MutableKillStateStore) {
        let store = MutableKillStateStore::clear();
        let sink = Arc::new(ArmedFailSink::new(Box::new(store.clone())).expect("clear test sink"));
        (sink, store)
    }

    pub(crate) fn checked_sink(
        store: MutableKillStateStore,
    ) -> Result<ArmedFailSink, StartupError> {
        ArmedFailSink::new(Box::new(store))
    }

    // -- proof builders ------------------------------------------------------

    pub(crate) fn g7(campaign: CampaignId, expiry: u64, now: u64) -> G7Attestation {
        let payload = G7Payload { campaign_id: campaign, g7_closure_epoch: 7, expiry_unix: expiry };
        let signature = eip191_sign(&payload.preimage(), &owner_key());
        G7Attestation::verify_with_owner(&payload, &signature, now, owner_address()).expect("g7")
    }

    pub(crate) fn live(campaign: CampaignId, expiry: u64, now: u64) -> LiveRunAttestation {
        live_windowed(campaign, 0, expiry, now)
    }

    pub(crate) fn live_windowed(
        campaign: CampaignId,
        window_start: u64,
        expiry: u64,
        now: u64,
    ) -> LiveRunAttestation {
        let payload = LiveRunPayload { campaign_id: campaign, window_start, expiry_unix: expiry };
        let signature = eip191_sign(&payload.preimage(), &owner_key());
        LiveRunAttestation::verify_with_owner(&payload, &signature, now, owner_address())
            .expect("live")
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn deployment(
        provider: &dyn CodeHashProvider,
        executor: Address,
        code_hash: B256,
        binary_digest: B256,
        deployment_digest: B256,
        store: StoreIdentity,
    ) -> DeploymentEvidence {
        let payload = DeploymentPayload {
            chain_id: CHAIN_ID,
            executor,
            code_hash,
            binary_digest,
            deployment_digest,
            r9_store_identity: store,
        };
        let signature = eip191_sign(&payload.preimage(), &owner_key());
        DeploymentEvidence::verify_with_owner(&payload, &signature, provider, owner_address())
            .expect("deployment")
    }

    // -- suppression fixtures ------------------------------------------------

    pub(crate) fn write_suppression_file(dir: &Path, epoch: u64, suppressed: bool) -> PathBuf {
        let path = dir.join("mev-suppression.json");
        let body = format!("{{\"version\":1,\"epoch\":{epoch},\"suppressed\":{suppressed}}}");
        std::fs::write(&path, body).expect("write suppression file");
        path
    }

    pub(crate) fn epoch_store(dir: &Path) -> SuppressionEpochStore {
        SuppressionEpochStore::bootstrap(dir.join("hw.redb")).expect("bootstrap epoch store")
    }

    // -- custody fixtures ----------------------------------------------------

    /// Writes a hot-wallet file (0600) for `key` and returns its path.
    pub(crate) fn write_hot_wallet(dir: &Path, key: &SigningKey) -> PathBuf {
        let path = dir.join("hotwallet");
        let secret = key.to_bytes();
        let content = format!("0x{}", alloy_primitives::hex::encode(secret));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .expect("open hot wallet");
        file.write_all(content.as_bytes()).expect("write hot wallet");
        drop(file);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        path
    }

    /// A test hot-wallet key (fixed non-zero seed) and its derived address.
    pub(crate) fn hot_wallet_key() -> (SigningKey, Address) {
        let key = SigningKey::from_slice(&[0x42u8; 32]).expect("hot key");
        let address = address_from_verifying_key(key.verifying_key());
        (key, address)
    }

    // -- validated-tx + plan fixtures ----------------------------------------

    pub(crate) fn victim_env(priority: u128) -> (Vec<u8>, B256) {
        let unsigned = TxEip1559 {
            chain_id: CHAIN_ID,
            nonce: 1,
            gas_limit: 100_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: priority,
            to: TxKind::Call(POOL1),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Bytes::new(),
        };
        let signed = sign_ephemeral_atomic_tx(&unsigned).expect("victim envelope");
        let hash = keccak256(&signed.raw_backrun);
        (signed.raw_backrun, hash)
    }

    pub(crate) fn plan(victim: B256) -> BackrunPlan {
        let mut plan = BackrunPlan {
            parent_hash: B256::ZERO,
            block_number: 0,
            predecessor_index: 0,
            payload_id: PayloadId::new([0u8; 8]),
            victim,
            route: [
                BackrunHop {
                    pool: POOL1,
                    protocol: ExactProtocol::UniswapV2,
                    token_in: WETH,
                    token_out: TOKEN,
                    fee_pips: 3_000,
                },
                BackrunHop {
                    pool: POOL2,
                    protocol: ExactProtocol::UniswapV2,
                    token_in: TOKEN,
                    token_out: WETH,
                    fee_pips: 3_000,
                },
            ],
            amount_in: U256::from(1_000_000_000_000_000_000u128),
            amount_out: U256::from(1_010_000_000_000_000_000u128),
            gross_profit: U256::from(10_000_000_000_000_000u128),
            digest: BackrunPlanDigest(B256::ZERO),
        };
        plan.digest = MeasurementEncoder::digest(&plan).expect("plan digest");
        plan
    }

    /// Assemble a validated unsigned tx for `executor`; returns it plus the victim
    /// hash (so a matching `VictimClaim` can be minted).
    pub(crate) fn validated_tx(executor: Address) -> (ValidatedUnsignedAtomicTx, B256) {
        let (victim_raw, victim_hash) = victim_env(37);
        let plan = plan(victim_hash);
        let frame = MeasurementContext {
            parent_hash: plan.parent_hash,
            block_number: plan.block_number,
            predecessor_index: plan.predecessor_index,
            payload_id: plan.payload_id,
            victim: plan.victim,
        };
        let input = AssembleInput {
            plan: &plan,
            current_frame: frame,
            executor,
            hops: [
                HopExecutionParams { adapter: ADAPTER, min_amount_out: U256::from(1u64) },
                HopExecutionParams { adapter: ADAPTER, min_amount_out: U256::from(1u64) },
            ],
            chain_id: CHAIN_ID,
            nonce: 0,
            gas: 2_000_000,
            max_fee_per_gas: 1_000_000_000,
            valid_until_block: 12_345_678,
            victim_raw_tx: &victim_raw,
            victim_tx_hash: victim_hash,
            expected_victim_priority_fee: Some(37),
            priority_economics: Some(PriorityEconomicsAuthority::new(
                U256::from(1),
                U256::from(1),
                U256::from(1),
                plan.block_number,
            )),
        };
        let vtx = assemble_validated(&input).expect("validated tx");
        (vtx, victim_hash)
    }

    /// Bootstrap a victim claim store and claim `victim` under `campaign`, returning
    /// the proof + the store identity.
    pub(crate) fn victim_claim(
        dir: &Path,
        victim: B256,
        campaign: CampaignId,
    ) -> (VictimClaim, StoreIdentity) {
        let config = VictimClaimConfig { db_path: dir.join("claims.redb") };
        let store = VictimClaimStore::bootstrap(&config).expect("bootstrap claim store");
        let identity = store.store_identity();
        let claim = match store.try_claim(CHAIN_ID, victim, campaign).expect("claim") {
            base_mev_trader::ClaimResult::Claimed(proof) => proof,
            base_mev_trader::ClaimResult::AlreadyClaimed => panic!("unexpected already claimed"),
        };
        (claim, identity)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_primitives::B256;
    use base_mev_trader::{
        CampaignId, ClaimResult, KillReason, KillState, VictimClaimConfig, VictimClaimStore,
    };

    use super::{ArmError, testkit as tk, try_claim_arm};

    #[test]
    fn startup_non_clear_refuses_without_sink() {
        let store = tk::MutableKillStateStore::clear();
        store.set(KillState::Unknown);
        assert!(matches!(
            tk::checked_sink(store),
            Err(base_mev_trader::StartupError::KillStateNotClear)
        ));
    }

    #[test]
    fn claim_is_first_observer_after_clear_becomes_unknown() {
        let dir = tk::TempDir::new("claim-observe-kill");
        let config = VictimClaimConfig { db_path: dir.path.join("claims.redb") };
        let claims = VictimClaimStore::bootstrap(&config).expect("bootstrap claim store");
        let victim = B256::repeat_byte(0xA1);
        let campaign = CampaignId::new([0xA2; 32]);
        let (sink, store) = tk::mutable_sink();
        store.set(KillState::Unknown);

        let result = try_claim_arm(&claims, 8453, victim, campaign, &sink);

        assert!(matches!(result, Err(ArmError::Poisoned)));
        assert!(sink.is_poisoned());
        assert!(matches!(
            claims.try_claim(8453, victim, campaign).expect("direct claim after refusal"),
            ClaimResult::Claimed(_)
        ));
    }

    #[test]
    fn poison_is_sticky_after_store_returns_clear() {
        let (sink, store) = tk::mutable_sink();
        store.set(KillState::Unknown);
        assert!(matches!(sink.check(), Err(ArmError::Poisoned)));

        store.set(KillState::Clear { verified_at: 1 });
        assert!(matches!(sink.check(), Err(ArmError::Poisoned)));
        assert!(sink.is_poisoned());
    }

    #[test]
    fn latch_poison_persists_when_engage_fails() {
        let store = tk::MutableKillStateStore::clear();
        store.fail_engage();
        let sink = Arc::new(tk::checked_sink(store).expect("clear startup"));

        assert!(matches!(
            sink.latch(KillReason::KeyOrSignatureFailure),
            ArmError::LatchPersistFailed
        ));
        assert!(sink.is_poisoned());
        assert!(matches!(sink.check(), Err(ArmError::Poisoned)));
    }
}
