//! B1 safety interlocks: closed-by-default risk guards + verify-only arming.
//!
//! This module is keyless and SAFE. It contains, and only contains:
//!   * pure decision functions (`per_tx_cap`, `drawdown_floor`, `kill_switch`,
//!     `submit_gate`) that are closed-by-default and total (no silent Allow);
//!   * a fail-closed `ArmedCriteria` loader that VERIFIES (recovers) an owner
//!     signature over the G4 prereg criteria — it never signs and never holds a
//!     private key;
//!   * a latched kill-state store with atomic persistence and reset anti-replay.
//!
//! It holds ZERO submission/signing/egress capability. Owner-signature handling
//! is recover-only (EIP-191 `recover_address_from_msg`); the trust root is a
//! compile-time immutable address that is UNSET in B1, so production arming is
//! structurally impossible (`is_armed()` is always false) until an owner G4
//! signature exists. Enforcement wiring (blocking real sends) is B3; this rung
//! only computes decisions.

use std::{
    fs::{self, File},
    io::Write,
    path::PathBuf,
};

use alloy_primitives::{Address, Signature, U256};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Compile-time trust anchors (immutable; the B1 owner address is unset).
// ---------------------------------------------------------------------------

/// SHA-256 of the canonical G4 criteria payload (base-mev prereg v2 §1, anchored
/// `^# ===== BEGIN/END CANONICAL CRITERIA` bytes @ [`EXPECTED_CRITERIA_COMMIT`]).
pub const CRITERIA_SHA: [u8; 32] = [
    0x1b, 0xed, 0x38, 0xe1, 0x21, 0xee, 0x4a, 0xe4, 0x2d, 0x79, 0x5b, 0x93, 0x69, 0x5b, 0x58, 0x3e,
    0x4a, 0x7a, 0x1f, 0x0a, 0x09, 0x99, 0xa8, 0x45, 0x09, 0x12, 0xbc, 0xbb, 0x7b, 0x51, 0xdd, 0x60,
];

/// Expected Git source-commit (SHA-1, 40 lowercase hex) the criteria are anchored to.
pub const EXPECTED_CRITERIA_COMMIT: &str = "4f789f2e85a9dfdaff990d505b3793a4fa23a476";

/// Raw 20-byte form of [`EXPECTED_CRITERIA_COMMIT`].
const EXPECTED_CRITERIA_COMMIT_BYTES: [u8; 20] = [
    0x4f, 0x78, 0x9f, 0x2e, 0x85, 0xa9, 0xdf, 0xda, 0xff, 0x99, 0x0d, 0x50, 0x5b, 0x37, 0x93, 0xa4,
    0xfa, 0x23, 0xa4, 0x76,
];

/// Expected criteria schema version.
pub const EXPECTED_CRITERIA_VERSION: &str = "2.0.0";

/// Domain-separator prefix for the owner arm attestation (byte-exact ASCII).
const ARM_DOMAIN_PREFIX: &str = "base-mev:p2-prereg-v2:arm:";

/// Domain-separator prefix for the owner kill-reset attestation (byte-exact ASCII).
const KILLRESET_DOMAIN_PREFIX: &str = "base-mev:p2-killreset:";

/// Owner attestation address (trust root). UNSET in B1 (G4 unsigned): production
/// arming and kill-reset are structurally impossible until the owner G4 signature
/// exists, at which point pinning the real address is a deliberate code change +
/// rebuild + review — never a mutable config toggle.
#[cfg(not(test))]
pub const OWNER_ATTEST_ADDRESS: Option<Address> = None;

/// Test-only trust root: pins the well-known first Anvil address so the
/// verify-only paths can be exercised with precomputed (never in-process)
/// signature fixtures. This override exists solely under `#[cfg(test)]`.
#[cfg(test)]
pub const OWNER_ATTEST_ADDRESS: Option<Address> = Some(TEST_OWNER_ADDRESS);

/// Address of the offline test keypair used to precompute signature fixtures.
#[cfg(test)]
const TEST_OWNER_ADDRESS: Address = Address::new([
    0xf3, 0x9f, 0xd6, 0xe5, 0x1a, 0xad, 0x88, 0xf6, 0xf4, 0xce, 0x6a, 0xb8, 0x82, 0x72, 0x79, 0xcf,
    0xff, 0xb9, 0x22, 0x66,
]);

// ---------------------------------------------------------------------------
// Decision types.
// ---------------------------------------------------------------------------

/// Outcome of a single risk guard. There is no silent-default variant: every
/// input maps to `Allow`, `Reject`, or `Halt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The guard's precondition is satisfied.
    Allow,
    /// The candidate is rejected (bounded, non-halting), e.g. it exceeds a cap.
    Reject(GuardReason),
    /// The pipeline must halt (fail-closed), e.g. an incomplete or breached floor.
    Halt(GuardReason),
}

/// Reason attached to a non-`Allow` guard outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardReason {
    /// Candidate size strictly exceeds the armed per-tx capital cap.
    PerTxCapExceeded,
    /// Complete realized loss strictly exceeds the armed drawdown floor.
    DrawdownFloorBreached,
    /// Drawdown accounting is not authoritative-complete (pending/missing/error).
    DrawdownIncomplete,
    /// Kill state is not a verified `Clear` (unknown or engaged).
    KillNotClear,
}

/// Master submit decision. `Open` requires the full conjunction of every guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitDecision {
    /// Every predicate is satisfied; submission is permitted by this rung.
    Open,
    /// Submission is closed (the default for any unmet/unknown/error predicate).
    Closed(ClosedReason),
}

/// Reason a [`SubmitDecision`] is `Closed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosedReason {
    /// Criteria are not armed (loader failed or owner address unset).
    NotArmed,
    /// Armed criteria version/SHA do not match the compile-time runtime pin.
    CriteriaPinMismatch,
    /// Candidate size exceeds the armed per-tx cap.
    PerTxCapExceeded,
    /// Complete realized loss exceeds the armed drawdown floor.
    DrawdownFloorBreached,
    /// Drawdown accounting is incomplete.
    DrawdownIncomplete,
    /// Kill state is not a verified `Clear`.
    KillNotClear,
}

// ---------------------------------------------------------------------------
// Guard inputs.
// ---------------------------------------------------------------------------

/// Completeness-typed drawdown accounting input. Only `Complete` is eligible to
/// produce `Allow`; unresolved/missing/error are treated as halts, never as a
/// realized loss of zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawdownInput {
    /// Authoritative, settled cumulative realized loss.
    Complete {
        /// Cumulative realized loss in wei (losses only; profit does not offset).
        cumulative_realized_loss_wei: U256,
        /// Where the settled figure came from.
        provenance: LossProvenance,
    },
    /// Some prior submission is still unresolved / not settled.
    PendingOrUnresolved,
    /// The accounting source is absent.
    Missing,
    /// The accounting source errored.
    Error,
}

/// Provenance of a `Complete` drawdown figure. Both variants are authoritative;
/// the value is informational and never relaxes the guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LossProvenance {
    /// Settled from on-chain realized results.
    OnchainRealized,
    /// Settled from the independent replay oracle.
    ReplaySettled,
}

