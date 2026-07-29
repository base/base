//! The five arm proof types. Each has private fields, NO derive, and a real
//! verification with no empty constructor:
//!
//! * [`G7Attestation`] — owner-signed G7-closure attestation (reusable in window).
//! * [`LiveRunAttestation`] — owner-signed live-run window (reusable in window).
//! * [`SubmitSuppressionClear`] — a fresh, non-suppressed, non-rolled-back epoch.
//! * [`DeploymentEvidence`] — owner-signed deployment bound to a keyless on-chain
//!   code-hash re-lookup ([`CodeHashProvider`]).
//! * `base_mev_trader::VictimClaim` — the R9 at-most-once claim (re-used as-is).
//!
//! Owner signatures are EIP-191 (`recover_address_from_msg`) verified against the
//! compile-pinned [`base_mev_trader::OWNER_ATTEST_ADDRESS`], which is `None` in
//! every non-test build — so `verify` fails closed everywhere in production. The
//! `#[cfg(test)] verify_with_owner` seam pins an explicit owner so positive
//! vectors can be exercised (the dependency is compiled without `cfg(test)`, so
//! its owner is `None`).

use alloy_primitives::{Address, B256, Signature, keccak256};
use base_mev_trader::{CampaignId, OWNER_ATTEST_ADDRESS, StoreIdentity};

use super::suppression::{SuppressionEpochStore, SuppressionFileStore};
use super::witness::CHAIN_ID_BASE;

// -- domain tags (byte-exact) -------------------------------------------------

const G7_DOMAIN: &[u8] = b"base-mev/g7-closure/v1";
const LIVE_DOMAIN: &[u8] = b"base-mev/live-run/v1";
const DEPLOY_DOMAIN: &[u8] = b"base-mev/deploy/v1";

/// EIP-191 recover-and-compare: returns true iff the 65-byte signature recovers to
/// `owner` over `preimage`. Any malformed signature or recovery failure is false.
fn recover_matches(preimage: &[u8], signature: &[u8; 65], owner: Address) -> bool {
    let Ok(parsed) = Signature::from_raw_array(signature) else {
        return false;
    };
    parsed.recover_address_from_msg(preimage).is_ok_and(|recovered| recovered == owner)
}

// -- CodeHashProvider (keyless, in-process) -----------------------------------

/// A code-hash lookup error from the in-node state reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// The underlying state read failed / was unavailable.
    Unavailable(String),
    /// The authority returned a value that cannot authorize submission.
    Invalid(&'static str),
    /// An authority value exceeded its explicit resource bound.
    TooLarge {
        /// Bounded value being read.
        subject: &'static str,
        /// Maximum accepted bytes/value.
        limit: u64,
        /// Observed bytes/value.
        actual: u64,
    },
}

impl core::fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unavailable(message) => write!(formatter, "provider unavailable: {message}"),
            Self::Invalid(message) => write!(formatter, "provider value invalid: {message}"),
            Self::TooLarge { subject, limit, actual } => {
                write!(formatter, "{subject} exceeds bound {limit}: {actual}")
            }
        }
    }
}

impl core::error::Error for ProviderError {}

/// Keyless in-process code-hash + head reader. B5 injects a node-local state
/// reader implementation; this is NOT an HTTP/RPC client, so the arm crate keeps
/// its single-egress-site seal (`ProdBackend::execute`) intact.
pub trait CodeHashProvider {
    /// `keccak256(code_bytes)` of `addr` at the latest committed block. Production
    /// implementations reject absent/empty code rather than authorizing its conventional hash.
    fn code_hash_at_latest_committed(&self, addr: Address) -> Result<B256, ProviderError>;

    /// The latest committed block number.
    fn current_block(&self) -> Result<u64, ProviderError>;

    /// Native balance of a present account at the latest committed canonical head.
    ///
    /// `Ok(None)` means the account was absent and must fail closed; it is not a zero balance.
    fn native_balance_at_latest_committed(
        &self,
        address: Address,
    ) -> Result<Option<alloy_primitives::U256>, ProviderError>;
}

