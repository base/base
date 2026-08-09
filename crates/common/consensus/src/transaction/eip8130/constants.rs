//! Constants for the [EIP-8130] Account Abstraction transaction type.
//!
//! [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130

use alloy_primitives::{Address, U256, address};

/// Container for [EIP-8130] protocol constants.
///
/// All constants are exposed as associated `pub const` items so the public API
/// is type-anchored (per repo convention: "the public API exports types, not loose
/// functions").
///
/// Spec status (as of writing): EIP-8130 is in Draft. The transaction and payer
/// type bytes below are pinned to the EIP-8130 constant-table values
/// (`AA_TX_TYPE = 0x79`, `AA_PAYER_TYPE = 0x7A`).
///
/// [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130
#[derive(Debug)]
pub struct Eip8130Constants;

impl Eip8130Constants {
    /// [EIP-2718] transaction type byte for AA transactions (`EIP8130_TX_TYPE`).
    ///
    /// Pinned to the EIP-8130 constant-table value `AA_TX_TYPE = 0x79`.
    ///
    /// [EIP-2718]: https://eips.ethereum.org/EIPS/eip-2718
    pub const EIP8130_TX_TYPE: u8 = 0x79;

    /// Magic prefix byte for payer signature domain separation (`EIP8130_PAYER_TYPE`).
    ///
    /// Used in the payer signature preimage:
    /// `keccak256(EIP8130_PAYER_TYPE || rlp([...fields through calls...]))`.
    ///
    /// Pinned to the EIP-8130 constant-table value `AA_PAYER_TYPE = 0x7A`.
    pub const EIP8130_PAYER_TYPE: u8 = 0x7A;

    /// Base intrinsic gas cost for any AA transaction (`EIP8130_BASE_COST`).
    pub const EIP8130_BASE_COST: u64 = 15_000;

    /// Sentinel `nonce_key` value selecting nonce-free mode (`NONCE_KEY_MAX`).
    ///
    /// When `nonce_key == NONCE_KEY_MAX`, no nonce state is read or written
    /// and replay protection relies on `valid_before` (which must be non-zero).
    pub const NONCE_KEY_MAX: U256 = U256::MAX;

    /// Actor scope bit: ungated `sender_auth` validation context; may originate
    /// transactions to any `call.to`.
    pub const SCOPE_SENDER: u16 = 0x0001;

    /// Actor scope bit: policy-gated sender context; may originate transactions
    /// only to the actor's `policy_manager`.
    pub const SCOPE_POLICY: u16 = 0x0002;

    /// Actor scope bit: nonce authorization context; permits a restricted actor
    /// to use sequenced `nonce_key`s (otherwise nonceless-only).
    pub const SCOPE_NONCE: u16 = 0x0004;

    /// Actor scope bit: self-pay gas; authorizes paying the account's own gas
    /// when `payer == sender`.
    pub const SCOPE_SELF_PAYER: u16 = 0x0008;

    /// Actor scope bit: sponsor gas; authorizes acting as `payer_auth` for a
    /// different sender (`payer != sender`).
    pub const SCOPE_SPONSOR_PAYER: u16 = 0x0010;

    // ERC-1271 signing rides on operational authority (admin `scope == 0x00`, or
    // a SENDER actor without POLICY); it is not its own scope bit, so there is no
    // `SCOPE_SIGNATURE`. The remaining bits of the `uint16` scope are spare,
    // reserved for future pure grants. The Keystore itself is scope-agnostic
    // except for `scope == 0` (admin) and the single interpreted `SCOPE_POLICY`
    // bit; every other bit is stored verbatim and interpreted protocol-side.

    /// Domain-separation prefix for the `replay_id` preimage
    /// (`keccak256(REPLAY_ID_TYPE || rlp([...])`).
    ///
    /// Pinned to the EIP-8130 constant-table value `REPLAY_ID_TYPE = 0x7901`. It
    /// shares the `AA_TX_TYPE` first byte (`0x79`) but appends `0x01`: the
    /// sender/payer signing hashes hash `EIP8130_TX_TYPE`/`EIP8130_PAYER_TYPE`
    /// followed by an RLP list header (always `>= 0xc0`), so the trailing `0x01`
    /// can never coincide with a valid list header and the preimage spaces cannot
    /// collide.
    pub const REPLAY_ID_TYPE: [u8; 2] = [0x79, 0x01];

    /// Unrestricted scope value (actor is valid in all contexts).
    pub const SCOPE_UNRESTRICTED: u16 = 0x0000;

