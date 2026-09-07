//! State changes that are not related to transactions.

use super::calc;
use alloy_consensus::BlockHeader;
use alloy_eips::eip4895::Withdrawal;
use alloy_hardforks::EthereumHardforks;
use alloy_primitives::map::AddressMap;
use revm::context::Block;

/// Collect all balance changes at the end of the block.
///
/// Balance changes might include the block reward, uncle rewards, withdrawals, or irregular
/// state changes (DAO fork).
#[inline]
pub fn post_block_balance_increments<H>(
    spec: impl EthereumHardforks,
    block_env: impl Block,
    ommers: &[H],
    withdrawals: Option<&[Withdrawal]>,
) -> AddressMap<u128>
where
    H: BlockHeader,
{
    let mut balance_increments = AddressMap::with_capacity_and_hasher(
        withdrawals.map_or(ommers.len(), |w| w.len()),
        Default::default(),
    );

    // Add block rewards if they are enabled.
    if let Some(base_block_reward) =
        calc::base_block_reward(&spec, block_env.number().saturating_to())
    {
        // Ommer rewards
        for ommer in ommers {
            *balance_increments.entry(ommer.beneficiary()).or_default() += calc::ommer_reward(
                base_block_reward,
                block_env.number().saturating_to(),
                ommer.number(),
            );
        }

        // Full block reward
        *balance_increments.entry(block_env.beneficiary()).or_default() +=
            calc::block_reward(base_block_reward, ommers.len());
    }

    // process withdrawals
    insert_post_block_withdrawals_balance_increments(
        spec,
        block_env.timestamp().saturating_to(),
        withdrawals,
        &mut balance_increments,
    );

    balance_increments
}

/// Returns a map of addresses to their balance increments if the Shanghai hardfork is active at the
/// given timestamp.
///
/// Zero-valued withdrawals are filtered out.
#[inline]
pub fn post_block_withdrawals_balance_increments(
    spec: impl EthereumHardforks,
    block_timestamp: u64,
    withdrawals: &[Withdrawal],
) -> AddressMap<u128> {
    let mut balance_increments =
        AddressMap::with_capacity_and_hasher(withdrawals.len(), Default::default());
    insert_post_block_withdrawals_balance_increments(
        spec,
        block_timestamp,
        Some(withdrawals),
        &mut balance_increments,
    );
    balance_increments
}

/// Applies all withdrawal balance increments if shanghai is active at the given timestamp to the
/// given `balance_increments` map.
#[inline]
pub fn insert_post_block_withdrawals_balance_increments(
    spec: impl EthereumHardforks,
    block_timestamp: u64,
    withdrawals: Option<&[Withdrawal]>,
    balance_increments: &mut AddressMap<u128>,
) {
    // Process withdrawals
    if spec.is_shanghai_active_at_timestamp(block_timestamp) {
        if let Some(withdrawals) = withdrawals {
            for withdrawal in withdrawals {
                *balance_increments.entry(withdrawal.address).or_default() +=
                    withdrawal.amount_wei().to::<u128>();
            }
        }
    }
}