/// Three-state kill signal. Only an explicit, verified `Clear` permits work; an
/// `Unknown` (cold start, load failure, absence) fails closed exactly like
/// `Engaged`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillState {
    /// State could not be established (fail-closed).
    Unknown,
    /// Verified clear; `verified_at` is the engagement epoch that was cleared.
    Clear {
        /// Engagement epoch the owner reset cleared.
        verified_at: u64,
    },
    /// Latched engaged with the triggering reason.
    Engaged {
        /// Why the kill switch engaged.
        reason: KillReason,
    },
}

/// Automatic kill-switch triggers (prereg v2 §2/§3, M3 P0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KillReason {
    /// Key load or signature failure (P0 (c): engage immediately, no retry).
    KeyOrSignatureFailure,
    /// At least one strict-minOut principal-loss sample (prereg §3: immediate halt).
    StrictMinOutPrincipalLoss,
    /// Cumulative drawdown floor breach.
    DrawdownFloorBreach,
}

// ---------------------------------------------------------------------------
// Pure decision functions (§2: pure, side-effect free, total).
// ---------------------------------------------------------------------------

/// Per-tx capital cap. Strictly-greater-than the armed cap rejects; at or under
/// the cap allows.
pub fn per_tx_cap(amount_in_wei: U256, armed: &ArmedCriteria) -> Decision {
    if amount_in_wei > armed.per_tx_cap_wei {
        Decision::Reject(GuardReason::PerTxCapExceeded)
    } else {
        Decision::Allow
    }
}

/// Drawdown floor. Only `Complete` accounting can allow; a complete loss strictly
/// above the floor halts, and any non-complete input halts as incomplete.
pub fn drawdown_floor(loss: DrawdownInput, armed: &ArmedCriteria) -> Decision {
    match loss {
        DrawdownInput::Complete { cumulative_realized_loss_wei, .. } => {
            if cumulative_realized_loss_wei > armed.drawdown_floor_wei {
                Decision::Halt(GuardReason::DrawdownFloorBreached)
            } else {
                Decision::Allow
            }
        }
        DrawdownInput::PendingOrUnresolved | DrawdownInput::Missing | DrawdownInput::Error => {
            Decision::Halt(GuardReason::DrawdownIncomplete)
        }
    }
}

/// Kill switch. Only a verified `Clear` allows; `Unknown` and `Engaged` both halt.
pub fn kill_switch(state: &KillState) -> Decision {
    match state {
        KillState::Clear { .. } => Decision::Allow,
        KillState::Unknown | KillState::Engaged { .. } => Decision::Halt(GuardReason::KillNotClear),
    }
}

/// Context assembled by the (future) enforcement caller for the master gate.
#[derive(Debug)]
pub struct SubmitContext<'a> {
    /// Loaded arming state (only a validly-loaded artifact reports `is_armed()`).
    pub armed: &'a ArmedCriteria,
    /// Candidate input size in wei.
    pub amount_in_wei: U256,
    /// Drawdown accounting input.
    pub drawdown: DrawdownInput,
    /// Kill state.
    pub kill: KillState,
}

/// Master gate (closed-by-default). Returns `Open` only when EVERY predicate is
/// satisfied: armed, version/SHA match the compile-time runtime pin, size within
/// cap, drawdown complete-and-under-floor, and kill verified clear. Any unmet,
/// unknown, incomplete, or error predicate returns `Closed`.
pub fn submit_gate(ctx: SubmitContext<'_>) -> SubmitDecision {
    if !ctx.armed.is_armed() {
        return SubmitDecision::Closed(ClosedReason::NotArmed);
    }
    // Defense in depth: even an armed artifact must match the compiled-in pins.
    if ctx.armed.criteria_version != EXPECTED_CRITERIA_VERSION
        || ctx.armed.criteria_sha256 != CRITERIA_SHA
    {
        return SubmitDecision::Closed(ClosedReason::CriteriaPinMismatch);
    }
    if per_tx_cap(ctx.amount_in_wei, ctx.armed) != Decision::Allow {
        return SubmitDecision::Closed(ClosedReason::PerTxCapExceeded);
    }
    match drawdown_floor(ctx.drawdown, ctx.armed) {
        Decision::Allow => {}
        Decision::Halt(GuardReason::DrawdownFloorBreached) => {
            return SubmitDecision::Closed(ClosedReason::DrawdownFloorBreached);
        }
        _ => return SubmitDecision::Closed(ClosedReason::DrawdownIncomplete),
    }
    if kill_switch(&ctx.kill) != Decision::Allow {
        return SubmitDecision::Closed(ClosedReason::KillNotClear);
    }
    SubmitDecision::Open
}

// ---------------------------------------------------------------------------
// ArmedCriteria loader = B1-READY code (fail-closed core).
// ---------------------------------------------------------------------------

/// The signed prereg arming artifact. The owner signature is VERIFIED (recovered)
/// only; no signing occurs anywhere in this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CriteriaArtifact {
    /// Canonical criteria payload bytes (prereg v2 §1). Its SHA-256 is pinned.
    pub canonical_payload: Vec<u8>,
    /// Claimed source-commit SHA-1 as 40 lowercase hex (no `0x`).
    pub criteria_source_commit_sha: String,
    /// Owner EIP-191 signature over the domain-separated arm message (r‖s‖v).
    pub owner_signature: [u8; 65],
}

/// Why a load produced `Unarmed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnarmedReason {
    /// `sha256(canonical_payload)` did not equal the pinned criteria SHA.
    CriteriaShaMismatch,
    /// Source-commit was not exactly 40 lowercase hex (rejects 39/41/64/`0x`/upper).
    SourceCommitFormatInvalid,
    /// Source-commit did not equal the expected commit.
    SourceCommitMismatch,
    /// Criteria version parsed from the payload did not match the expected version.
    CriteriaVersionMismatch,
    /// A required criteria value was missing or unparseable.
    CriteriaValueParseFailed,
    /// The trust-root owner address is unset (B1 default).
    OwnerAddressUnset,
    /// The 65-byte signature could not be parsed (bad parity byte).
    SignatureMalformed,
    /// Public-key recovery from the signature failed.
    SignatureRecoveryFailed,
    /// Recovered signer did not equal the trust-root owner address.
    OwnerSignatureMismatch,
}

/// Loaded arming state. `is_armed()` is true only when the loader validated every
/// check against the compile-time trust root; otherwise guard values are zero and
/// `is_armed()` is false (fail-closed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArmedCriteria {
    armed: bool,
    unarmed_reason: Option<UnarmedReason>,
    criteria_version: String,
    criteria_sha256: [u8; 32],
    per_tx_cap_wei: U256,
    drawdown_floor_wei: U256,
    hot_wallet_cap_wei: U256,
}

impl ArmedCriteria {
    /// Loads and validates an arming artifact against the compile-time trust root.
    /// Any failure yields a fail-closed unarmed value.
    pub fn load(artifact: &CriteriaArtifact) -> Self {
        Self::load_with_owner(artifact, OWNER_ATTEST_ADDRESS)
    }