    /// [EIP-7702]-style delegation indicator code prefix.
    ///
    /// A delegated account's code is exactly `DELEGATION_INDICATOR_PREFIX || target`
    /// where `target` is a 20-byte address.
    ///
    /// [EIP-7702]: https://eips.ethereum.org/EIPS/eip-7702
    pub const DELEGATION_INDICATOR_PREFIX: [u8; 3] = [0xef, 0x01, 0x00];

    /// Total length in bytes of an [EIP-7702] delegation indicator
    /// (`DELEGATION_INDICATOR_PREFIX || target`).
    ///
    /// [EIP-7702]: https://eips.ethereum.org/EIPS/eip-7702
    pub const DELEGATION_INDICATOR_SIZE: usize = 23;

    /// `account_changes` entry type byte: account creation.
    pub const ACCOUNT_CHANGE_TYPE_CREATE: u8 = 0x00;

    /// `account_changes` entry type byte: a signed account-change batch
    /// (`SignedAccountChanges`, applied via `applySignedAccountChanges`).
    pub const ACCOUNT_CHANGE_TYPE_CONFIG: u8 = 0x01;

    /// `account_changes` entry type byte: code delegation.
    pub const ACCOUNT_CHANGE_TYPE_DELEGATION: u8 = 0x02;

    /// `SignedAccountChanges.channel` byte: the Local channel (binds
    /// `block.chainid`; carries epoch + sequence and the unsequenced JIT mode).
    pub const CHANNEL_LOCAL: u8 = 0x00;

    /// `SignedAccountChanges.channel` byte: the Multichain channel (binds
    /// `chain_id == 0`; a plain monotonic counter with no epoch or JIT mode).
    pub const CHANNEL_MULTICHAIN: u8 = 0x01;

    /// `ChangeType` op byte: authorize (upsert) an actor. Payload is
    /// `abi.encode(bytes32 actorId, ActorConfig cfg, bytes policyData)`.
    pub const CHANGE_TYPE_AUTHORIZE_ACTOR: u8 = 0x00;

    /// `ChangeType` op byte: revoke an actor. Payload is `abi.encode(bytes32 actorId)`.
    pub const CHANGE_TYPE_REVOKE_ACTOR: u8 = 0x01;

    /// `ChangeType` op byte: increment the local epoch (either channel; empty
    /// payload). Invalidates every unlanded local signature at a prior epoch.
    pub const CHANGE_TYPE_INCREMENT_LOCAL_EPOCH: u8 = 0x02;

    /// `ChangeType` op byte: lock the account (Local channel only; standalone;
    /// payload is `abi.encode(uint16 unlockDelay)`).
    pub const CHANGE_TYPE_LOCK: u8 = 0x03;

    /// `ChangeType` op byte: unlock the account (Local channel only; standalone;
    /// empty payload).
    pub const CHANGE_TYPE_UNLOCK: u8 = 0x04;

    /// Local-channel sequence low-half sentinel (`type(uint32).max`) marking an
    /// unsequenced (JIT) batch: it consumes no sequence and stays replayable
    /// until the local epoch moves. Sequenced batches may run up to
    /// `UNSEQUENCED - 2`.
    pub const UNSEQUENCED: u32 = u32::MAX;

    /// The single canonical secp256k1 ("k1") authenticator, fixed at
    /// `address(1)`. Native `ecrecover`: the protocol recovers from the `data`
    /// blob (`r || s || v`) rather than `STATICCALL`-ing a contract. The same
    /// identity serves both the implicit default EOA and any explicitly
    /// registered k1 actor; the `actor_config` slot alone distinguishes a
    /// full-owner EOA from a scoped key.
    ///
    /// `address(0)` is reserved as the empty / "no actor configured" sentinel and
    /// is never a valid authenticator selector; addresses below this are reserved.
    pub const K1_AUTHENTICATOR: Address = address!("0x0000000000000000000000000000000000000001");

    /// `AccountState.flags` bit that disables the implicit default-EOA path.
    ///
    /// The implicit default EOA is a [`Self::K1_AUTHENTICATOR`] signature whose
    /// recovered signer equals the account; with no explicit `actor_config` it
    /// resolves to a full owner, gated solely on this flag. Set by
    /// `createAccount`/`importAccount` (disabled by default), and by authorizing
    /// or revoking the self-actor; once set it is never cleared (monotonic), so
    /// an explicit self-actor entry always implies the flag is set.
    pub const DEFAULT_EOA_REVOKED: u8 = 0x01;

