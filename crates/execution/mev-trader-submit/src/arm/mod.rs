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
// from outside the crate.
mod claim;
mod custody;
mod proofs;
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
pub use request::{Channel, RequestSpec};
pub use suppression::SuppressionRollbackError;
#[cfg(feature = "arm-provisioning")]
pub use suppression::provision_suppression_anchor;
pub use transport::{
    AttributionRetryToken, EgressPlan, RawBackend, RawEgress, SubmissionAttempt, SubmitOutcome,
    send_gated,
};
#[cfg(all(feature = "arm-live-egress", not(test)))]
pub use transport::ProdBackend;
pub use witness::{
    ArmRuntime, AuthorizedCandidate, AuthorizedSignedSubmission, CHAIN_ID_BASE, CheckedCandidate,
    DeploymentIdentity, DeploymentIdentitySource, DrawdownSource, FreshnessSources,
    PairedSubmission, ProofBindings, ValidatedExecutionIdentity,
};

use std::sync::atomic::{AtomicBool, Ordering};

use base_mev_trader::{KillReason, KillState, KillStateStore};

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

/// The single shared fail-stop sink: a durable [`KillStateStore`] handle plus a
/// process-local poison flag. Shared (via `Arc`) by the claim façade, the custody
/// load-and-sign path, and the egress re-validation. Once poisoned it stays
/// poisoned for the process lifetime; every entrypoint checks it first.
///
/// [`latch`](Self::latch) engages the durable kill AND sets the poison flag on
/// BOTH the success and failure of the durable `engage` — a signing/claim failure
/// is fail-stop regardless of whether the latch persisted.
pub struct ArmedFailSink {
    kill: Box<dyn KillStateStore + Send + Sync>,
    poisoned: AtomicBool,
}

impl core::fmt::Debug for ArmedFailSink {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("ArmedFailSink")
            .field("poisoned", &self.poisoned.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl ArmedFailSink {
    /// Builds a sink over a durable kill-state store.
    pub fn new(kill: Box<dyn KillStateStore + Send + Sync>) -> Self {
        Self { kill, poisoned: AtomicBool::new(false) }
    }

    /// Whether this process has been poisoned by a prior fail-stop latch.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::SeqCst)
    }

    /// Fail-closed if already poisoned.
    pub fn check(&self) -> Result<(), ArmError> {
        if self.is_poisoned() { Err(ArmError::Poisoned) } else { Ok(()) }
    }

    /// The current durable kill state (fails closed to `Unknown`).
    pub fn kill_state(&self) -> KillState {
        self.kill.load()
    }

    /// Fail-stop latch: engage the durable kill with `reason` and poison the
    /// process. The poison is set whether or not the durable engage succeeds, so a
    /// persistence failure does NOT weaken the fail-stop. Returns the corresponding
    /// [`ArmError`] the caller should surface.
    pub fn latch(&self, reason: KillReason) -> ArmError {
        // Poison FIRST (SeqCst): during a durable-engage fsync delay or failure no
        // other thread may pass `check()`. The poison is unconditional (fail-stop),
        // so a failed durable engage does not weaken the halt.
        self.poisoned.store(true, Ordering::SeqCst);
        let engaged = self.kill.engage(reason);
        match engaged {
            Ok(()) => ArmError::KillReason(reason),
            Err(_) => ArmError::LatchPersistFailed,
        }
    }
}

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
        ExactProtocol, FileKillStateStore, LossProvenance, MeasurementContext, MeasurementEncoder,
        StoreIdentity, VictimClaim, VictimClaimConfig, VictimClaimStore,
    };
    use k256::ecdsa::SigningKey;

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
    use super::witness::{DeploymentIdentity, DeploymentIdentitySource, DrawdownSource, TimeSource};

    pub(crate) const WETH: Address = address!("4200000000000000000000000000000000000006");
    pub(crate) const EXECUTOR: Address = address!("2000000000000000000000000000000000000002");
    pub(crate) const ADAPTER: Address = address!("00000000000000000000000000000000000000a1");
    pub(crate) const TOKEN: Address = address!("00000000000000000000000000000000000000c0");
    pub(crate) const POOL1: Address = address!("00000000000000000000000000000000000000f1");
    pub(crate) const POOL2: Address = address!("00000000000000000000000000000000000000f2");
    pub(crate) const CHAIN_ID: u64 = 8453;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

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
        let (signature, recovery_id) =
            key.sign_prehash_recoverable(hash.as_slice()).expect("sign");
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

    pub(crate) fn sink(dir: &Path) -> Arc<ArmedFailSink> {
        let kill = FileKillStateStore::new(dir.join("kill"));
        Arc::new(ArmedFailSink::new(Box::new(kill)))
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