// -- G7Attestation ------------------------------------------------------------

/// The owner-signed G7-closure payload.
#[derive(Debug, Clone, Copy)]
pub struct G7Payload {
    /// Campaign this closure covers.
    pub campaign_id: CampaignId,
    /// G7 closure epoch.
    pub g7_closure_epoch: u64,
    /// Attestation expiry (unix seconds).
    pub expiry_unix: u64,
}

impl G7Payload {
    pub(crate) fn preimage(&self) -> Vec<u8> {
        let mut buffer = Vec::with_capacity(32 + 32 + 8 + 8);
        buffer.extend_from_slice(keccak256(G7_DOMAIN).as_slice());
        buffer.extend_from_slice(self.campaign_id.as_bytes());
        buffer.extend_from_slice(&self.g7_closure_epoch.to_be_bytes());
        buffer.extend_from_slice(&self.expiry_unix.to_be_bytes());
        buffer
    }
}

/// A verified G7-closure attestation. Reusable within its window (no consume, no
/// nonce): it is a gate-state, and at-most-once is the R9 claim's job.
#[derive(Debug)]
pub struct G7Attestation {
    campaign_id: CampaignId,
    expiry_unix: u64,
}

impl G7Attestation {
    /// Verifies against the compile-pinned owner and `expiry_unix > now`.
    pub fn verify(payload: &G7Payload, signature: &[u8; 65], now: u64) -> Option<Self> {
        let owner = OWNER_ATTEST_ADDRESS?;
        Self::verify_inner(payload, signature, now, owner)
    }

    fn verify_inner(
        payload: &G7Payload,
        signature: &[u8; 65],
        now: u64,
        owner: Address,
    ) -> Option<Self> {
        if payload.expiry_unix <= now {
            return None;
        }
        if !recover_matches(&payload.preimage(), signature, owner) {
            return None;
        }
        Some(Self { campaign_id: payload.campaign_id, expiry_unix: payload.expiry_unix })
    }

    /// Test seam: verify against an explicit owner (dependency owner is `None`).
    #[cfg(test)]
    pub fn verify_with_owner(
        payload: &G7Payload,
        signature: &[u8; 65],
        now: u64,
        owner: Address,
    ) -> Option<Self> {
        Self::verify_inner(payload, signature, now, owner)
    }

    /// Whether this attestation covers `campaign`.
    pub fn covers(&self, campaign: CampaignId) -> bool {
        self.campaign_id == campaign
    }

    /// The covered campaign.
    pub const fn campaign_id(&self) -> CampaignId {
        self.campaign_id
    }

    /// The attestation expiry (unix seconds).
    pub const fn expiry(&self) -> u64 {
        self.expiry_unix
    }
}

// -- LiveRunAttestation -------------------------------------------------------

/// The owner-signed live-run window payload.
#[derive(Debug, Clone, Copy)]
pub struct LiveRunPayload {
    /// Campaign this live-run window covers.
    pub campaign_id: CampaignId,
    /// Window start (unix seconds, inclusive).
    pub window_start: u64,
    /// Window expiry (unix seconds, exclusive).
    pub expiry_unix: u64,
}

impl LiveRunPayload {
    pub(crate) fn preimage(&self) -> Vec<u8> {
        let mut buffer = Vec::with_capacity(32 + 32 + 8 + 8);
        buffer.extend_from_slice(keccak256(LIVE_DOMAIN).as_slice());
        buffer.extend_from_slice(self.campaign_id.as_bytes());
        buffer.extend_from_slice(&self.window_start.to_be_bytes());
        buffer.extend_from_slice(&self.expiry_unix.to_be_bytes());
        buffer
    }
}

/// A verified live-run attestation. Reusable within `[window_start, expiry)`.
#[derive(Debug)]
pub struct LiveRunAttestation {
    campaign_id: CampaignId,
    window_start: u64,
    expiry_unix: u64,
}