    /// Loads against an explicit trust root. The public [`load`](Self::load)
    /// hardwires the compile-time [`OWNER_ATTEST_ADDRESS`]; this seam exists so the
    /// unset-owner branch stays exercisable without a mutable trust root.
    fn load_with_owner(artifact: &CriteriaArtifact, owner: Option<Address>) -> Self {
        // (1) payload SHA must equal the compile-time pin.
        let payload_sha: [u8; 32] = Sha256::digest(&artifact.canonical_payload).into();
        if payload_sha != CRITERIA_SHA {
            return Self::unarmed(UnarmedReason::CriteriaShaMismatch);
        }
        // (3a) source-commit format + value (a separate, unsigned artifact field).
        let Some(commit_bytes) = parse_commit_sha(&artifact.criteria_source_commit_sha) else {
            return Self::unarmed(UnarmedReason::SourceCommitFormatInvalid);
        };
        if commit_bytes != EXPECTED_CRITERIA_COMMIT_BYTES {
            return Self::unarmed(UnarmedReason::SourceCommitMismatch);
        }
        // (4 + 3b) parse values and version from the SHA-bound payload bytes.
        let Some(parsed) = parse_criteria_payload(&artifact.canonical_payload) else {
            return Self::unarmed(UnarmedReason::CriteriaValueParseFailed);
        };
        if parsed.criteria_version != EXPECTED_CRITERIA_VERSION {
            return Self::unarmed(UnarmedReason::CriteriaVersionMismatch);
        }
        // (2) owner signature VERIFY (recover-only) against the compile-time root.
        let Some(owner) = owner else {
            return Self::unarmed(UnarmedReason::OwnerAddressUnset);
        };
        let message =
            arm_message(&parsed.criteria_version, &payload_sha, &artifact.criteria_source_commit_sha);
        let Ok(signature) = Signature::from_raw_array(&artifact.owner_signature) else {
            return Self::unarmed(UnarmedReason::SignatureMalformed);
        };
        let Ok(recovered) = signature.recover_address_from_msg(message.as_bytes()) else {
            return Self::unarmed(UnarmedReason::SignatureRecoveryFailed);
        };
        if recovered != owner {
            return Self::unarmed(UnarmedReason::OwnerSignatureMismatch);
        }
        Self {
            armed: true,
            unarmed_reason: None,
            criteria_version: parsed.criteria_version,
            criteria_sha256: payload_sha,
            per_tx_cap_wei: parsed.per_tx_cap_wei,
            drawdown_floor_wei: parsed.drawdown_floor_wei,
            hot_wallet_cap_wei: parsed.hot_wallet_cap_wei,
        }
    }

    fn unarmed(reason: UnarmedReason) -> Self {
        Self {
            armed: false,
            unarmed_reason: Some(reason),
            criteria_version: String::new(),
            criteria_sha256: [0u8; 32],
            per_tx_cap_wei: U256::ZERO,
            drawdown_floor_wei: U256::ZERO,
            hot_wallet_cap_wei: U256::ZERO,
        }
    }

    /// Whether arming validation succeeded.
    pub fn is_armed(&self) -> bool {
        self.armed
    }

    /// The failure reason when not armed.
    pub fn unarmed_reason(&self) -> Option<UnarmedReason> {
        self.unarmed_reason
    }

    /// Armed per-tx capital cap (wei); zero when unarmed.
    pub fn per_tx_cap_wei(&self) -> U256 {
        self.per_tx_cap_wei
    }

    /// Armed drawdown floor (wei); zero when unarmed.
    pub fn drawdown_floor_wei(&self) -> U256 {
        self.drawdown_floor_wei
    }

    /// Armed hot-wallet funding cap (wei); zero when unarmed.
    pub fn hot_wallet_cap_wei(&self) -> U256 {
        self.hot_wallet_cap_wei
    }

    /// Armed criteria version; empty when unarmed.
    pub fn criteria_version(&self) -> &str {
        &self.criteria_version
    }

    /// Armed criteria SHA-256; zero when unarmed.
    pub fn criteria_sha256(&self) -> [u8; 32] {
        self.criteria_sha256
    }

    /// Constructs an armed value directly for guard/gate boundary tests, bypassing
    /// crypto. Never available outside `#[cfg(test)]`.
    #[cfg(test)]
    fn armed_for_test(
        per_tx_cap_wei: U256,
        drawdown_floor_wei: U256,
        criteria_version: &str,
        criteria_sha256: [u8; 32],
    ) -> Self {
        Self {
            armed: true,
            unarmed_reason: None,
            criteria_version: criteria_version.to_owned(),
            criteria_sha256,
            per_tx_cap_wei,
            drawdown_floor_wei,
            hot_wallet_cap_wei: U256::ZERO,
        }
    }
}

/// Parsed criteria values bound to the SHA-pinned payload.
struct ParsedCriteria {
    criteria_version: String,
    per_tx_cap_wei: U256,
    drawdown_floor_wei: U256,
    hot_wallet_cap_wei: U256,
}

/// Parses the fixed `key: value` / `# comment` grammar. Returns `None` if any
/// required key is absent or a wei value is not a base-10 integer.
fn parse_criteria_payload(payload: &[u8]) -> Option<ParsedCriteria> {
    let text = std::str::from_utf8(payload).ok()?;
    let mut criteria_version: Option<String> = None;
    let mut per_tx_cap_wei: Option<U256> = None;
    let mut drawdown_floor_wei: Option<U256> = None;
    let mut hot_wallet_cap_wei: Option<U256> = None;

    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "criteria_version" => criteria_version = Some(value.to_owned()),
            "per_tx_cap_wei" => per_tx_cap_wei = Some(U256::from_str_radix(value, 10).ok()?),
            "drawdown_floor_wei" => drawdown_floor_wei = Some(U256::from_str_radix(value, 10).ok()?),
            "hot_wallet_cap_wei" => hot_wallet_cap_wei = Some(U256::from_str_radix(value, 10).ok()?),
            _ => {}
        }
    }

    Some(ParsedCriteria {
        criteria_version: criteria_version?,
        per_tx_cap_wei: per_tx_cap_wei?,
        drawdown_floor_wei: drawdown_floor_wei?,
        hot_wallet_cap_wei: hot_wallet_cap_wei?,
    })
}

/// Parses a source-commit SHA-1: accepts ONLY exactly 40 lowercase hex chars
/// (rejecting 39/41/64-length, `0x`-prefix, and uppercase). Returns the 20 bytes.
fn parse_commit_sha(input: &str) -> Option<[u8; 20]> {
    let bytes = input.as_bytes();
    if bytes.len() != 40 {
        return None;
    }
    let mut out = [0u8; 20];
    for (index, slot) in out.iter_mut().enumerate() {
        let hi = lower_hex_nibble(bytes[2 * index])?;
        let lo = lower_hex_nibble(bytes[2 * index + 1])?;
        *slot = (hi << 4) | lo;
    }
    Some(out)
}

/// Maps a single ASCII byte to its lowercase-hex nibble; `None` for uppercase or
/// any non `[0-9a-f]` byte.
fn lower_hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

const LOWER_HEX: &[u8; 16] = b"0123456789abcdef";

/// Appends `bytes` as fixed-width lowercase hex.
fn push_lower_hex(out: &mut String, bytes: &[u8]) {
    for &byte in bytes {
        out.push(LOWER_HEX[(byte >> 4) as usize] as char);
        out.push(LOWER_HEX[(byte & 0x0f) as usize] as char);
    }
}

