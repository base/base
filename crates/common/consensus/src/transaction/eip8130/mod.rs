//! [EIP-8130] Account Abstraction by Account Configuration transaction type.
//!
//! Provides type-only plumbing for the new transaction kind:
//! [`TxEip8130`] (unsigned), [`Eip8130Signed`] (signed envelope), [`AccountChange`]
//! (tagged-union account-mutation entries), and [`Call`] (per-call payload).
//!
//! [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130

mod constants;
pub use constants::Eip8130Constants;

mod addresses;
pub use addresses::Eip8130Contracts;

mod call;
pub use call::Call;

mod account_changes;
pub use account_changes::{
    AccountChange, ActorChange, ActorChangeType, ConfigChange, CreateEntry, Delegation,
    InitialActor, Scope,
};

mod tx;
pub use tx::TxEip8130;

mod signed;
pub use signed::Eip8130Signed;