impl LiveRunAttestation {
    /// Verifies against the compile-pinned owner and `window_start <= now < expiry`.
    pub fn verify(payload: &LiveRunPayload, signature: &[u8; 65], now: u64) -> Option<Self> {
        let owner = OWNER_ATTEST_ADDRESS?;
        Self::verify_inner(payload, signature, now, owner)
    }

    fn verify_inner(
        payload: &LiveRunPayload,
        signature: &[u8; 65],
        now: u64,
        owner: Address,
    ) -> Option<Self> {
        if now < payload.window_start || now >= payload.expiry_unix {
            return None;
        }
        if !recover_matches(&payload.preimage(), signature, owner) {
            return None;
        }
        Some(Self {
            campaign_id: payload.campaign_id,
            window_start: payload.window_start,
            expiry_unix: payload.expiry_unix,
        })
    }

    /// Test seam: verify against an explicit owner.
    #[cfg(test)]
    pub fn verify_with_owner(
        payload: &LiveRunPayload,
        signature: &[u8; 65],
        now: u64,
        owner: Address,
    ) -> Option<Self> {
        Self::verify_inner(payload, signature, now, owner)
    }

    /// Whether this attestation covers `campaign`.
    pub fn covers(&self, campaign: CampaignId) -> bool {
        self.campaign_id == campaign
    }

    /// The covered campaign.
    pub const fn campaign_id(&self) -> CampaignId {
        self.campaign_id
    }

    /// The live-run window start (unix seconds, inclusive), re-checked at egress.
    pub const fn window_start(&self) -> u64 {
        self.window_start
    }

    /// The attestation expiry (unix seconds).
    pub const fn expiry(&self) -> u64 {
        self.expiry_unix
    }
}

// -- SubmitSuppressionClear ---------------------------------------------------

/// Proof that submission is NOT suppressed at a fresh, non-rolled-back epoch.
#[derive(Debug)]
pub struct SubmitSuppressionClear {
    epoch: u64,
}

impl SubmitSuppressionClear {
    /// Reads the suppression file fresh and records the epoch as a non-decreasing
    /// high-water mark. `Some` only when the file parses, `suppressed == false`,
    /// AND `epoch_store.observe(epoch)` succeeds (monotonic, not a rollback). Any
    /// read/parse/observe failure is `None` (fail-closed).
    pub(crate) fn read(
        file_store: &SuppressionFileStore,
        epoch_store: &SuppressionEpochStore,
    ) -> Option<Self> {
        // Fail-closed on a present writer lock before AND after the JSON read
        // (mid-write or stale crash): treat as suppressed regardless of JSON
        // contents (ralplan §2.2 shared contract with the TS O_EXCL-no-auto-steal
        // writer). The SAME guarded path is used by the egress re-validation.
        let record = file_store.read_fresh_guarded()?;
        if record.suppressed {
            return None;
        }
        epoch_store.observe(record.epoch).ok()?;
        Some(Self { epoch: record.epoch })
    }

    /// The cleared epoch.
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }
}

// -- DeploymentEvidence -------------------------------------------------------

/// The owner-signed deployment payload, bound to a keyless on-chain code-hash.
#[derive(Debug, Clone, Copy)]
pub struct DeploymentPayload {
    /// Chain id (Base = 8453).
    pub chain_id: u64,
    /// The executor contract address.
    pub executor: Address,
    /// The attested code hash of the executor (`keccak256(code)`).
    pub code_hash: B256,
    /// The attested build/binary digest.
    pub binary_digest: B256,
    /// The attested deployment digest.
    pub deployment_digest: B256,
    /// The R9 claim-store identity this deployment is bound to.
    pub r9_store_identity: StoreIdentity,
}

