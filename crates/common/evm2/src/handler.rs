//! Base transaction handler hooks.

use alloy_consensus::Transaction;
use alloy_primitives::{Address, U256};
use base_common_consensus::Predeploys;
use base_common_genesis::BaseUpgrade;
use evm2::{
    Evm, EvmFeatures, TxResult,
    ethereum::{charge_upfront, default_settle_gas},
    handler::{GasSettlement, TxHandlerHooks},
    interpreter::Host,
    registry::{HandlerError, HandlerResult},
};

use crate::{BaseEvmTypes, transaction::BaseTxEnvelope};

/// Base transaction handler hooks.
///
/// For non-deposit transactions these charge the OP-stack L1 data fee and the Isthmus operator
/// fee alongside the standard upfront gas cost, and distribute the collected fees to their
/// vaults at settlement (L1 data fee → [`Predeploys::L1_FEE_VAULT`], base fee →
/// [`Predeploys::BASE_FEE_VAULT`], operator fee → [`Predeploys::OPERATOR_FEE_VAULT`]); the
/// coinbase priority fee and caller gas refund keep the default Ethereum behavior. Deposits are
/// funded on L1 and exempt (and never routed through these hooks).
#[derive(Clone, Copy, Debug, Default)]
pub struct BaseTxHandlerHooks;

impl BaseTxHandlerHooks {
    /// Returns the L1 data fee for `envelope` at the active Base upgrade, priced over the full
    /// EIP-2718 transaction. Deposits are funded on L1 and exempt (zero).
    fn l1_fee(host: &mut Evm<'_, BaseEvmTypes>, envelope: &BaseTxEnvelope) -> U256 {
        let Some(enveloped) = envelope.enveloped() else {
            return U256::ZERO;
        };
        let upgrade = host.config_spec_id().upgrade();
        host.block_env().ext.calculate_tx_l1_cost(enveloped, upgrade)
    }

    /// Returns the Isthmus operator fee for `envelope` over `gas`. Zero for deposits and before
    /// Isthmus.
    ///
    /// The operator fee is an Isthmus feature; `L1FeeParams::operator_fee_charge` computes it
    /// unconditionally, so the Isthmus gate is applied here (matching the reference, which gates
    /// in its `reward_beneficiary`). The Base upgrade ladder is discriminant-ordered.
    fn operator_fee(host: &mut Evm<'_, BaseEvmTypes>, envelope: &BaseTxEnvelope, gas: u64) -> U256 {
        let Some(enveloped) = envelope.enveloped() else {
            return U256::ZERO;
        };
        let upgrade = host.config_spec_id().upgrade();
        if (upgrade as u8) < (BaseUpgrade::Isthmus as u8) {
            return U256::ZERO;
        }
        host.block_env().ext.operator_fee_charge(enveloped, U256::from(gas), upgrade)
    }

    /// Returns the transaction's gas limit.
    fn gas_limit(envelope: &BaseTxEnvelope) -> u64 {
        match envelope {
            BaseTxEnvelope::Standard { tx, .. } => tx.gas_limit(),
            BaseTxEnvelope::Deposit(tx) => tx.gas_limit,
        }
    }

    /// Rejects the transaction when `caller` cannot cover the full upfront charge — the maximum
    /// gas cost, the transferred value, and the OP-stack L1 data and operator fees.
    ///
    /// The framework's `validate_sender` already rejects a caller that cannot cover the gas cost
    /// and value, but it is unaware of the L1 and operator fees charged here, and
    /// [`charge_upfront`] deducts with wrapping arithmetic. Without this guard an underfunded
    /// caller would silently wrap to a spurious balance instead of the transaction failing; this
    /// mirrors the revm reference, which rejects such a caller with `LackOfFundForMaxFee`.
    ///
    /// A no-op for deposits (funded on L1 and exempt) and when fee charging or balance checks are
    /// disabled (e.g. `eth_call` simulation), matching the conditions under which `validate_sender`
    /// and `charge_upfront` themselves enforce balances.
    fn ensure_can_pay_fees(
        host: &mut Evm<'_, BaseEvmTypes>,
        envelope: &BaseTxEnvelope,
        caller: Address,
        l1_fee: U256,
        operator_fee: U256,
    ) -> HandlerResult<()> {
        let Some(tx) = envelope.as_standard() else {
            return Ok(());
        };
        if !host.feature(EvmFeatures::FEE_CHARGE) || !host.feature(EvmFeatures::BALANCE_CHECK) {
            return Ok(());
        }
        let required = U256::from(tx.gas_limit())
            .saturating_mul(U256::from(tx.max_fee_per_gas()))
            .saturating_add(tx.value())
            .saturating_add(l1_fee)
            .saturating_add(operator_fee);
        let balance =
            host.state_mut().account(&caller, false).map_err(HandlerError::Fatal)?.balance();
        if balance < required {
            return Err(HandlerError::InsufficientFunds);
        }
        Ok(())
    }

