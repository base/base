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
/// Spec status (as of writing): EIP-8130 is in Draft. The transaction type byte
/// is the EIP-8130 constant-table value `AA_TX_TYPE = 0x79`. The signing and
/// replay-id domains are two-byte sub-prefixes of it (`0x7900`, `0x7901`,
/// `0x7902`), so EIP-8130 claims no other [EIP-2718] type byte.
///
/// [EIP-2718]: https://eips.ethereum.org/EIPS/eip-2718
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

    /// Domain prefix of the sender signature hash
    /// (`keccak256(SENDER_SIGNATURE_TYPE || rlp([...]))`).
    ///
    /// Every EIP-8130 domain starts with [`Self::EIP8130_TX_TYPE`]. The
    /// transaction hash hashes that byte followed by an RLP list header, which
    /// is always `>= 0xc0`, while the two-byte domains append a second byte
    /// below `0xc0`. No two of these preimages can therefore be equal, and
    /// in particular a signature over a transaction hash is never a valid
    /// sender or payer signature.
    pub const SENDER_SIGNATURE_TYPE: [u8; 2] = [0x79, 0x00];

    /// Domain prefix of the payer signature hash
    /// (`keccak256(PAYER_SIGNATURE_TYPE || rlp([...]))`). See
    /// [`Self::SENDER_SIGNATURE_TYPE`] for why the domains cannot collide.
    pub const PAYER_SIGNATURE_TYPE: [u8; 2] = [0x79, 0x01];

    /// Domain prefix of the nonce-free `replay_id`
    /// (`keccak256(REPLAY_ID_TYPE || rlp([...]))`). See
    /// [`Self::SENDER_SIGNATURE_TYPE`] for why the domains cannot collide.
    pub const REPLAY_ID_TYPE: [u8; 2] = [0x79, 0x02];

    /// Sentinel `nonce_key` value selecting nonce-free mode (`NONCE_KEY_MAX`).
    ///
    /// When `nonce_key == NONCE_KEY_MAX`, no nonce state is read or written
    /// and replay protection relies on `valid_before` (which must be non-zero).
    pub const NONCE_KEY_MAX: U256 = U256::MAX;

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

    /// `account_changes` entry type byte: code delegation, the only entry type.
    ///
    /// Every other type byte is reserved by the AA transaction type and never
    /// decodes.
    pub const ACCOUNT_CHANGE_TYPE_DELEGATION: u8 = 0x01;

    /// The native secp256k1 ("k1") authenticator selector, fixed at
    /// `address(1)` and the only accepted one. A named sender or payer
    /// authenticates with `K1_AUTHENTICATOR || r || s || v`, recovered with
    /// native `ecrecover` to that exact address.
    pub const K1_AUTHENTICATOR: Address = address!("0x0000000000000000000000000000000000000001");

    /// In-memory `payer` value for open payer mode, encoded on the wire as the
    /// single byte `0x00`. The payer is recovered from a raw 65-byte
    /// `payer_auth` over the payer signature hash. A 20-byte zero address is
    /// not a valid wire `payer`.
    pub const OPEN_PAYER: Address = Address::ZERO;

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
    /// on-chain window (a compile-time assertion in `base-execution-txpool`
    /// enforces this) so the pool never admits a transaction whose `valid_before`
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

    /// Seconds/milliseconds split for validity-bound normalization, from
    /// EIP-8130's Timestamp Normalization section (`10^11`).
    ///
    /// `valid_after`/`valid_before` may be supplied in either Unix **seconds**
    /// or Unix **milliseconds**; the denomination is auto-detected per value.
    /// A non-zero bound *below* this threshold is interpreted as seconds and
    /// scaled to milliseconds; a bound at or above it is already milliseconds.
    /// The split sits far from every realistic timestamp for the next several
    /// millennia (`10^11` seconds is year ~5138; `10^11` ms is March 1973), so a
    /// seconds value (~`1.7e9`) is always below it and a milliseconds value
    /// (~`1.7e12`) always at or above it.
    pub const TIMESTAMP_MS_THRESHOLD: u64 = 100_000_000_000;

    /// Normalizes a validity bound (`valid_after`/`valid_before`) to Unix
    /// **milliseconds**, per EIP-8130 Timestamp Normalization.
    ///
    /// `0` is the "disabled" sentinel and passes through unchanged; a non-zero
    /// value below [`Self::TIMESTAMP_MS_THRESHOLD`] is treated as seconds and
    /// scaled by `1000` (saturating); a value at or above the threshold is
    /// already milliseconds and passes through unchanged. This is the single
    /// definition of the seconds↔ms rule shared by the mempool admission window
    /// ([`crate::Eip8130Signed::validate_timestamp`]) and the consensus
    /// inclusion window and nonce-free replay ring.
    #[must_use]
    pub const fn normalize_timestamp_ms(value: u64) -> u64 {
        if value != 0 && value < Self::TIMESTAMP_MS_THRESHOLD {
            value.saturating_mul(1_000)
        } else {
            value
        }
    }

    /// Maximum number of call phases accepted in one transaction.
    ///
    /// Each phase occupies an in-memory [`alloc::vec::Vec`] even when its RLP
    /// payload is empty. Bounding the count while decoding prevents a sequence
    /// of single-byte empty RLP lists from amplifying into unbounded allocations
    /// before transaction-pool admission limits run.
    pub const MAX_CALL_PHASES_PER_TX: usize = 1_024;

    /// Maximum number of `account_changes` entries accepted in one transaction.
    ///
    /// Delegation is the only account change type, and a transaction sets or
    /// clears at most one delegation.
    pub const MAX_ACCOUNT_CHANGES_PER_TX: usize = 1;
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
        assert_ne!(Eip8130Constants::EIP8130_TX_TYPE, LEGACY_TX_TYPE);
        assert_ne!(Eip8130Constants::EIP8130_TX_TYPE, EIP2930_TX_TYPE);
        assert_ne!(Eip8130Constants::EIP8130_TX_TYPE, EIP1559_TX_TYPE);
        assert_ne!(Eip8130Constants::EIP8130_TX_TYPE, EIP7702_TX_TYPE);
        assert_ne!(Eip8130Constants::EIP8130_TX_TYPE, DEPOSIT_TX_TYPE);
    }

    #[test]
    fn domains_are_sub_prefixes_of_the_tx_type() {
        let domains = [
            Eip8130Constants::SENDER_SIGNATURE_TYPE,
            Eip8130Constants::PAYER_SIGNATURE_TYPE,
            Eip8130Constants::REPLAY_ID_TYPE,
        ];
        assert_eq!(domains, [[0x79, 0x00], [0x79, 0x01], [0x79, 0x02]]);
        for domain in domains {
            assert_eq!(domain[0], Eip8130Constants::EIP8130_TX_TYPE);
            // Below every RLP list header, so no domain prefix can be read as
            // the start of the transaction-hash preimage.
            assert!(domain[1] < alloy_rlp::EMPTY_LIST_CODE);
        }
    }

    #[test]
    fn normalize_timestamp_ms_detects_seconds_and_milliseconds() {
        // Zero is the disabled sentinel: never scaled.
        assert_eq!(Eip8130Constants::normalize_timestamp_ms(0), 0);
        // Realistic seconds (~1.7e9) are below the threshold and scale to ms.
        assert_eq!(Eip8130Constants::normalize_timestamp_ms(1_700_000_000), 1_700_000_000_000);
        // Realistic milliseconds (~1.7e12) are at/above the threshold: unchanged.
        assert_eq!(Eip8130Constants::normalize_timestamp_ms(1_700_000_000_000), 1_700_000_000_000);
        // Threshold boundary: the value just below is seconds; the threshold
        // itself is already milliseconds.
        assert_eq!(
            Eip8130Constants::normalize_timestamp_ms(Eip8130Constants::TIMESTAMP_MS_THRESHOLD - 1),
            (Eip8130Constants::TIMESTAMP_MS_THRESHOLD - 1) * 1_000
        );
        assert_eq!(
            Eip8130Constants::normalize_timestamp_ms(Eip8130Constants::TIMESTAMP_MS_THRESHOLD),
            Eip8130Constants::TIMESTAMP_MS_THRESHOLD
        );
    }

    #[test]
    fn delegation_indicator_size_matches_prefix_plus_address() {
        assert_eq!(
            Eip8130Constants::DELEGATION_INDICATOR_SIZE,
            Eip8130Constants::DELEGATION_INDICATOR_PREFIX.len() + 20
        );
    }
}