impl DeploymentPayload {
    pub(crate) fn preimage(&self) -> Vec<u8> {
        let mut buffer = Vec::with_capacity(32 + 8 + 20 + 32 + 32 + 32 + 32);
        buffer.extend_from_slice(keccak256(DEPLOY_DOMAIN).as_slice());
        buffer.extend_from_slice(&self.chain_id.to_be_bytes());
        buffer.extend_from_slice(self.executor.as_slice());
        buffer.extend_from_slice(self.code_hash.as_slice());
        buffer.extend_from_slice(self.binary_digest.as_slice());
        buffer.extend_from_slice(self.deployment_digest.as_slice());
        buffer.extend_from_slice(self.r9_store_identity.as_bytes());
        buffer
    }
}

/// Verified deployment evidence: an owner signature AND a keyless on-chain
/// re-lookup that the executor's live code hash equals the attested hash.
#[derive(Debug)]
pub struct DeploymentEvidence {
    executor: Address,
    code_hash: B256,
    binary_digest: B256,
    deployment_digest: B256,
    r9_store_identity: StoreIdentity,
}

impl DeploymentEvidence {
    /// Verifies the owner signature AND that the on-chain code hash at the latest
    /// committed block matches the attested `code_hash`.
    pub fn verify(
        payload: &DeploymentPayload,
        signature: &[u8; 65],
        provider: &dyn CodeHashProvider,
    ) -> Option<Self> {
        let owner = OWNER_ATTEST_ADDRESS?;
        Self::verify_inner(payload, signature, provider, owner)
    }

    fn verify_inner(
        payload: &DeploymentPayload,
        signature: &[u8; 65],
        provider: &dyn CodeHashProvider,
        owner: Address,
    ) -> Option<Self> {
        // The deployment must be on Base (8453) — a non-Base deployment attestation
        // must never bind to a Base claim/executor.
        if payload.chain_id != CHAIN_ID_BASE {
            return None;
        }
        if !recover_matches(&payload.preimage(), signature, owner) {
            return None;
        }
        let onchain = provider.code_hash_at_latest_committed(payload.executor).ok()?;
        if onchain != payload.code_hash {
            return None;
        }
        Some(Self {
            executor: payload.executor,
            code_hash: payload.code_hash,
            binary_digest: payload.binary_digest,
            deployment_digest: payload.deployment_digest,
            r9_store_identity: payload.r9_store_identity,
        })
    }

    /// Test seam: verify against an explicit owner.
    #[cfg(test)]
    pub fn verify_with_owner(
        payload: &DeploymentPayload,
        signature: &[u8; 65],
        provider: &dyn CodeHashProvider,
        owner: Address,
    ) -> Option<Self> {
        Self::verify_inner(payload, signature, provider, owner)
    }

    /// The attested executor address.
    pub const fn executor(&self) -> Address {
        self.executor
    }

    /// The attested code hash.
    pub const fn code_hash(&self) -> B256 {
        self.code_hash
    }

    /// The attested build/binary digest.
    pub const fn binary_digest(&self) -> B256 {
        self.binary_digest
    }

    /// The attested deployment digest.
    pub const fn deployment_digest(&self) -> B256 {
        self.deployment_digest
    }

