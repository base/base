//! Block Access List (BAL) data structures for efficient state access in blockchain execution.
//!
//! This module provides types for managing Block Access Lists, which optimize state access
//! by pre-computing and organizing data that will be accessed during block execution.
//!
//! ## Key Types
//!
//! - [`BlockAccessIndex`]: block access index
//! - [`Bal`]: Main BAL structure containing a map of accounts
//! - [`BalChanges<T>`]: Chronological list of [`BalChange`] items recording sequential writes to a
//!   state item
//! - [`AccountBal`]: Complete BAL structure for an account (balance, nonce, code, and storage)
//! - [`AccountInfoBal`]: Account info BAL data (nonce, balance, code)
//! - [`StorageBal`]: Storage-level BAL data for an account
//! - [`BalContext`]: attached read BAL plus optional builder, carried by the database wrapper
//! - [`BalError`]: lookup failures against an attached BAL

mod account;
mod bal_context;
mod changes;
mod error;
mod list;

pub use account::{AccountBal, AccountInfoBal, StorageBal};
pub use alloy_eip7928::BlockAccessIndex;
pub use bal_context::BalContext;
pub use changes::{BalChange, BalChanges, BalCodeChange};
pub use error::BalError;
pub use list::Bal;
