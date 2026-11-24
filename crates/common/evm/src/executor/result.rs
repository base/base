//! Contains the [`BaseTxResult`] type.

use alloy_consensus::{Transaction, TransactionEnvelope};
use alloy_eips::Encodable2718;
use alloy_evm::{block::TxResult as TxResultTrait, eth::EthTxResult};
use alloy_primitives::{Address, U256};
use base_common_consensus::Predeploys;
use base_common_flz::{flz_compress_len, tx_estimated_size_fjord_from_fastlz_size};
use revm::{context::result::ResultAndState, state::AccountInfo};

/// The result of executing a Base transaction.
#[derive(Debug)]
pub struct BaseTxResult<H, T> {
    /// The inner result of the transaction execution.
    pub inner: EthTxResult<H, T>,
    /// Whether the transaction is a deposit transaction.
    pub is_deposit: bool,
    /// The sender of the transaction.
    pub sender: Address,
    /// The depositor account info, fetched during execution for post-Regolith deposit nonce.
    pub depositor: Option<AccountInfo>,
    /// The Base transaction `committedPhases` of the transaction: how many leading call phases
    /// committed. Zero for every other transaction type. Carried with the result so the receipt
    /// can be built on a different thread from the one that executed the transaction.
    pub base_tx_committed_phases: u64,
    /// The estimated DA size of a transaction whose result was reused from speculative prewarm
    /// execution instead of executed by the block executor, `None` otherwise.
    ///
    /// Its DA footprint needs the block's DA footprint gas scalar, so the executor adds it when
    /// the block finishes, and `inner.blob_gas_used` is zero.
    pub reused_da_size: Option<u64>,
}

impl<H, T> BaseTxResult<H, T> {
    /// Lowest gas a non-deposit transaction can use after refunds: the 21,000 intrinsic gas less
    /// the largest refund, a fifth of the gas spent.
    const MIN_REFUNDED_GAS_USED: u64 = 16_800;

    /// Wraps `result`, a speculative prewarm execution result of `tx` whose recorded reads match
    /// the current state, for commit without executing `tx`, or returns `None` if `tx` must be
    /// executed instead.
    ///
    /// Engine validation skips the block executor for reused results, so this rejects what the
    /// executor handles differently from a plain [`Evm::transact`]:
    /// - every transaction type other than legacy, EIP-2930, EIP-1559 and EIP-7702, including
    ///   deposits and Base transactions, whose results need the depositor nonce, payer gas
    ///   reservation or committed phases from execution;
    /// - results whose caller could not have paid the L1 data fee and operator fee on top of the
    ///   gas limit and value. Prewarm checks these separately, execution checks their sum.
    ///
    /// The DA footprint and its block limit are applied when the block finishes, see
    /// [`Self::reused_da_size`].
    ///
    /// [`Evm::transact`]: alloy_evm::Evm::transact
    pub fn from_prewarm<Tx>(tx: &Tx, result: ResultAndState<H>) -> Option<Self>
    where
        Tx: Transaction + Encodable2718 + TransactionEnvelope<TxType = T>,
    {
        if !matches!(tx.ty(), 0..=2 | 4) {
            return None;
        }

        // Execution bumps the caller's nonce from `tx.nonce()`, which the reuse check confirmed
        // the caller had, so the caller is among these accounts. Others match only when created
        // or authorized by a transaction with the same nonce; such results are not reused.
        let nonce = tx.nonce();
        let mut bumped = result.state.iter().filter(|(_, account)| {
            account.original_info().nonce == nonce && account.info.nonce > nonce
        });
        let (&sender, caller) = bumped.next()?;
        if bumped.next().is_some() {
            return None;
        }

        // Fees credited to a vault by the transaction. A transaction that also drains the vault
        // hides its credit, so its result is not reused.
        let credit = |vault| {
            result.state.get(&vault).map_or(Some(U256::ZERO), |account| {
                account.info.balance.checked_sub(account.original_info().balance)
            })
        };
        let gas_limit = U256::from(tx.gas_limit());
        let l1_fee = credit(Predeploys::L1_FEE_VAULT)?;
        // The operator fee is linear in gas with a non-negative constant, so scaling the fee
        // charged for the gas used up to the gas limit bounds the fee for the gas limit.
        let operator_fee = credit(Predeploys::OPERATOR_FEE_VAULT)?
            .checked_mul(gas_limit)?
            .div_ceil(U256::from(Self::MIN_REFUNDED_GAS_USED));
        let required = gas_limit
            .checked_mul(U256::from(tx.max_fee_per_gas()))?
            .checked_add(tx.value())?
            .checked_add(l1_fee)?
            .checked_add(operator_fee)?;
        if caller.original_info().balance < required {
            return None;
        }

        let fastlz_size = flz_compress_len(&tx.encoded_2718());
        Some(Self {
            inner: EthTxResult { result, blob_gas_used: 0, tx_type: tx.tx_type() },
            is_deposit: false,
            sender,
            depositor: None,
            base_tx_committed_phases: 0,
            reused_da_size: Some(
                tx_estimated_size_fjord_from_fastlz_size(fastlz_size).saturating_div(1_000_000),
            ),
        })
    }
}

impl<H: Send + 'static, T: Send + 'static> TxResultTrait for BaseTxResult<H, T> {
    type HaltReason = H;