    /// Credits `amount` to `recipient`'s balance.
    fn credit(
        host: &mut Evm<'_, BaseEvmTypes>,
        recipient: Address,
        amount: U256,
    ) -> HandlerResult<()> {
        host.state_mut()
            .account(&recipient, false)
            .map_err(HandlerError::Fatal)?
            .add_balance(amount);
        Ok(())
    }
}

impl TxHandlerHooks<BaseEvmTypes> for BaseTxHandlerHooks {
    fn before_execution(
        host: &mut Evm<'_, BaseEvmTypes>,
        envelope: &BaseTxEnvelope,
        caller: Address,
        upfront_fee: U256,
    ) -> HandlerResult<()> {
        // Charge the upfront gas cost, the L1 data fee, and the operator fee (on the gas limit)
        // from the caller. Both fees are stashed for settlement so it charges/refunds against the
        // exact amounts collected here, without recomputing them.
        let l1_fee = Self::l1_fee(host, envelope);
        let operator_fee = Self::operator_fee(host, envelope, Self::gas_limit(envelope));
        let ext = host.ext_mut();
        ext.l1_fee = l1_fee;
        ext.operator_fee = operator_fee;
        // `validate_sender` only checks the gas cost and value; the L1 and operator fees are
        // charged on top here via `charge_upfront`'s wrapping subtraction, so reject an
        // underfunded caller up front rather than let its balance wrap to a spurious value.
        Self::ensure_can_pay_fees(host, envelope, caller, l1_fee, operator_fee)?;
        charge_upfront(
            host,
            caller,
            upfront_fee.saturating_add(l1_fee).saturating_add(operator_fee),
        )
    }

    fn settle_transaction(
        host: &mut Evm<'_, BaseEvmTypes>,
        envelope: &BaseTxEnvelope,
        gas: GasSettlement<BaseEvmTypes>,
    ) -> HandlerResult<TxResult<BaseEvmTypes>> {
        let caller = gas.caller;
        let basefee = host.block_env().basefee;
        let l1_fee = host.ext().l1_fee;
        // The operator fee charged upfront (on the gas limit), stashed in before_execution.
        let operator_fee_charged = host.ext().operator_fee;

        // Default settlement refunds the caller's unused gas and pays the coinbase the priority
        // fee. The base fee is credited to its vault below (the OP-stack does not burn it).
        let mut result = default_settle_gas(host, gas)?;
        let gas_used = result.tx_gas_used();

        // Refund the operator fee down from the amount charged (on the gas limit) to the gas
        // actually used; the net is credited to the operator-fee vault below.
        let operator_fee_used = Self::operator_fee(host, envelope, gas_used);
        let operator_fee_refund = operator_fee_charged.saturating_sub(operator_fee_used);
        Self::credit(host, caller, operator_fee_refund)?;

        // Distribute the collected OP-stack fees to their vaults.
        Self::credit(host, Predeploys::L1_FEE_VAULT, l1_fee)?;
        Self::credit(
            host,
            Predeploys::BASE_FEE_VAULT,
            basefee.saturating_mul(U256::from(gas_used)),
        )?;
        Self::credit(host, Predeploys::OPERATOR_FEE_VAULT, operator_fee_used)?;

        result.ext.l1_fee = l1_fee;
        Ok(result)
    }
}
