//! Errors returned by Block Access List lookups.

use alloy_primitives::{Address, U256};

/// Error returned when a BAL (Block Access List, [EIP-7928]) lookup
/// cannot find data the caller expected to be present.
///
/// A BAL is supposed to enumerate every account and storage slot a block
/// will touch, so when execution queries the BAL for an entry that is
/// missing, the BAL is either malformed or being consulted for state that
/// it does not cover. Each variant identifies which kind of lookup failed
/// and carries the key that was queried so callers can report it.
///
/// [EIP-7928]: https://eips.ethereum.org/EIPS/eip-7928
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BalError {
    /// The address was not present in the BAL's accounts map.
    ///
    /// Returned by address-keyed lookups when the BAL is attached but does not
    /// list this account. Means the BAL is incomplete for the access being
    /// attempted.
    AccountNotFound {
        /// Address that was not found.
        address: Address,
    },
    /// The account exists in the BAL but the requested storage slot is not
    /// listed under it.
    ///
    /// Returned by storage lookups when the account is covered by the BAL
    /// yet this particular slot was not declared. As with
    /// [`BalError::AccountNotFound`], this indicates the BAL is incomplete
    /// for the access being attempted.
    SlotNotFound {
        /// Address of the account whose slot was missing.
        address: Address,
        /// Storage slot that was not found.
        slot: U256,
    },
}

impl core::fmt::Display for BalError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::AccountNotFound { address } => {
                write!(f, "Account {address} not found in BAL")
            }
            Self::SlotNotFound { address, slot } => {
                write!(f, "Slot {slot:#x} not found in BAL for account {address}")
            }
        }
    }
}

impl core::error::Error for BalError {}
