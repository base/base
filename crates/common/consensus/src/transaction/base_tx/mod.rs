//! Base transaction type (native account abstraction).
//!
//! Provides type-only plumbing for the new transaction kind:
//! [`TxBase`] (unsigned), [`TxBaseSigned`] (signed envelope), [`AccountChange`]
//! (typed account-change entries), and [`Call`] (per-call payload),
//! plus [`CoinbaseTip`] for static phase-0 tip recovery.

mod constants;
pub use constants::TxBaseConstants;

mod call;
pub use call::Call;

mod account_changes;
pub use account_changes::{AccountChange, Delegation};

mod tx;
pub use tx::TxBase;

#[cfg(feature = "serde")]
mod payer_serde;
#[cfg(feature = "serde")]
pub use payer_serde::TxBasePayerSerde;

mod coinbase_tip;
pub use coinbase_tip::CoinbaseTip;

mod signed;
pub use signed::{TxBaseSigned, TxBaseStaticError, TxBaseTimestampError};

mod structure;
pub use structure::{TxBaseStructuralError, TxBaseStructure};
