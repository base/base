//! Utilities for dealing with eth_call and adjacent RPC endpoints.

use alloy_primitives::U256;
use revm::Database;

/// Insufficient funds error
#[derive(Debug, thiserror::Error)]
#[error("insufficient funds: cost {cost} > balance {balance}")]
pub struct InsufficientFundsError {
    /// Transaction cost
    pub cost: U256,
    /// Account balance
    pub balance: U256,
}

/// Error type for call utilities
#[derive(Debug, thiserror::Error)]
pub enum CallError<E> {
    /// Database error
    #[error(transparent)]
    Database(E),
    /// Insufficient funds error
    #[error(transparent)]
    InsufficientFunds(#[from] InsufficientFundsError),
}

/// Calculates the caller gas allowance.
///
/// `allowance = (account.balance - tx.value) / tx.gas_price`
///
/// Returns an error if the caller has insufficient funds.
/// Caution: This assumes non-zero `env.gas_price`. Otherwise, zero allowance will be returned.
///
/// Note: this takes the mut [Database] trait because the loaded sender can be reused for the
/// following operation like `eth_call`.
pub fn caller_gas_allowance<DB, T>(db: &mut DB, env: &T) -> Result<u64, CallError<DB::Error>>
where
    DB: Database,
    T: revm::context_interface::Transaction,
{
    // Get the caller account.
    let caller = db.basic(env.caller()).map_err(CallError::Database)?;
    // Get the caller balance.
    let balance = caller.map(|acc| acc.balance).unwrap_or_default();
    // Get transaction value.
    let value = env.value();
    // Subtract transferred value from the caller balance. Return error if the caller has
    // insufficient funds.
    let balance =
        balance.checked_sub(env.value()).ok_or(InsufficientFundsError { cost: value, balance })?;

    Ok(balance
        // Calculate the amount of gas the caller can afford with the specified gas price.
        .checked_div(U256::from(env.gas_price()))
        // This will be 0 if gas price is 0. It is fine, because we check it before.
        .unwrap_or_default()
        .saturating_to())
}
