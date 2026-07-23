//! Shared pure executor-calldata encoder for the unsigned and legacy phase-b tiers.

use alloy_primitives::{Address, U256};
use alloy_sol_types::{SolCall, sol};
#[cfg(feature = "phase-b")]
use base_mev_trader::BackrunPlan;

use crate::tx_authority::ValidatedAtomicCall;

sol! {
    #[allow(missing_docs)]
    struct SwapHop {
        address adapter;
        address pool;
        address tokenIn;
        address tokenOut;
        uint24 feeBps;
        uint256 minAmountOut;
        address fundingTarget;
    }

    #[allow(missing_docs)]
    function executeBlinkOfaAtomic(
        SwapHop firstHop,
        SwapHop secondHop,
        uint256 amountIn,
        uint256 minFinalAmount,
        uint256 validUntilBlock
    );
}

/// Phase-B-only scalar hop input retained for legacy byte-parity tests.
#[cfg(feature = "phase-b")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LegacyAuthorityHop {
    pub(crate) adapter: Address,
    pub(crate) pool: Address,
    pub(crate) token_in: Address,
    pub(crate) token_out: Address,
    pub(crate) fee_bps: u32,
    pub(crate) min_amount_out: U256,
    pub(crate) funding_target: Address,
}

/// Sole byte encoder for `BlinkAtomicExecutor.executeBlinkOfaAtomic`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AtomicCalldataEncoder;

impl AtomicCalldataEncoder {
    /// Encodes a T4b call from an opaque value minted only after authority validation.
    pub(crate) fn encode_validated(call: &ValidatedAtomicCall) -> Vec<u8> {
        let [first, second] = call.hops().map(|hop| Self::swap_hop(hop.parts()));
        Self::encode_parts(
            first,
            second,
            call.amount_in(),
            call.min_final_amount(),
            call.valid_until_block(),
        )
    }

    /// Encodes the phase-B legacy surface after its owning tier validates every scalar.
    #[cfg(feature = "phase-b")]
    pub(crate) fn encode_legacy(
        plan: &BackrunPlan,
        hops: [LegacyAuthorityHop; 2],
        valid_until_block: u64,
    ) -> Vec<u8> {
        let [first, second] = hops.map(|hop| {
            Self::swap_hop((
                hop.adapter,
                hop.pool,
                hop.token_in,
                hop.token_out,
                hop.fee_bps,
                hop.min_amount_out,
                hop.funding_target,
            ))
        });
        Self::encode_parts(first, second, plan.amount_in, plan.amount_out, valid_until_block)
    }

    fn swap_hop(
        (adapter, pool, token_in, token_out, fee_bps, min_amount_out, funding_target): (
            Address,
            Address,
            Address,
            Address,
            u32,
            U256,
            Address,
        ),
    ) -> SwapHop {
        SwapHop {
            adapter,
            pool,
            tokenIn: token_in,
            tokenOut: token_out,
            feeBps: alloy_primitives::aliases::U24::from(fee_bps),
            minAmountOut: min_amount_out,
            fundingTarget: funding_target,
        }
    }

    fn encode_parts(
        first_hop: SwapHop,
        second_hop: SwapHop,
        amount_in: U256,
        min_final_amount: U256,
        valid_until_block: u64,
    ) -> Vec<u8> {
        executeBlinkOfaAtomicCall {
            firstHop: first_hop,
            secondHop: second_hop,
            amountIn: amount_in,
            minFinalAmount: min_final_amount,
            validUntilBlock: U256::from(valid_until_block),
        }
        .abi_encode()
    }
}