    /// The bound R9 claim-store identity.
    pub const fn r9_store_identity(&self) -> StoreIdentity {
        self.r9_store_identity
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arm::testkit as tk;

    fn campaign() -> CampaignId {
        CampaignId::new([0x0Au8; 32])
    }

    #[test]
    fn g7_positive_and_negatives() {
        let now = 1_000;
        let attest = tk::g7(campaign(), now + 100, now);
        assert!(attest.covers(campaign()));
        assert_eq!(attest.expiry(), now + 100);

        // Expiry not strictly after now -> fail-closed.
        let payload = G7Payload { campaign_id: campaign(), g7_closure_epoch: 7, expiry_unix: now };
        let sig = tk::eip191_sign(&payload.preimage(), &tk::owner_key());
        assert!(G7Attestation::verify_with_owner(&payload, &sig, now, tk::owner_address()).is_none());

        // Field mutation: a signature over a different payload does not verify.
        let other = G7Payload { campaign_id: campaign(), g7_closure_epoch: 9, expiry_unix: now + 100 };
        let sig_other = tk::eip191_sign(&other.preimage(), &tk::owner_key());
        let target = G7Payload { campaign_id: campaign(), g7_closure_epoch: 7, expiry_unix: now + 100 };
        assert!(
            G7Attestation::verify_with_owner(&target, &sig_other, now, tk::owner_address()).is_none()
        );

        // Wrong owner.
        let good = tk::eip191_sign(&target.preimage(), &tk::owner_key());
        let wrong = alloy_primitives::Address::repeat_byte(0xEE);
        assert!(G7Attestation::verify_with_owner(&target, &good, now, wrong).is_none());

        // Production `verify` fails closed (dependency OWNER is None).
        assert!(G7Attestation::verify(&target, &good, now).is_none());
    }

    #[test]
    fn live_positive_and_window() {
        let now = 5_000;
        let attest = tk::live(campaign(), now + 10, now);
        assert!(attest.covers(campaign()));

        // Before window start.
        let payload = LiveRunPayload { campaign_id: campaign(), window_start: now + 1, expiry_unix: now + 10 };
        let sig = tk::eip191_sign(&payload.preimage(), &tk::owner_key());
        assert!(
            LiveRunAttestation::verify_with_owner(&payload, &sig, now, tk::owner_address()).is_none()
        );
        // At/after expiry.
        let payload2 = LiveRunPayload { campaign_id: campaign(), window_start: 0, expiry_unix: now };
        let sig2 = tk::eip191_sign(&payload2.preimage(), &tk::owner_key());
        assert!(
            LiveRunAttestation::verify_with_owner(&payload2, &sig2, now, tk::owner_address()).is_none()
        );
    }

    #[test]
    fn deploy_positive_and_codehash_binding() {
        let code_hash = B256::repeat_byte(0x33);
        let provider = tk::FakeProvider { code_hash, block: 100, fail: false };
        let store = StoreIdentity::new([0x55u8; 32]);
        let evidence = tk::deployment(
            &provider,
            tk::EXECUTOR,
            code_hash,
            B256::repeat_byte(0x01),
            B256::repeat_byte(0x02),
            store,
        );
        assert_eq!(evidence.executor(), tk::EXECUTOR);
        assert_eq!(evidence.code_hash(), code_hash);
        assert_eq!(evidence.r9_store_identity(), store);

        // On-chain code hash mismatch -> None.
        let wrong_chain = tk::FakeProvider { code_hash: B256::repeat_byte(0x99), block: 100, fail: false };
        let payload = DeploymentPayload {
            chain_id: tk::CHAIN_ID,
            executor: tk::EXECUTOR,
            code_hash,
            binary_digest: B256::repeat_byte(0x01),
            deployment_digest: B256::repeat_byte(0x02),
            r9_store_identity: store,
        };
        let sig = tk::eip191_sign(&payload.preimage(), &tk::owner_key());
        assert!(
            DeploymentEvidence::verify_with_owner(&payload, &sig, &wrong_chain, tk::owner_address())
                .is_none()
        );
        // Provider failure -> None.
        let failing = tk::FakeProvider { code_hash, block: 0, fail: true };
        assert!(
            DeploymentEvidence::verify_with_owner(&payload, &sig, &failing, tk::owner_address())
                .is_none()
        );
    }

    #[test]
    fn deploy_non_base_chain_is_none() {
        // A deployment attested for a non-Base chain must never verify.
        let code_hash = B256::repeat_byte(0x33);
        let provider = tk::FakeProvider { code_hash, block: 100, fail: false };
        let store = StoreIdentity::new([0x55u8; 32]);
        let payload = DeploymentPayload {
            chain_id: 1, // Ethereum mainnet, NOT Base.
            executor: tk::EXECUTOR,
            code_hash,
            binary_digest: B256::repeat_byte(0x01),
            deployment_digest: B256::repeat_byte(0x02),
            r9_store_identity: store,
        };
        let sig = tk::eip191_sign(&payload.preimage(), &tk::owner_key());
        assert!(
            DeploymentEvidence::verify_with_owner(&payload, &sig, &provider, tk::owner_address())
                .is_none()
        );
    }

    #[test]
    fn deploy_domain_separation() {
        // A G7 signature must not verify a deployment payload (different domain +
        // preimage shape).
        let store = StoreIdentity::new([0x55u8; 32]);
        let g7_payload =
            G7Payload { campaign_id: campaign(), g7_closure_epoch: 7, expiry_unix: 9_999 };
        let g7_sig = tk::eip191_sign(&g7_payload.preimage(), &tk::owner_key());
        let code_hash = B256::repeat_byte(0x33);
        let provider = tk::FakeProvider { code_hash, block: 100, fail: false };
        let deploy_payload = DeploymentPayload {
            chain_id: tk::CHAIN_ID,
            executor: tk::EXECUTOR,
            code_hash,
            binary_digest: B256::repeat_byte(0x01),
            deployment_digest: B256::repeat_byte(0x02),
            r9_store_identity: store,
        };
        assert!(
            DeploymentEvidence::verify_with_owner(
                &deploy_payload,
                &g7_sig,
                &provider,
                tk::owner_address()
            )
            .is_none()
        );
    }

    #[test]
    fn suppression_clear_and_rollback() {
        let dir = tk::TempDir::new("supp");
        let epoch_store = tk::epoch_store(&dir.path);

        // Suppressed -> None.
        let path = tk::write_suppression_file(&dir.path, 5, true);
        let file = SuppressionFileStore::new(&path);
        assert!(SubmitSuppressionClear::read(&file, &epoch_store).is_none());

        // Not suppressed, epoch 5 -> Some, high-water now 5.
        let path = tk::write_suppression_file(&dir.path, 5, false);
        let file = SuppressionFileStore::new(&path);
        let clear = SubmitSuppressionClear::read(&file, &epoch_store).expect("clear");
        assert_eq!(clear.epoch(), 5);

        // Rollback to epoch 3 -> None (3 < high-water 5).
        let path = tk::write_suppression_file(&dir.path, 3, false);
        let file = SuppressionFileStore::new(&path);
        assert!(SubmitSuppressionClear::read(&file, &epoch_store).is_none());

        // Forward to epoch 6 -> Some.
        let path = tk::write_suppression_file(&dir.path, 6, false);
        let file = SuppressionFileStore::new(&path);
        assert!(SubmitSuppressionClear::read(&file, &epoch_store).is_some());
    }

    #[test]
    fn suppression_lock_present_is_fail_closed() {
        let dir = tk::TempDir::new("supp-lock");
        let epoch_store = tk::epoch_store(&dir.path);
        // A valid non-suppressed file...
        let path = tk::write_suppression_file(&dir.path, 5, false);
        let file = SuppressionFileStore::new(&path);
        assert!(SubmitSuppressionClear::read(&file, &epoch_store).is_some());
        // ...but with the writer lock present, the clear is refused fail-closed.
        let mut lock = path.into_os_string();
        lock.push(".lock");
        std::fs::write(std::path::PathBuf::from(lock), b"").unwrap();
        assert!(SubmitSuppressionClear::read(&file, &epoch_store).is_none());
    }

    #[test]
    fn suppression_file_parse_fail_closed() {
        let dir = tk::TempDir::new("suppparse");
        let path = dir.path.join("bad.json");
        std::fs::write(&path, b"{\"version\":2,\"epoch\":1,\"suppressed\":false}").unwrap();
        assert!(SuppressionFileStore::new(&path).read_fresh().is_none());
        std::fs::write(&path, b"not json").unwrap();
        assert!(SuppressionFileStore::new(&path).read_fresh().is_none());
        // Missing file.
        assert!(SuppressionFileStore::new(dir.path.join("nope.json")).read_fresh().is_none());
    }
}