    /// `AccountState.flags` bit (spec `LOCKED`): when set, actor configuration is
    /// frozen — every config change and delegation is rejected on both the native
    /// and EVM paths. The only permitted operation is `applySignedLockChanges`'s
    /// unlock op. Set/cleared exclusively through the EVM `applySignedLockChanges`
    /// entry point.
    pub const FLAG_LOCKED: u8 = 0x02;

    /// `AccountState.flags` bit (spec `UNLOCK_INITIATED`): selects how the packed
    /// `lock_union` field is interpreted. While clear, `lock_union` holds the
    /// configured `unlock_delay` (seconds, `uint16` range); while set, it holds
    /// `unlocks_at` (the timestamp at which the pending unlock takes effect). Only
    /// meaningful when [`Self::FLAG_LOCKED`] is set.
    pub const FLAG_UNLOCK_INITIATED: u8 = 0x04;

    /// `AccountState.flags` bit (spec `CONTRACT_ESTABLISHED`): set on every
    /// account the keystore establishes — `createAccount` (mirrored by the
    /// node's create path) and `importAccount` — marking it
    /// "keystore-established, not a proven address key."
    ///
    /// Permanent once set and never consulted during authentication. The protocol
    /// reads it for code-delegation gating: an account may have **empty code yet
    /// retain EIP-8130 state** (e.g. an EIP-6780 same-transaction `SELFDESTRUCT`),
    /// so empty code alone must never be read as proof of a known EOA key. The
    /// node therefore rejects a delegation onto an empty-code account that carries
    /// this flag (it is not a proven-key EOA and must not be re-delegated as one).
    pub const FLAG_CONTRACT_ESTABLISHED: u8 = 0x08;

    /// Exact byte length of a policy-bearing actor's `policyData`:
    /// `manager (20) || commitment (32)`. Required when `scope & SCOPE_POLICY`
    /// is set; `policyData` MUST be empty otherwise.
    pub const POLICY_DATA_LEN: usize = 52;

    /// Maximum number of `ConfigChange` entries the mempool accepts in a single
    /// transaction. The spec marks this as a node policy ("Nodes SHOULD enforce
    /// a configurable per-transaction limit"); we pin a conservative default
    /// here that downstream operators can revisit once the spec finalises.
    pub const MAX_CONFIG_CHANGES_PER_TX: usize = 10;

    /// Maximum number of `account_changes` entries (of any kind: `Create`,
    /// `ConfigChange`, `Delegation`) the mempool accepts in a single
    /// transaction. This is an **interim** total-entry admission cap that keeps
    /// per-transaction admission work (and the in-memory overlay it applies
    /// against) small and bounded while the interleaved authorize-and-apply
    /// admission flow beds in.
    ///
    /// Relationship to the per-type caps ([`Self::MAX_CONFIG_CHANGES_PER_TX`]
    /// and the implicit ≤1 `Create` / ≤1 `Delegation` structural limits):
    ///
    /// - **While this cap is the smallest** (3 < 10 today), it is the *binding*
    ///   admission constraint — a transaction can never reach
    ///   `MAX_CONFIG_CHANGES_PER_TX` config changes because the total cap stops
    ///   it first. The per-type caps are effectively dormant.
    /// - **Once this is raised to or above `MAX_CONFIG_CHANGES_PER_TX`**, the
    ///   per-type caps become the binding constraints: `MAX_CONFIG_CHANGES_PER_TX`
    ///   bounds config changes, and the ≤1 `Create` / ≤1 `Delegation` structural
    ///   rules bound the rest. Raising this cap therefore *relaxes* admission up
    ///   to (but never beyond) the per-type ceilings.
    ///
    /// Keep this value `<= MAX_CONFIG_CHANGES_PER_TX + 2` (one create + one
    /// delegation) if the intent is for the total cap to stay the binding limit.
    pub const MAX_ACCOUNT_CHANGES_PER_TX: usize = 3;