/// Builds the byte-exact domain-separated arm message:
/// `base-mev:p2-prereg-v2:arm:<version>:<hex64(sha256)>:<hex40(commit)>`.
fn arm_message(version: &str, payload_sha: &[u8; 32], commit_hex: &str) -> String {
    let mut message = String::with_capacity(ARM_DOMAIN_PREFIX.len() + version.len() + 1 + 64 + 1 + 40);
    message.push_str(ARM_DOMAIN_PREFIX);
    message.push_str(version);
    message.push(':');
    push_lower_hex(&mut message, payload_sha);
    message.push(':');
    message.push_str(commit_hex);
    message
}

// ---------------------------------------------------------------------------
// Kill-state store (atomic persistence + reset anti-replay).
// ---------------------------------------------------------------------------

/// Owner reset attestation. Verified (recover-only) against the trust root and
/// bound to the current engagement epoch (anti-replay).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetAttestation {
    /// Engagement epoch this reset is authorized for.
    pub engagement_epoch: u64,
    /// Owner-chosen nonce bound into the signed message.
    pub nonce: u64,
    /// Owner EIP-191 signature over the killreset domain message (r‖s‖v).
    pub signature: [u8; 65],
}

/// Kill-state persistence contract. Transitions are split: `engage` (automatic,
/// latching) and `owner_reset` (verified). There is no generic `persist(Clear)`.
pub trait KillStateStore {
    /// Loads the current kill state, failing closed to `Unknown` on any
    /// absence/corruption/verification failure.
    fn load(&self) -> KillState;

    /// Latches engaged with `reason`, incrementing the monotonic epoch.
    fn engage(&self, reason: KillReason) -> Result<(), KillStoreError>;

    /// Clears an engaged latch iff the attestation verifies for the current epoch.
    /// A persistence failure leaves the latch engaged.
    fn owner_reset(&self, attestation: &ResetAttestation) -> Result<(), KillStoreError>;
}

/// Errors from kill-state persistence and reset verification.
#[derive(Debug, Error)]
pub enum KillStoreError {
    /// Atomic persistence I/O failed (temp write / fsync / rename).
    #[error("kill-state persistence failed")]
    Io,
    /// The trust-root owner address is unset.
    #[error("owner attestation address is unset")]
    OwnerAddressUnset,
    /// Reset attestation epoch does not equal the current engaged epoch.
    #[error("reset attestation epoch does not match the engaged epoch")]
    ResetEpochMismatch,
    /// Reset signature could not be parsed.
    #[error("reset attestation signature is malformed")]
    SignatureMalformed,
    /// Reset signature recovery failed.
    #[error("reset attestation signature recovery failed")]
    SignatureRecoveryFailed,
    /// Recovered reset signer is not the trust-root owner.
    #[error("reset attestation signer is not the owner")]
    OwnerSignatureMismatch,
    /// Reset requested while not engaged.
    #[error("kill state is not engaged")]
    NotEngaged,
}

/// Minimal purpose-built kill-state file store. Writes are atomic (temp → fsync →
/// rename) and every load path fails closed to `Unknown`.
#[derive(Debug, Clone)]
pub struct FileKillStateStore {
    path: PathBuf,
}

/// On-disk record. A `Clear` is stored with its full attestation (never a bare
/// flag) so restart re-verification can reject tampering.
#[derive(Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum PersistedRecord {
    Engaged { epoch: u64, reason: KillReason },
    Clear { epoch: u64, reset: PersistedReset },
}

/// Persisted reset attestation (signature carried as lowercase hex).
#[derive(Serialize, Deserialize)]
struct PersistedReset {
    engagement_epoch: u64,
    nonce: u64,
    signature_hex: String,
}