    fn result(&self) -> &ResultAndState<Self::HaltReason> {
        &self.inner.result
    }

    fn into_result(self) -> ResultAndState<Self::HaltReason> {
        self.inner.result
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Sealed, SignableTransaction, TxLegacy};
    use alloy_primitives::{B256, Bytes, Signature, TxKind};
    use base_common_consensus::{BaseTxEnvelope, TxDeposit};
    use revm::{
        context::result::{ExecutionResult, HaltReason, Output, ResultGas, SuccessReason},
        state::{Account, EvmState},
    };
    use rstest::rstest;

    use super::*;

    const CALLER: Address = Address::repeat_byte(0x11);
    /// Also the most the transfer can cost in gas, at a gas price of one and no value.
    const GAS_LIMIT: u64 = 100_000;
    const L1_FEE: u64 = 1_000;
    /// Operator fee charged for the gas used; scaled up to the gas limit it is bounded by 1,000.
    const OPERATOR_FEE: u64 = 168;

    fn transfer() -> BaseTxEnvelope {
        BaseTxEnvelope::Legacy(
            TxLegacy {
                gas_price: 1,
                gas_limit: GAS_LIMIT,
                to: TxKind::Call(Address::repeat_byte(0x22)),
                ..Default::default()
            }
            .into_signed(Signature::new(
                Default::default(),
                Default::default(),
                Default::default(),
            )),
        )
    }

    /// An account loaded with `info` whose nonce the transaction bumped.
    fn bumped(info: AccountInfo) -> Account {
        let mut account = Account::from(info);
        account.info.nonce += 1;
        account
    }

    /// A vault holding `before`, credited or drained to `after`.
    fn vault(before: u64, after: u64) -> Account {
        let mut account =
            Account::from(AccountInfo { balance: U256::from(before), ..Default::default() });
        account.info.balance = U256::from(after);
        account
    }

    /// A successful prewarm result of [`transfer`] with these account changes.
    fn prewarmed(
        accounts: impl IntoIterator<Item = (Address, Account)>,
    ) -> ResultAndState<HaltReason> {
        let state: EvmState = accounts.into_iter().collect();
        ResultAndState::new(
            ExecutionResult::Success {
                reason: SuccessReason::Stop,
                gas: ResultGas::new_with_state_gas(21_000, 0, 0, 0),
                logs: Vec::new(),
                output: Output::Call(Bytes::new()),
            },
            state,
        )
    }

    /// A result is reused only if the caller could pay the gas limit, the L1 fee and the
    /// operator fee bounded at the gas limit.
    #[rstest]
    #[case::covers_every_fee(GAS_LIMIT + L1_FEE + 1_000, OPERATOR_FEE, true)]
    #[case::misses_operator_fee_bound(GAS_LIMIT + L1_FEE + 999, OPERATOR_FEE, false)]
    #[case::covers_gas_and_l1_fee(GAS_LIMIT + L1_FEE, 0, true)]
    #[case::misses_l1_fee(GAS_LIMIT + L1_FEE - 1, 0, false)]
    fn reuses_only_affordable_results(
        #[case] balance: u64,
        #[case] operator_fee: u64,
        #[case] reused: bool,
    ) {
        let result = BaseTxResult::from_prewarm(
            &transfer(),
            prewarmed([
                (
                    CALLER,
                    bumped(AccountInfo { balance: U256::from(balance), ..Default::default() }),
                ),
                (Predeploys::L1_FEE_VAULT, vault(5, 5 + L1_FEE)),
                (Predeploys::OPERATOR_FEE_VAULT, vault(0, operator_fee)),
            ]),
        );

        assert_eq!(result.is_some(), reused);
        if let Some(result) = result {
            assert_eq!(result.sender, CALLER);
            assert_eq!(result.inner.blob_gas_used, 0, "the executor adds the DA footprint");
            assert!(result.reused_da_size.is_some_and(|size| size > 0));
        }
    }

    /// Results whose caller or fees cannot be read from the state, and deposits, are executed.
    #[rstest]
    #[case::two_accounts_bumped_from_the_tx_nonce([
        (CALLER, bumped(AccountInfo { balance: U256::MAX >> 1, ..Default::default() })),
        (Address::repeat_byte(0x33), bumped(AccountInfo::default())),
    ])]
    #[case::vault_drained([
        (CALLER, bumped(AccountInfo { balance: U256::MAX >> 1, ..Default::default() })),
        (Predeploys::L1_FEE_VAULT, vault(10, 5)),
    ])]
    fn rejects_unreadable_results(#[case] accounts: [(Address, Account); 2]) {
        assert!(BaseTxResult::from_prewarm(&transfer(), prewarmed(accounts)).is_none());
    }

    #[test]
    fn deposits_are_executed() {
        let deposit = BaseTxEnvelope::Deposit(Sealed::new_unchecked(
            TxDeposit { from: CALLER, gas_limit: GAS_LIMIT, ..Default::default() },
            B256::repeat_byte(0xd1),
        ));
        let accounts =
            [(CALLER, bumped(AccountInfo { balance: U256::MAX >> 1, ..Default::default() }))];

        assert!(BaseTxResult::from_prewarm(&deposit, prewarmed(accounts)).is_none());
    }
}