    /// Maximum validity-window span (in **milliseconds** beyond the current
    /// reference time, i.e. `valid_before - now`) the mempool accepts for
    /// nonce-free-mode transactions (`nonce_key == NONCE_KEY_MAX`). Per the spec
    /// ("Nodes SHOULD reject `NONCE_KEY_MAX` transactions whose validity window
    /// exceeds a short window"), a tight window bounds the replay surface in the
    /// absence of nonce state.
    ///
    /// Sized at 20 seconds (20,000 ms, ~10 Base block times at 2s) so a single
    /// `valid_before` picked by a client stays inside the window on every node
    /// despite the spread between each node's head-block timestamp and
    /// wall-clock time (followers lag the sequencer), while still keeping the
    /// nonce-free replay surface small.
    ///
    /// # Invariant: must stay `<=` the on-chain inclusion window
    ///
    /// This is only the mempool *pre-filter*; the authoritative, consensus-critical
    /// window is `NonceManagerStorage::NONCE_FREE_EXPIRY_WINDOW` (currently
    /// **30,000 ms**), enforced against the block timestamp when the nonce-free
    /// replay entry is recorded at inclusion. This value MUST remain `<=` that
    /// on-chain window so the pool never admits a transaction whose `valid_before`
    /// the block-inclusion check would reject (which would waste block space on
    /// transactions that can never land). The gap between the two (20,000 vs
    /// 30,000 ms) is deliberate headroom that also absorbs the skew between the
    /// pool's reference clock and the inclusion block timestamp.
    ///
    /// Raising this at or beyond the on-chain window is a coordinated **consensus /
    /// fork-level** change: the on-chain `NONCE_FREE_EXPIRY_WINDOW` must be raised
    /// too, and `NonceManagerStorage::REPLAY_BUFFER_CAPACITY` resized to keep
    /// `peak nonce-free throughput x window` within capacity (see the buffer-sizing
    /// invariant test in `base-common-precompiles`).
    pub const NONCE_FREE_MAX_EXPIRY_WINDOW: u64 = 20_000;

    /// Maximum number of actor entries the mempool accepts in a single
    /// `Create.initial_actors` slice. Bounds per-transaction memory and CPU
    /// spent on duplicate-actor_id detection at admission time.
    pub const MAX_ACTORS_PER_ENTRY: usize = 32;

    /// Maximum number of `actorChanges` the mempool accepts within a single
    /// `ConfigChange` entry. An interim conservative cap that keeps the
    /// per-config-change work (ABI decode, duplicate detection, authenticator
    /// validation) small and bounded. Deliberately lower than
    /// [`Self::MAX_ACTORS_PER_ENTRY`]; can be raised toward that value once
    /// the interleaved admission flow is proven out.
    pub const MAX_ACTOR_CHANGES_PER_CONFIG: usize = 5;

    /// Maximum runtime bytecode size for a create entry, matching EIP-170's
    /// `MAX_CODE_SIZE` limit. EIP-8130 places runtime code directly, so the
    /// mempool rejects oversized code before execution.
    pub const MAX_CODE_SIZE: usize = 24_576;
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY_TX_TYPE: u8 = 0x00;
    const EIP2930_TX_TYPE: u8 = 0x01;
    const EIP1559_TX_TYPE: u8 = 0x02;
    const EIP7702_TX_TYPE: u8 = 0x04;
    const DEPOSIT_TX_TYPE: u8 = 0x7E;

    #[test]
    fn type_bytes_are_distinct() {
        assert_ne!(Eip8130Constants::EIP8130_TX_TYPE, Eip8130Constants::EIP8130_PAYER_TYPE);
        assert_ne!(Eip8130Constants::EIP8130_TX_TYPE, LEGACY_TX_TYPE);
        assert_ne!(Eip8130Constants::EIP8130_TX_TYPE, EIP2930_TX_TYPE);
        assert_ne!(Eip8130Constants::EIP8130_TX_TYPE, EIP1559_TX_TYPE);
        assert_ne!(Eip8130Constants::EIP8130_TX_TYPE, EIP7702_TX_TYPE);
        assert_ne!(Eip8130Constants::EIP8130_TX_TYPE, DEPOSIT_TX_TYPE);
    }

    #[test]
    fn scope_bits_are_orthogonal() {
        let bits = [
            Eip8130Constants::SCOPE_SENDER,
            Eip8130Constants::SCOPE_POLICY,
            Eip8130Constants::SCOPE_NONCE,
            Eip8130Constants::SCOPE_SELF_PAYER,
            Eip8130Constants::SCOPE_SPONSOR_PAYER,
        ];
        let mut acc: u16 = 0;
        for b in bits {
            assert_eq!(b.count_ones(), 1, "scope bit must be a single bit");
            assert_eq!(acc & b, 0, "scope bits must be orthogonal");
            acc |= b;
        }
        assert_eq!(Eip8130Constants::SCOPE_UNRESTRICTED, 0);
    }

    #[test]
    fn delegation_indicator_size_matches_prefix_plus_address() {
        assert_eq!(
            Eip8130Constants::DELEGATION_INDICATOR_SIZE,
            Eip8130Constants::DELEGATION_INDICATOR_PREFIX.len() + 20
        );
    }
}
