//! Stateful 2D nonce validation: resolve the channel implied by `nonce_key` and
//! compare the transaction's `nonce_sequence` against the live nonce state.

use core::cmp::Ordering;

use alloy_primitives::{Address, B256, U256};
use base_common_consensus::{Eip8130Constants, TxEip8130};
use base_common_precompiles::NonceManagerStorage;

use crate::NonceError;

/// Which consumer is validating the nonce. The check is identical except for how
/// a sequence *ahead* of the channel nonce is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NonceMode {
    /// Mempool admission. A sequence ahead of the channel is admissible and
    /// buffered until its predecessors arrive.
    Pool,
    /// Block inclusion. The sequence must equal the channel nonce exactly.
    Inclusion,
}

/// Outcome of a successful nonce validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NonceStatus {
    /// The transaction's sequence equals the channel nonce (or is a nonce-free
    /// transaction whose replay hash has not been seen) and may execute now.
    Ready,
    /// Pool-only: the sequence is ahead of the channel nonce by `gap` and is
    /// buffered until the intervening sequences are filled.
    Buffered {
        /// How far ahead of the current channel nonce the sequence is.
        gap: u64,
    },
}

/// Validates an EIP-8130 transaction's 2D nonce against the live nonce state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct NonceValidator;

impl NonceValidator {
    /// Validates `tx`'s `(nonce_key, nonce_sequence)` for `account` (the resolved
    /// transaction sender).
    ///
    /// `protocol_nonce` is the account's current basic nonce, read by the caller
    /// from account state; it is consulted only for the protocol channel
    /// (`nonce_key == 0`). `storage` serves the 2D channels and the nonce-free
    /// replay set. `now` (Unix seconds; block timestamp at inclusion, wall-clock
    /// in the pool) bounds the nonce-free replay-set lookup and is unused for
    /// sequence channels.
    ///
    /// Returns [`NonceStatus::Ready`] when the transaction may execute now,
    /// [`NonceStatus::Buffered`] when a pool transaction is ahead of its channel,
    /// or a [`NonceError`] for a stale, gapped (inclusion), or replayed nonce.
    pub fn validate(
        tx: &TxEip8130,
        account: Address,
        protocol_nonce: u64,
        storage: &NonceManagerStorage<'_>,
        mode: NonceMode,
        now: u64,
    ) -> Result<NonceStatus, NonceError> {
        if tx.nonce_key == Eip8130Constants::NONCE_KEY_MAX {
            // Nonce-free: no sequence channel. The structural rules
            // (nonce_sequence == 0, expiry window) are enforced upstream by
            // `Eip8130Signed::validate_timestamp`; the only stateful check is
            // that this logical transaction's replay hash is not already
            // recorded and unexpired.
            let replay = Self::replay_hash(tx, account);
            if storage.is_expiring_nonce_seen(replay, now)? {
                return Err(NonceError::Replay);
            }
            return Ok(NonceStatus::Ready);
        }

        // Protocol nonce (key 0) lives in account state, not the nonce manager;
        // every other channel is served by the manager precompile.
        let channel = if tx.nonce_key == U256::ZERO {
            protocol_nonce
        } else {
            storage.get_nonce(account, tx.nonce_key)?
        };

        match tx.nonce_sequence.cmp(&channel) {
            Ordering::Less => Err(NonceError::TooLow { channel, got: tx.nonce_sequence }),
            Ordering::Equal => Ok(NonceStatus::Ready),
            Ordering::Greater => match mode {
                NonceMode::Inclusion => {
                    Err(NonceError::TooHigh { channel, got: tx.nonce_sequence })
                }
                NonceMode::Pool => Ok(NonceStatus::Buffered { gap: tx.nonce_sequence - channel }),
            },
        }
    }

    /// The transaction's signature- and fee-invariant replay identifier.
    ///
    /// Public so the execution layer can recompute the identical key when it
    /// records the nonce via `check_and_mark_expiring_nonce` at block inclusion,
    /// rather than duplicating the derivation and risking divergence.
    #[must_use]
    pub fn replay_hash(tx: &TxEip8130, account: Address) -> B256 {
        tx.replay_id(account)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, address};
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use super::*;

