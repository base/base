//! Receipt gas accounting.

use alloc::vec::Vec;

use base_common_types_chain::TxReceipt;

/// Retrieves gas spent by transactions as a vector of tuples (transaction index, gas used).
pub fn gas_spent_by_transactions<I, T>(receipts: I) -> Vec<(u64, u64)>
where
    I: IntoIterator<Item = T>,
    T: TxReceipt,
{
    receipts
        .into_iter()
        .enumerate()
        .map(|(id, receipt)| (id as u64, receipt.cumulative_gas_used()))
        .collect()
}