impl FileKillStateStore {
    /// Creates a store backed by `path` (a node-config location).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Reads and deserializes the record; `None` on absence/unreadable/torn/corrupt.
    fn read_record(&self) -> Option<PersistedRecord> {
        let bytes = fs::read(&self.path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Current monotonic epoch (0 when no valid record exists).
    fn current_epoch(&self) -> u64 {
        match self.read_record() {
            Some(PersistedRecord::Engaged { epoch, .. } | PersistedRecord::Clear { epoch, .. }) => {
                epoch
            }
            None => 0,
        }
    }

    /// Atomically replaces the record: temp file → fsync → rename (+ best-effort
    /// directory fsync). Any failure leaves the previous record intact.
    fn atomic_write(&self, record: &PersistedRecord) -> Result<(), KillStoreError> {
        let bytes = serde_json::to_vec(record).map_err(|_| KillStoreError::Io)?;
        let mut temp = self.path.clone().into_os_string();
        temp.push(".tmp.");
        temp.push(unique_suffix());
        let temp = PathBuf::from(temp);

        let write = (|| -> std::io::Result<()> {
            let mut file = File::create(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            Ok(())
        })();
        if write.is_err() {
            let _ = fs::remove_file(&temp);
            return Err(KillStoreError::Io);
        }
        if fs::rename(&temp, &self.path).is_err() {
            let _ = fs::remove_file(&temp);
            return Err(KillStoreError::Io);
        }
        if let Some(parent) = self.path.parent() {
            if let Ok(dir) = File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    }

    /// Verifies a reset attestation against `expected_epoch` and the trust root.
    fn verify_reset(
        &self,
        attestation: &ResetAttestation,
        expected_epoch: u64,
    ) -> Result<(), KillStoreError> {
        if attestation.engagement_epoch != expected_epoch {
            return Err(KillStoreError::ResetEpochMismatch);
        }
        let Some(owner) = OWNER_ATTEST_ADDRESS else {
            return Err(KillStoreError::OwnerAddressUnset);
        };
        let message = killreset_message(attestation.engagement_epoch, attestation.nonce);
        let signature = Signature::from_raw_array(&attestation.signature)
            .map_err(|_| KillStoreError::SignatureMalformed)?;
        let recovered = signature
            .recover_address_from_msg(message.as_bytes())
            .map_err(|_| KillStoreError::SignatureRecoveryFailed)?;
        if recovered != owner {
            return Err(KillStoreError::OwnerSignatureMismatch);
        }
        Ok(())
    }
}

impl KillStateStore for FileKillStateStore {
    fn load(&self) -> KillState {
        match self.read_record() {
            None => KillState::Unknown,
            Some(PersistedRecord::Engaged { reason, .. }) => KillState::Engaged { reason },
            Some(PersistedRecord::Clear { epoch, reset }) => {
                let Some(signature) = decode_signature_hex(&reset.signature_hex) else {
                    return KillState::Unknown;
                };
                let attestation = ResetAttestation {
                    engagement_epoch: reset.engagement_epoch,
                    nonce: reset.nonce,
                    signature,
                };
                match self.verify_reset(&attestation, epoch) {
                    Ok(()) => KillState::Clear { verified_at: epoch },
                    Err(_) => KillState::Unknown,
                }
            }
        }
    }

    fn engage(&self, reason: KillReason) -> Result<(), KillStoreError> {
        let next_epoch = self.current_epoch().saturating_add(1);
        self.atomic_write(&PersistedRecord::Engaged { epoch: next_epoch, reason })
    }

    fn owner_reset(&self, attestation: &ResetAttestation) -> Result<(), KillStoreError> {
        // Only an engaged latch may be reset.
        if !matches!(self.load(), KillState::Engaged { .. }) {
            return Err(KillStoreError::NotEngaged);
        }
        let current_epoch = self.current_epoch();
        // Anti-replay: the attestation must be bound to the current engaged epoch.
        self.verify_reset(attestation, current_epoch)?;
        let reset = PersistedReset::from(attestation);
        // Persistence failure keeps the latch engaged (the old record is intact).
        self.atomic_write(&PersistedRecord::Clear { epoch: current_epoch, reset })
    }
}

impl From<&ResetAttestation> for PersistedReset {
    fn from(attestation: &ResetAttestation) -> Self {
        let mut signature_hex = String::with_capacity(130);
        push_lower_hex(&mut signature_hex, &attestation.signature);
        Self {
            engagement_epoch: attestation.engagement_epoch,
            nonce: attestation.nonce,
            signature_hex,
        }
    }
}

/// Builds the byte-exact killreset message: `base-mev:p2-killreset:<epoch>:<nonce>`.
fn killreset_message(engagement_epoch: u64, nonce: u64) -> String {
    let mut message = String::with_capacity(KILLRESET_DOMAIN_PREFIX.len() + 40);
    message.push_str(KILLRESET_DOMAIN_PREFIX);
    message.push_str(&engagement_epoch.to_string());
    message.push(':');
    message.push_str(&nonce.to_string());
    message
}

/// Decodes an exactly-130-char lowercase-hex string into a 65-byte signature.
fn decode_signature_hex(hex: &str) -> Option<[u8; 65]> {
    let bytes = hex.as_bytes();
    if bytes.len() != 130 {
        return None;
    }
    let mut out = [0u8; 65];
    for (index, slot) in out.iter_mut().enumerate() {
        let hi = lower_hex_nibble(bytes[2 * index])?;
        let lo = lower_hex_nibble(bytes[2 * index + 1])?;
        *slot = (hi << 4) | lo;
    }
    Some(out)
}

/// Process- and time-unique suffix for atomic temp files.
fn unique_suffix() -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let mut suffix = String::with_capacity(40);
    suffix.push_str(&pid.to_string());
    suffix.push('.');
    suffix.push_str(&nanos.to_string());
    suffix
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    // --- offline signature fixtures (precomputed; this module never signs) -----
    // Test keypair = well-known Anvil account #0
    //   key  0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
    //   addr 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 (= TEST_OWNER_ADDRESS)
    // Wrong key = Anvil account #1 (0x7099...79C8).
    // Each signature was produced offline with `cast wallet sign` over the exact
    // domain message; the loader/store only ever recover (verify) them.

    // K0 over base-mev:p2-prereg-v2:arm:2.0.0:<criteria_sha>:<expected_commit>
    const SIG_ARM_VALID: &str = "fa75de384d714e0974d4ee6457f92aeb3b79237434edd200446be8845e50a5530d86cf4abf3662a7c31b3881bb5d51df857734003c7f1ed2437fc5e8bca42bd31b";
    // K1 (wrong key) over the same valid arm message.
    const SIG_ARM_WRONG_KEY: &str = "4ff66dffe77faaef94afd529f71beabdc7e55d4d92a5881c7a9c3cd892b69abc100f7ab53db188ddc8a5c671390e891554e8193843a23d4e2f57f85c5fae53571b";
    // K0 over a wrong-domain (p2-prereg-v3) arm message.
    const SIG_ARM_WRONG_DOMAIN: &str = "0887d81d1d91def0e9cd921250b8b4b3b251b4a66451d9c734d853ad4b8f39f260f769dd7656fad2beb2688686ba5917ccaa48c979b2b3634735d89b26770c461c";
    // K0 over base-mev:p2-killreset:1:424242
    const SIG_RESET_EPOCH1: &str = "c4e4fec07c5dc52eddee17d3c5c4ba6d0801be20fb8c688bf016df0e95d480df6679248e725806ecff4e7636921f60cdbe75bc1d69ab44d2cb15f947c8bfcf201b";
    // K0 over base-mev:p2-killreset:2:424242
    const SIG_RESET_EPOCH2: &str = "ee5c79ef81539ebc292d84a9af594b31d5bf87d4d06574f307286fc6aec711f741f314197a1c926e72f553706ddadce7abcbfa41d43df5caa2c9058fded635601c";
    // K0 over a wrong-domain (p2-killresetX) reset message for epoch 1.
    const SIG_RESET_WRONG_DOMAIN: &str = "20cfdcae942bc0d4dcd64844d8a58b9d4cfde18ff4b21559710efc0faa73248b037c7dd36358df03dab96dcee1932d429d118b50a22218c7d0e5a0e8d7579b6f1b";

    const RESET_NONCE: u64 = 424242;

    // Embedded canonical payload = base-mev prereg v2 §1 @ EXPECTED_CRITERIA_COMMIT
    // (anchored `^# ===== BEGIN/END CANONICAL CRITERIA` bytes). Its SHA-256 is the
    // pinned CRITERIA_SHA; the build-time binding test asserts that below.
    const CANONICAL_PAYLOAD: &[u8] =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/resources/criteria_canonical_payload_v2.txt"));

    // Values carried by the canonical payload (bound to CRITERIA_SHA).
    const PAYLOAD_PER_TX_CAP_WEI: u64 = 630_000_000_000_000;
    const PAYLOAD_DRAWDOWN_FLOOR_WEI: u64 = 50_000_000_000_000_000;

    fn sig(hex: &str) -> [u8; 65] {
        decode_signature_hex(hex).expect("valid fixture signature hex")
    }

    fn valid_artifact() -> CriteriaArtifact {
        CriteriaArtifact {
            canonical_payload: CANONICAL_PAYLOAD.to_vec(),
            criteria_source_commit_sha: EXPECTED_CRITERIA_COMMIT.to_owned(),
            owner_signature: sig(SIG_ARM_VALID),
        }
    }

    fn armed() -> ArmedCriteria {
        let criteria = ArmedCriteria::load(&valid_artifact());
        assert!(criteria.is_armed(), "fixture must arm under the test trust root");
        criteria
    }

    fn under_floor_complete() -> DrawdownInput {
        DrawdownInput::Complete {
            cumulative_realized_loss_wei: U256::ZERO,
            provenance: LossProvenance::OnchainRealized,
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "b1-safety-{}-{}-{}",
            std::process::id(),
            tag,
            nanos
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn reset_attestation(engagement_epoch: u64, signature_hex: &str) -> ResetAttestation {
        ResetAttestation { engagement_epoch, nonce: RESET_NONCE, signature: sig(signature_hex) }
    }

    // ---------------------------------------------------------------------
    // (a) Decision-function boundaries (strict `>`).
    // ---------------------------------------------------------------------

    #[test]
    fn per_tx_cap_is_strict_greater_than() {
        let criteria = ArmedCriteria::armed_for_test(
            U256::from(1000u64),
            U256::ZERO,
            EXPECTED_CRITERIA_VERSION,
            CRITERIA_SHA,
        );
        assert_eq!(per_tx_cap(U256::from(999u64), &criteria), Decision::Allow);
        assert_eq!(per_tx_cap(U256::from(1000u64), &criteria), Decision::Allow);
        assert_eq!(
            per_tx_cap(U256::from(1001u64), &criteria),
            Decision::Reject(GuardReason::PerTxCapExceeded)
        );
    }

    #[test]
    fn drawdown_floor_is_strict_greater_than_for_complete() {
        let criteria = ArmedCriteria::armed_for_test(
            U256::ZERO,
            U256::from(1000u64),
            EXPECTED_CRITERIA_VERSION,
            CRITERIA_SHA,
        );
        let complete = |loss: u64| DrawdownInput::Complete {
            cumulative_realized_loss_wei: U256::from(loss),
            provenance: LossProvenance::ReplaySettled,
        };
        assert_eq!(drawdown_floor(complete(999), &criteria), Decision::Allow);
        assert_eq!(drawdown_floor(complete(1000), &criteria), Decision::Allow);
        assert_eq!(
            drawdown_floor(complete(1001), &criteria),
            Decision::Halt(GuardReason::DrawdownFloorBreached)
        );
    }

    #[test]
    fn kill_switch_allows_only_verified_clear() {
        assert_eq!(kill_switch(&KillState::Clear { verified_at: 7 }), Decision::Allow);
        assert_eq!(
            kill_switch(&KillState::Unknown),
            Decision::Halt(GuardReason::KillNotClear)
        );
        assert_eq!(
            kill_switch(&KillState::Engaged { reason: KillReason::DrawdownFloorBreach }),
            Decision::Halt(GuardReason::KillNotClear)
        );
    }

    // ---------------------------------------------------------------------
    // (b) Loader fail-closed for every branch.
    // ---------------------------------------------------------------------

    #[test]
    fn loader_arms_on_valid_artifact_and_binds_values() {
        let criteria = ArmedCriteria::load(&valid_artifact());
        assert!(criteria.is_armed());
        assert_eq!(criteria.unarmed_reason(), None);
        assert_eq!(criteria.criteria_version(), EXPECTED_CRITERIA_VERSION);
        assert_eq!(criteria.criteria_sha256(), CRITERIA_SHA);
        assert_eq!(criteria.per_tx_cap_wei(), U256::from(PAYLOAD_PER_TX_CAP_WEI));
        assert_eq!(criteria.drawdown_floor_wei(), U256::from(PAYLOAD_DRAWDOWN_FLOOR_WEI));
    }

    #[test]
    fn loader_unarmed_when_owner_address_unset() {
        // Exercises the compile-time B1 production branch (trust root None).
        let criteria = ArmedCriteria::load_with_owner(&valid_artifact(), None);
        assert!(!criteria.is_armed());
        assert_eq!(criteria.unarmed_reason(), Some(UnarmedReason::OwnerAddressUnset));
    }

    #[test]
    fn loader_unarmed_on_payload_tamper_sha_mismatch() {
        let mut artifact = valid_artifact();
        artifact.canonical_payload[0] ^= 0x01; // any change breaks the SHA pin
        let criteria = ArmedCriteria::load(&artifact);
        assert!(!criteria.is_armed());
        assert_eq!(criteria.unarmed_reason(), Some(UnarmedReason::CriteriaShaMismatch));
    }

    #[test]
    fn loader_unarmed_on_source_commit_mismatch() {
        let mut artifact = valid_artifact();
        artifact.criteria_source_commit_sha =
            "0000000000000000000000000000000000000000".to_owned();
        let criteria = ArmedCriteria::load(&artifact);
        assert!(!criteria.is_armed());
        assert_eq!(criteria.unarmed_reason(), Some(UnarmedReason::SourceCommitMismatch));
    }

    #[test]
    fn loader_unarmed_on_source_commit_format_violations() {
        for bad in [
            "4f789f2e85a9dfdaff990d505b3793a4fa23a47",    // 39 chars
            "4f789f2e85a9dfdaff990d505b3793a4fa23a4766",  // 41 chars
            "4f789f2e85a9dfdaff990d505b3793a4fa23a4764f789f2e85a9dfdaff990d505", // 64 chars
            "0x789f2e85a9dfdaff990d505b3793a4fa23a476",   // 0x-prefixed, length 40
            "4F789F2E85A9DFDAFF990D505B3793A4FA23A476",   // uppercase
        ] {
            let mut artifact = valid_artifact();
            artifact.criteria_source_commit_sha = bad.to_owned();
            let criteria = ArmedCriteria::load(&artifact);
            assert!(!criteria.is_armed(), "commit {bad} must not arm");
            assert_eq!(
                criteria.unarmed_reason(),
                Some(UnarmedReason::SourceCommitFormatInvalid),
                "commit {bad}"
            );
        }
    }

    #[test]
    fn loader_unarmed_on_wrong_owner_key() {
        let mut artifact = valid_artifact();
        artifact.owner_signature = sig(SIG_ARM_WRONG_KEY);
        let criteria = ArmedCriteria::load(&artifact);
        assert!(!criteria.is_armed());
        assert_eq!(criteria.unarmed_reason(), Some(UnarmedReason::OwnerSignatureMismatch));
    }

    #[test]
    fn loader_unarmed_on_wrong_domain_message() {
        let mut artifact = valid_artifact();
        artifact.owner_signature = sig(SIG_ARM_WRONG_DOMAIN);
        let criteria = ArmedCriteria::load(&artifact);
        assert!(!criteria.is_armed());
        assert_eq!(criteria.unarmed_reason(), Some(UnarmedReason::OwnerSignatureMismatch));
    }

    #[test]
    fn loader_unarmed_on_malformed_signature() {
        let mut artifact = valid_artifact();
        let mut malformed = [0x11u8; 65];
        malformed[64] = 0x07; // invalid parity byte -> from_raw_array rejects
        artifact.owner_signature = malformed;
        let criteria = ArmedCriteria::load(&artifact);
        assert!(!criteria.is_armed());
        assert_eq!(criteria.unarmed_reason(), Some(UnarmedReason::SignatureMalformed));
    }

    #[test]
    fn loader_unarmed_on_unrecoverable_signature() {
        let mut artifact = valid_artifact();
        let mut unrecoverable = [0u8; 65];
        unrecoverable[64] = 0x1b; // valid parity, but r = s = 0 cannot recover
        artifact.owner_signature = unrecoverable;
        let criteria = ArmedCriteria::load(&artifact);
        assert!(!criteria.is_armed());
        assert_eq!(criteria.unarmed_reason(), Some(UnarmedReason::SignatureRecoveryFailed));
    }

    // Payload-value / version parsing is unit-tested directly: with the SHA pin,
    // any value or version change also breaks the SHA (defense in depth), so the
    // parser is the reachable, non-vacuous surface for these branches.
    #[test]
    fn parse_criteria_payload_accepts_canonical_and_rejects_malformed() {
        let parsed = parse_criteria_payload(CANONICAL_PAYLOAD).expect("canonical parses");
        assert_eq!(parsed.criteria_version, EXPECTED_CRITERIA_VERSION);
        assert_eq!(parsed.per_tx_cap_wei, U256::from(PAYLOAD_PER_TX_CAP_WEI));
        assert_eq!(parsed.drawdown_floor_wei, U256::from(PAYLOAD_DRAWDOWN_FLOOR_WEI));

        assert!(parse_criteria_payload(b"").is_none(), "empty");
        assert!(
            parse_criteria_payload(b"criteria_version: 2.0.0\n").is_none(),
            "missing wei values"
        );
        assert!(
            parse_criteria_payload(
                b"criteria_version: 2.0.0\nper_tx_cap_wei: notanumber\ndrawdown_floor_wei: 1\nhot_wallet_cap_wei: 1\n"
            )
            .is_none(),
            "non-integer wei"
        );
        // A well-formed synthetic payload with a wrong version parses, and the
        // version comparison the loader performs would reject it.
        let wrong_version = parse_criteria_payload(
            b"criteria_version: 9.9.9\nper_tx_cap_wei: 1\ndrawdown_floor_wei: 1\nhot_wallet_cap_wei: 1\n",
        )
        .expect("well-formed");
        assert_ne!(wrong_version.criteria_version, EXPECTED_CRITERIA_VERSION);
    }

    #[test]
    fn parse_commit_sha_enforces_exact_lowercase_forty_hex() {
        assert!(parse_commit_sha(EXPECTED_CRITERIA_COMMIT).is_some());
        assert!(parse_commit_sha("4f789f2e85a9dfdaff990d505b3793a4fa23a47").is_none()); // 39
        assert!(parse_commit_sha("4f789f2e85a9dfdaff990d505b3793a4fa23a4766").is_none()); // 41
        assert!(parse_commit_sha("4F789F2E85A9DFDAFF990D505B3793A4FA23A476").is_none()); // upper
        assert!(parse_commit_sha("0x789f2e85a9dfdaff990d505b3793a4fa23a476").is_none()); // 0x
    }

    // ---------------------------------------------------------------------
    // (c) Drawdown completeness.
    // ---------------------------------------------------------------------

    #[test]
    fn drawdown_incomplete_inputs_all_halt() {
        let criteria = armed();
        for input in [DrawdownInput::PendingOrUnresolved, DrawdownInput::Missing, DrawdownInput::Error]
        {
            assert_eq!(
                drawdown_floor(input, &criteria),
                Decision::Halt(GuardReason::DrawdownIncomplete),
                "{input:?} must halt as incomplete, never treated as loss 0"
            );
        }
    }

    // ---------------------------------------------------------------------
    // (d) Kill 3-state + latch + persistence + reset anti-replay.
    // ---------------------------------------------------------------------

    #[test]
    fn kill_store_cold_start_is_unknown() {
        let dir = temp_dir("cold");
        let store = FileKillStateStore::new(dir.join("killstate.json"));
        assert_eq!(store.load(), KillState::Unknown);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn kill_store_engage_persists_across_restart() {
        let dir = temp_dir("engage");
        let path = dir.join("killstate.json");
        FileKillStateStore::new(&path)
            .engage(KillReason::StrictMinOutPrincipalLoss)
            .expect("engage");
        // Fresh handle == restart.
        let restarted = FileKillStateStore::new(&path);
        assert_eq!(
            restarted.load(),
            KillState::Engaged { reason: KillReason::StrictMinOutPrincipalLoss }
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn kill_store_valid_owner_reset_clears_and_survives_restart() {
        let dir = temp_dir("reset-ok");
        let path = dir.join("killstate.json");
        let store = FileKillStateStore::new(&path);
        store.engage(KillReason::DrawdownFloorBreach).expect("engage");
        store
            .owner_reset(&reset_attestation(1, SIG_RESET_EPOCH1))
            .expect("valid reset clears");
        assert_eq!(store.load(), KillState::Clear { verified_at: 1 });
        // Restart re-verifies the persisted attestation against the epoch.
        assert_eq!(
            FileKillStateStore::new(&path).load(),
            KillState::Clear { verified_at: 1 }
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn kill_store_reset_rejects_stale_epoch_replay() {
        let dir = temp_dir("reset-replay");
        let path = dir.join("killstate.json");
        let store = FileKillStateStore::new(&path);
        store.engage(KillReason::DrawdownFloorBreach).expect("engage epoch 1");
        store.engage(KillReason::DrawdownFloorBreach).expect("engage epoch 2");
        // A once-valid epoch-1 attestation cannot clear the epoch-2 engagement.
        let result = store.owner_reset(&reset_attestation(1, SIG_RESET_EPOCH1));
        assert!(matches!(result, Err(KillStoreError::ResetEpochMismatch)));
        assert!(matches!(store.load(), KillState::Engaged { .. }));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn kill_store_reset_rejects_epoch_forged_attestation() {
        let dir = temp_dir("reset-forge");
        let path = dir.join("killstate.json");
        let store = FileKillStateStore::new(&path);
        store.engage(KillReason::DrawdownFloorBreach).expect("epoch 1");
        store.engage(KillReason::DrawdownFloorBreach).expect("epoch 2");
        // Attestation claims epoch 2 but the signature is over the epoch-1 message.
        let result = store.owner_reset(&reset_attestation(2, SIG_RESET_EPOCH1));
        assert!(matches!(result, Err(KillStoreError::OwnerSignatureMismatch)));
        assert!(matches!(store.load(), KillState::Engaged { .. }));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn kill_store_reset_at_second_epoch_uses_bound_signature() {
        let dir = temp_dir("reset-epoch2");
        let path = dir.join("killstate.json");
        let store = FileKillStateStore::new(&path);
        store.engage(KillReason::DrawdownFloorBreach).expect("epoch 1");
        store.engage(KillReason::DrawdownFloorBreach).expect("epoch 2");
        store
            .owner_reset(&reset_attestation(2, SIG_RESET_EPOCH2))
            .expect("epoch-2 reset clears");
        assert_eq!(store.load(), KillState::Clear { verified_at: 2 });
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn kill_store_reset_rejects_wrong_domain() {
        let dir = temp_dir("reset-domain");
        let path = dir.join("killstate.json");
        let store = FileKillStateStore::new(&path);
        store.engage(KillReason::DrawdownFloorBreach).expect("engage");
        let result = store.owner_reset(&reset_attestation(1, SIG_RESET_WRONG_DOMAIN));
        assert!(matches!(result, Err(KillStoreError::OwnerSignatureMismatch)));
        assert!(matches!(store.load(), KillState::Engaged { .. }));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn kill_store_reset_rejected_when_not_engaged() {
        let dir = temp_dir("reset-notengaged");
        let path = dir.join("killstate.json");
        let store = FileKillStateStore::new(&path);
        let result = store.owner_reset(&reset_attestation(1, SIG_RESET_EPOCH1));
        assert!(matches!(result, Err(KillStoreError::NotEngaged)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn kill_store_corrupt_and_bare_clear_load_unknown() {
        let dir = temp_dir("corrupt");
        let path = dir.join("killstate.json");
        let store = FileKillStateStore::new(&path);

        fs::write(&path, b"{ this is not json").expect("write torn");
        assert_eq!(store.load(), KillState::Unknown, "torn/corrupt -> Unknown");

        // Bare Clear without the reset attestation must not clear (fail-closed).
        fs::write(&path, br#"{"state":"clear","epoch":1}"#).expect("write bare clear");
        assert_eq!(store.load(), KillState::Unknown, "bare clear -> Unknown");

        // Clear with a tampered (undecodable) attestation signature.
        fs::write(
            &path,
            br#"{"state":"clear","epoch":1,"reset":{"engagement_epoch":1,"nonce":424242,"signature_hex":"00"}}"#,
        )
        .expect("write tampered clear");
        assert_eq!(store.load(), KillState::Unknown, "tampered clear -> Unknown");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn kill_store_reset_persist_failure_keeps_engaged() {
        let dir = temp_dir("reset-writefail");
        let path = dir.join("killstate.json");
        let store = FileKillStateStore::new(&path);
        store.engage(KillReason::DrawdownFloorBreach).expect("engage");
        assert!(matches!(store.load(), KillState::Engaged { .. }));

        // Make the directory read-only so the atomic temp write fails.
        set_mode(&dir, 0o500);
        let result = store.owner_reset(&reset_attestation(1, SIG_RESET_EPOCH1));
        set_mode(&dir, 0o700); // restore before asserting to avoid leaking perms

        assert!(matches!(result, Err(KillStoreError::Io)), "reset write should fail");
        assert!(
            matches!(store.load(), KillState::Engaged { .. }),
            "state stays Engaged after a failed reset persist"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    fn set_mode(dir: &std::path::Path, mode: u32) {
        let mut perms = fs::metadata(dir).expect("metadata").permissions();
        perms.set_mode(mode);
        fs::set_permissions(dir, perms).expect("set permissions");
    }

    // ---------------------------------------------------------------------
    // (e) Engaged transitions (three automatic triggers).
    // ---------------------------------------------------------------------

    #[test]
    fn kill_store_engage_records_each_trigger_reason() {
        for reason in [
            KillReason::KeyOrSignatureFailure,
            KillReason::StrictMinOutPrincipalLoss,
            KillReason::DrawdownFloorBreach,
        ] {
            let dir = temp_dir("trigger");
            let store = FileKillStateStore::new(dir.join("killstate.json"));
            store.engage(reason).expect("engage");
            assert_eq!(store.load(), KillState::Engaged { reason });
            let _ = fs::remove_dir_all(&dir);
        }
    }

    // ---------------------------------------------------------------------
    // (f) Alert-delivery failure must not open submission.
    // ---------------------------------------------------------------------

    #[test]
    fn alert_delivery_failure_stays_closed() {
        // A trigger engaged the latch; even if a downstream alert never delivers,
        // the gate remains closed because the latch (not the alert) drives it.
        let criteria = armed();
        let closed = submit_gate(SubmitContext {
            armed: &criteria,
            amount_in_wei: U256::from(1u64),
            drawdown: under_floor_complete(),
            kill: KillState::Engaged { reason: KillReason::KeyOrSignatureFailure },
        });
        assert_eq!(closed, SubmitDecision::Closed(ClosedReason::KillNotClear));
        // Likewise an Unknown (undetermined) kill state fails closed.
        let unknown = submit_gate(SubmitContext {
            armed: &criteria,
            amount_in_wei: U256::from(1u64),
            drawdown: under_floor_complete(),
            kill: KillState::Unknown,
        });
        assert_eq!(unknown, SubmitDecision::Closed(ClosedReason::KillNotClear));
    }

    // ---------------------------------------------------------------------
    // (g) submit_gate truth table (only the full conjunction opens).
    // ---------------------------------------------------------------------

    #[test]
    fn submit_gate_opens_only_on_full_conjunction() {
        let criteria = armed();
        let opened = submit_gate(SubmitContext {
            armed: &criteria,
            amount_in_wei: criteria.per_tx_cap_wei(),
            drawdown: under_floor_complete(),
            kill: KillState::Clear { verified_at: 1 },
        });
        assert_eq!(opened, SubmitDecision::Open);
    }

    #[test]
    fn submit_gate_closed_when_not_armed() {
        let unarmed = ArmedCriteria::load_with_owner(&valid_artifact(), None);
        let decision = submit_gate(SubmitContext {
            armed: &unarmed,
            amount_in_wei: U256::from(1u64),
            drawdown: under_floor_complete(),
            kill: KillState::Clear { verified_at: 1 },
        });
        assert_eq!(decision, SubmitDecision::Closed(ClosedReason::NotArmed));
    }

    #[test]
    fn submit_gate_closed_on_criteria_pin_mismatch() {
        // Armed, but the version/SHA do not match the compiled-in runtime pin.
        let mismatched =
            ArmedCriteria::armed_for_test(U256::from(1u64), U256::from(1u64), "1.0.0", [0u8; 32]);
        let decision = submit_gate(SubmitContext {
            armed: &mismatched,
            amount_in_wei: U256::ZERO,
            drawdown: under_floor_complete(),
            kill: KillState::Clear { verified_at: 1 },
        });
        assert_eq!(decision, SubmitDecision::Closed(ClosedReason::CriteriaPinMismatch));
    }

    #[test]
    fn submit_gate_closed_on_each_single_predicate_failure() {
        let criteria = armed();
        let cap = criteria.per_tx_cap_wei();

        // over cap
        assert_eq!(
            submit_gate(SubmitContext {
                armed: &criteria,
                amount_in_wei: cap + U256::from(1u64),
                drawdown: under_floor_complete(),
                kill: KillState::Clear { verified_at: 1 },
            }),
            SubmitDecision::Closed(ClosedReason::PerTxCapExceeded)
        );

        // drawdown over floor
        assert_eq!(
            submit_gate(SubmitContext {
                armed: &criteria,
                amount_in_wei: cap,
                drawdown: DrawdownInput::Complete {
                    cumulative_realized_loss_wei: criteria.drawdown_floor_wei() + U256::from(1u64),
                    provenance: LossProvenance::OnchainRealized,
                },
                kill: KillState::Clear { verified_at: 1 },
            }),
            SubmitDecision::Closed(ClosedReason::DrawdownFloorBreached)
        );

        // drawdown incomplete
        assert_eq!(
            submit_gate(SubmitContext {
                armed: &criteria,
                amount_in_wei: cap,
                drawdown: DrawdownInput::Missing,
                kill: KillState::Clear { verified_at: 1 },
            }),
            SubmitDecision::Closed(ClosedReason::DrawdownIncomplete)
        );

        // kill not clear
        assert_eq!(
            submit_gate(SubmitContext {
                armed: &criteria,
                amount_in_wei: cap,
                drawdown: under_floor_complete(),
                kill: KillState::Unknown,
            }),
            SubmitDecision::Closed(ClosedReason::KillNotClear)
        );
    }

    // ---------------------------------------------------------------------
    // Build-time binding: the embedded payload hashes to the pinned SHA, and the
    // pinned commit bytes match the hex constant.
    // ---------------------------------------------------------------------

    #[test]
    fn embedded_payload_binds_to_pinned_criteria_sha() {
        let digest: [u8; 32] = Sha256::digest(CANONICAL_PAYLOAD).into();
        assert_eq!(digest, CRITERIA_SHA);
    }

    #[test]
    fn expected_commit_hex_and_bytes_agree() {
        assert_eq!(parse_commit_sha(EXPECTED_CRITERIA_COMMIT), Some(EXPECTED_CRITERIA_COMMIT_BYTES));
    }
}