    const ACCOUNT: Address = address!("0x1111111111111111111111111111111111111111");

    fn tx_with(nonce_key: U256, nonce_sequence: u64) -> TxEip8130 {
        TxEip8130 { nonce_key, nonce_sequence, ..Default::default() }
    }

    /// Runs `validate` against a freshly-seeded nonce manager. `seed` may mutate
    /// the manager (e.g. advance a channel) before the (immutable) check.
    fn check(
        tx: &TxEip8130,
        protocol_nonce: u64,
        mode: NonceMode,
        now: u64,
        seed: impl FnOnce(&mut NonceManagerStorage<'_>),
    ) -> Result<NonceStatus, NonceError> {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_timestamp(U256::from(now));
        StorageCtx::enter(&mut storage, |ctx| {
            let mut mgr = NonceManagerStorage::new(ctx);
            seed(&mut mgr);
            NonceValidator::validate(tx, ACCOUNT, protocol_nonce, &mgr, mode, now)
        })
    }

    #[test]
    fn protocol_nonce_ready_when_sequence_matches() {
        let tx = tx_with(U256::ZERO, 5);
        assert_eq!(check(&tx, 5, NonceMode::Inclusion, 0, |_| {}), Ok(NonceStatus::Ready));
    }

    #[test]
    fn protocol_nonce_below_channel_is_too_low() {
        let tx = tx_with(U256::ZERO, 4);
        assert_eq!(
            check(&tx, 5, NonceMode::Pool, 0, |_| {}),
            Err(NonceError::TooLow { channel: 5, got: 4 })
        );
    }

    #[test]
    fn protocol_nonce_gap_rejected_at_inclusion_buffered_in_pool() {
        let tx = tx_with(U256::ZERO, 8);
        assert_eq!(
            check(&tx, 5, NonceMode::Inclusion, 0, |_| {}),
            Err(NonceError::TooHigh { channel: 5, got: 8 })
        );
        assert_eq!(check(&tx, 5, NonceMode::Pool, 0, |_| {}), Ok(NonceStatus::Buffered { gap: 3 }));
    }

    #[test]
    fn channel_nonce_is_read_from_the_manager() {
        let key = U256::from(7u64);
        // Advance the 2D channel to 3, then the protocol_nonce arg must be ignored.
        let seed = |mgr: &mut NonceManagerStorage<'_>| {
            for _ in 0..3 {
                mgr.increment_nonce(ACCOUNT, key).unwrap();
            }
        };
        assert_eq!(
            check(&tx_with(key, 3), 999, NonceMode::Inclusion, 0, seed),
            Ok(NonceStatus::Ready)
        );
        assert_eq!(
            check(&tx_with(key, 2), 999, NonceMode::Pool, 0, seed),
            Err(NonceError::TooLow { channel: 3, got: 2 })
        );
        assert_eq!(
            check(&tx_with(key, 5), 999, NonceMode::Pool, 0, seed),
            Ok(NonceStatus::Buffered { gap: 2 })
        );
        assert_eq!(
            check(&tx_with(key, 5), 999, NonceMode::Inclusion, 0, seed),
            Err(NonceError::TooHigh { channel: 3, got: 5 })
        );
    }

    #[test]
    fn nonce_free_ready_when_replay_hash_unseen() {
        let tx = tx_with(Eip8130Constants::NONCE_KEY_MAX, 0);
        assert_eq!(check(&tx, 0, NonceMode::Pool, 1_000, |_| {}), Ok(NonceStatus::Ready));
    }

    #[test]
    fn nonce_free_replay_is_rejected() {
        let now = 1_000u64;
        let tx = tx_with(Eip8130Constants::NONCE_KEY_MAX, 0);
        let replay = NonceValidator::replay_hash(&tx, ACCOUNT);
        let seed = |mgr: &mut NonceManagerStorage<'_>| {
            mgr.check_and_mark_expiring_nonce(replay, now + 20).unwrap();
        };
        assert_eq!(check(&tx, 0, NonceMode::Inclusion, now, seed), Err(NonceError::Replay));
    }
}
