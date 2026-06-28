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
/// (`AA_TX_TYPE = 0x7B`, `AA_PAYER_TYPE = 0x7C`).
///
/// [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130
#[derive(Debug)]
pub struct Eip8130Constants;

impl Eip8130Constants {
    /// [EIP-2718] transaction type byte for AA transactions (`EIP8130_TX_TYPE`).
    ///
    /// Pinned to the EIP-8130 constant-table value `AA_TX_TYPE = 0x7B`.
    ///
    /// [EIP-2718]: https://eips.ethereum.org/EIPS/eip-2718
    pub const EIP8130_TX_TYPE: u8 = 0x7B;

    /// Magic prefix byte for payer signature domain separation (`EIP8130_PAYER_TYPE`).
    ///
    /// Used in the payer signature preimage:
    /// `keccak256(EIP8130_PAYER_TYPE || rlp([...fields through calls...]))`.
    ///
    /// Pinned to the EIP-8130 constant-table value `AA_PAYER_TYPE = 0x7C`.
    pub const EIP8130_PAYER_TYPE: u8 = 0x7C;

    /// Base intrinsic gas cost for any AA transaction (`EIP8130_BASE_COST`).
    pub const EIP8130_BASE_COST: u64 = 15_000;

    /// Sentinel `nonce_key` value selecting nonce-free mode (`NONCE_KEY_MAX`).
    ///
    /// When `nonce_key == NONCE_KEY_MAX`, no nonce state is read or written
    /// and replay protection relies on `expiry` (which must be non-zero).
    pub const NONCE_KEY_MAX: U256 = U256::MAX;

    /// Actor scope bit: ERC-1271 `verifySignature()` context.
    pub const SCOPE_SIGNATURE: u8 = 0x01;

    /// Actor scope bit: `sender_auth` validation context.
    pub const SCOPE_SENDER: u8 = 0x02;

    /// Actor scope bit: `payer_auth` validation context.
    pub const SCOPE_PAYER: u8 = 0x04;

    /// Actor scope bit: config change `auth` context.
    pub const SCOPE_CONFIG: u8 = 0x08;

    /// Unrestricted scope value (actor is valid in all contexts).
    pub const SCOPE_UNRESTRICTED: u8 = 0x00;

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

    /// `account_changes` entry type byte: actor config change.
    pub const ACCOUNT_CHANGE_TYPE_CONFIG: u8 = 0x01;

    /// `account_changes` entry type byte: code delegation.
    pub const ACCOUNT_CHANGE_TYPE_DELEGATION: u8 = 0x02;

    /// `actor_change` operation byte: authorize a new actor.
    pub const ACTOR_CHANGE_AUTHORIZE: u8 = 0x01;

    /// `actor_change` operation byte: revoke an existing actor.
    pub const ACTOR_CHANGE_REVOKE: u8 = 0x02;

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

    /// Maximum `expiry` window (in seconds beyond the current wall-clock time)
    /// the mempool accepts for nonce-free-mode transactions
    /// (`nonce_key == NONCE_KEY_MAX`). Per the spec ("Nodes SHOULD reject
    /// `NONCE_KEY_MAX` transactions whose `expiry` exceeds a short window"),
    /// a tight window bounds the replay surface in the absence of nonce state.
    pub const NONCE_FREE_MAX_EXPIRY_WINDOW: u64 = 10;

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
            Eip8130Constants::SCOPE_SIGNATURE,
            Eip8130Constants::SCOPE_SENDER,
            Eip8130Constants::SCOPE_PAYER,
            Eip8130Constants::SCOPE_CONFIG,
        ];
        let mut acc: u8 = 0;
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
