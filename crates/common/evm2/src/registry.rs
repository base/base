//! Base transaction registry.

use alloy_primitives::U256;
use evm2::{
    Evm, TxResult,
    env::TxEnvExt,
    ethereum::{
        TxEnvelope, eip1559, eip2930, eip7702, execute_initial_frame, finalize_gas,
        initial_gas_and_reservoir, intrinsic_gas, legacy, prepare_initial_frame,
        warm_base_accounts,
    },
    handler::GasSettlement,
    interpreter::{GasTracker, InstrStop},
    registry::{HandlerError, HandlerResult, TxRegistry, TxRequest},
};

use crate::{
    BaseEvmTypes, BaseTxHandlerHooks,
    transaction::{BaseTxEnvelope, DEPOSIT_TX_TYPE, TxDeposit},
};

impl BaseEvmTypes {
    /// Builds the Base transaction registry.
    ///
    /// Registers the deposit handler (type `0x7e`) and the standard Ethereum
    /// transaction handlers (legacy/2930/1559/7702; EIP-4844 blob transactions
    /// are unsupported on Base and intentionally omitted), the latter wired with
    /// [`BaseTxHandlerHooks`] so non-deposit transactions run the L1 fee
    /// settlement path.
    pub fn tx_registry() -> TxRegistry<Self, TxResult<Self>> {
        let mut registry = TxRegistry::new().with_handler(
            DEPOSIT_TX_TYPE,
            BaseTxEnvelope::as_deposit,
            Self::handle_deposit,
        );
        registry.register(
            0,
            |tx: &BaseTxEnvelope| tx.as_standard().and_then(TxEnvelope::as_legacy),
            legacy::handle_with_hooks::<Self, BaseTxHandlerHooks>,
        );
        registry.register(
            1,
            |tx: &BaseTxEnvelope| tx.as_standard().and_then(TxEnvelope::as_eip2930),
            eip2930::handle_with_hooks::<Self, BaseTxHandlerHooks>,
        );
        registry.register(
            2,
            |tx: &BaseTxEnvelope| tx.as_standard().and_then(TxEnvelope::as_eip1559),
            eip1559::handle_with_hooks::<Self, BaseTxHandlerHooks>,
        );
        // Type 3 (EIP-4844 blob transactions) is intentionally not registered:
        // Base rejects blob transactions, so no handler exists for them.
        registry.register(
            4,
            |tx: &BaseTxEnvelope| tx.as_standard().and_then(TxEnvelope::as_eip7702),
            eip7702::handle_with_hooks::<Self, BaseTxHandlerHooks>,
        );
        registry
    }

    /// Executes a deposit transaction, mirroring the OP-stack deposit rules of the revm
    /// execution path (`base-common-evm`), for which Base is always Regolith-active.
    ///
    /// The minted value is credited to the sender before execution and kept regardless of the
    /// outcome. System-transaction deposits are rejected (post-Regolith) and settled as failed
    /// deposits. Deposits are exempt from the L1 data fee, the operator fee, and the
    /// beneficiary reward, so gas is finalized without charging any fee. A halted deposit is
    /// charged the full gas limit — there is no L2 fee payer to refund — while a reverted
    /// deposit reports its actual gas, like any transaction.
    pub fn handle_deposit(
        req: TxRequest<'_, '_, Self, TxDeposit>,
    ) -> HandlerResult<TxResult<Self>> {
        let host = req.host;
        let tx: &TxDeposit = *req.tx;

        // Credit the mint and bump the sender nonce, capturing the pre-bump nonce for a create
        // deposit's contract-address derivation.
        let nonce = Self::prepare_deposit_sender(host, tx)?;

        // System-transaction deposits are rejected post-Regolith (always active on Base).
        if tx.is_system_transaction {
            return Ok(Self::failed_deposit(tx));
        }

        // Pre-warm the sender, destination, coinbase, and precompiles, matching the standard
        // transaction handlers so warm/cold (EIP-2929) access gas agrees with the reference.
        warm_base_accounts(host, tx.from, tx.to);

        // Meter the deposit like a standard transaction: charge intrinsic gas up front, then
        // run the call/create frame with the remaining gas. A deposit that cannot afford its
        // intrinsic cost is settled as a failed deposit.
        let intrinsic = intrinsic_gas(host.version(), tx.from, tx.to, &tx.input, 0, 0, tx.value);
        if tx.gas_limit < intrinsic {
            return Ok(Self::failed_deposit(tx));
        }
        let (execution_gas_limit, reservoir) =
            initial_gas_and_reservoir(host.version(), tx.gas_limit, intrinsic, 0);
        let mut tx_gas =
            GasTracker::new_with_execution_gas_and_reservoir(execution_gas_limit, reservoir);
        // Deposits cannot fail fatally per the OP-stack spec (the revm reference catches every
        // transaction-level error in `catch_error` and returns a failed deposit); only a database
        // error is genuinely fatal. `prepare_initial_frame` only produces `Fatal` errors today,
        // but matching on the variant future-proofs against evm2 introducing typed frame errors:
        // a `Fatal` propagates, anything else settles as a failed deposit.
        let frame = match prepare_initial_frame(
            host,
            tx.from,
            nonce,
            tx.to,
            &tx.input,
            tx.value,
            &mut tx_gas,
        ) {
            Ok(frame) => frame,
            Err(err @ HandlerError::Fatal(_)) => return Err(err),
            Err(_) => return Ok(Self::failed_deposit(tx)),
        };
        let tx_env = TxEnvExt {
            origin: tx.from,
            gas_price: U256::ZERO,
            chain_id: U256::from(host.version().chain_id),
            ..TxEnvExt::default()
        };
        let result = execute_initial_frame(
            host,
            &tx_env,
            frame,
            &mut tx_gas,
            execution_gas_limit,
            reservoir,
        );

        // A halt has no L2 fee payer to refund, so it is charged the full gas limit. evm2 has
        // already rolled back the execution state; the mint and nonce bump are retained.
        if result.stop.is_halt() {
            return Ok(Self::failed_deposit(tx));
        }

        // Success or revert: finalize gas like a standard transaction, but skip the fee charge
        // and beneficiary reward (deposits are funded on L1). As with prepare_initial_frame, a
        // database error propagates as fatal while any other error settles as a failed deposit —
        // deposits never fail fatally per the OP-stack spec. finalize_gas only surfaces `Fatal`
        // errors today; the match makes the invariant explicit and future-proofs it.
        match finalize_gas(
            host,
            GasSettlement {
                caller: tx.from,
                gas_price: U256::ZERO,
                gas_limit: tx.gas_limit,
                floor_gas: 0,
                initial_state_gas: 0,
                state_refund: 0,
                result,
            },
        ) {
            Ok(result) => Ok(result),
            Err(err @ HandlerError::Fatal(_)) => Err(err),
            Err(_) => Ok(Self::failed_deposit(tx)),
        }
    }

    /// Credits a deposit's minted value to the sender and bumps its nonce, returning the
    /// sender's pre-bump nonce (used to derive a create deposit's contract address).
    ///
    /// The mint is added before execution and retained regardless of the deposit's outcome.
    /// The nonce is bumped once here (mirroring the standard-transaction handlers; evm2 only
    /// bumps automatically for nested creates).
    pub fn prepare_deposit_sender(host: &mut Evm<'_, Self>, tx: &TxDeposit) -> HandlerResult<u64> {
        let mut account = host.state_mut().account(&tx.from, false).map_err(HandlerError::Fatal)?;
        account.add_balance(U256::from(tx.mint));
        let nonce = account.nonce();
        account.bump_nonce();
        Ok(nonce)
    }

    /// Builds the result for a failed deposit: status `false`, the full gas limit charged, and
    /// no output.
    ///
    /// The sender's mint and nonce bump are applied by
    /// [`prepare_deposit_sender`](Self::prepare_deposit_sender) and any execution state has
    /// already been rolled back, so only the result is assembled here. The halt reason is
    /// reported uniformly as [`InstrStop::OutOfGas`] since the full gas limit is consumed.
    pub fn failed_deposit(tx: &TxDeposit) -> TxResult<Self> {
        TxResult::<Self> {
            status: false,
            total_gas_spent: tx.gas_limit,
            stop: InstrStop::OutOfGas,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::transaction::Recovered;
    use alloy_primitives::{Address, Bytes, TxKind, U256};
    use base_common_genesis::BaseUpgrade;
    use evm2::{Evm, Precompiles, env::BlockEnv, evm::InMemoryDB};

    use super::*;
    use crate::BaseSpecId;

    const SENDER: Address = Address::repeat_byte(0x11);
    const TARGET: Address = Address::repeat_byte(0x22);
    const SPEC: BaseSpecId = BaseSpecId::new(BaseUpgrade::Regolith);

    /// Builds an in-memory EVM wired with the Base transaction registry.
    fn new_evm() -> Evm<'static, BaseEvmTypes> {
        Evm::new(
            SPEC,
            BlockEnv::<BaseEvmTypes>::default(),
            BaseEvmTypes::tx_registry(),
            InMemoryDB::default(),
            Precompiles::base(SPEC.into()),
        )
    }

    /// Builds a deposit with the shared `SENDER`, a 100k gas limit, and the given fields.
    fn deposit(to: TxKind, mint: u128, value: U256, input: Bytes, is_system: bool) -> TxDeposit {
        TxDeposit {
            from: SENDER,
            to,
            mint,
            value,
            gas_limit: 100_000,
            input,
            is_system_transaction: is_system,
            ..Default::default()
        }
    }

    /// Routes `tx` through the registry and commits the resulting state changes.
    fn run(evm: &mut Evm<'static, BaseEvmTypes>, tx: TxDeposit) -> TxResult<BaseEvmTypes> {
        let recovered = Recovered::new_unchecked(BaseTxEnvelope::Deposit(tx), SENDER);
        evm.transact(&recovered).expect("deposit must not fatally error").commit()
    }

    fn balance(evm: &mut Evm<'static, BaseEvmTypes>, addr: Address) -> U256 {
        evm.state_mut()
            .account_info_untracked(&addr)
            .unwrap()
            .map(|i| i.balance)
            .unwrap_or_default()
    }

    fn nonce(evm: &mut Evm<'static, BaseEvmTypes>, addr: Address) -> u64 {
        evm.state_mut().account_info_untracked(&addr).unwrap().map(|i| i.nonce).unwrap_or_default()
    }

    #[test]
    fn registry_registers_deposit_and_standard_handlers() {
        let registry = BaseEvmTypes::tx_registry();
        assert!(registry.contains(DEPOSIT_TX_TYPE), "deposit handler must be registered");
        // Legacy (0), EIP-2930 (1), EIP-1559 (2), EIP-7702 (4). Type 3 (EIP-4844
        // blob transactions) is intentionally unregistered — Base rejects them.
        for ty in [0, 1, 2, 4] {
            assert!(registry.contains(ty), "standard handler {ty} must be registered");
        }
        assert!(!registry.contains(3), "EIP-4844 blob handler must not be registered");
    }

    #[test]
    fn mint_is_credited_and_nonce_bumped() {
        let mut evm = new_evm();
        let result =
            run(&mut evm, deposit(TxKind::Call(TARGET), 1_000, U256::ZERO, Bytes::new(), false));
        assert!(result.status);
        assert_eq!(balance(&mut evm, SENDER), U256::from(1_000));
        assert_eq!(nonce(&mut evm, SENDER), 1);
    }

    #[test]
    fn value_is_transferred_to_target() {
        let mut evm = new_evm();
        let result = run(
            &mut evm,
            deposit(TxKind::Call(TARGET), 1_000, U256::from(300), Bytes::new(), false),
        );
        assert!(result.status);
        assert_eq!(balance(&mut evm, SENDER), U256::from(700));
        assert_eq!(balance(&mut evm, TARGET), U256::from(300));
    }

    #[test]
    fn system_transaction_is_rejected_with_full_gas() {
        let mut evm = new_evm();
        let tx = deposit(TxKind::Call(TARGET), 1_000, U256::ZERO, Bytes::new(), true);
        let gas_limit = tx.gas_limit;
        let result = run(&mut evm, tx);
        assert!(!result.status);
        assert_eq!(result.total_gas_spent, gas_limit);
        // The mint is credited and the nonce bumped even though execution is skipped.
        assert_eq!(balance(&mut evm, SENDER), U256::from(1_000));
        assert_eq!(nonce(&mut evm, SENDER), 1);
    }

    #[test]
    fn create_deposit_deploys_contract() {
        let mut evm = new_evm();
        // Init code `STOP` deploys empty runtime code and succeeds.
        let init = Bytes::from_static(&[0x00]);
        let result = run(&mut evm, deposit(TxKind::Create, 1_000, U256::ZERO, init, false));
        assert!(result.status);
        assert_eq!(result.created_address, Some(SENDER.create(0)));
        assert_eq!(nonce(&mut evm, SENDER), 1);
    }

    #[test]
    fn reverted_deposit_reports_actual_gas_and_keeps_mint() {
        let mut evm = new_evm();
        // Init code `PUSH1 0, PUSH1 0, REVERT` reverts during contract creation.
        let init = Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xfd]);
        let tx = deposit(TxKind::Create, 1_000, U256::ZERO, init, false);
        let gas_limit = tx.gas_limit;
        let result = run(&mut evm, tx);
        assert!(!result.status);
        // A revert is metered like any transaction, not charged the full limit.
        assert!(result.total_gas_spent < gas_limit);
        assert_eq!(balance(&mut evm, SENDER), U256::from(1_000));
    }

    #[test]
    fn halted_deposit_is_charged_full_gas_and_keeps_mint() {
        let mut evm = new_evm();
        // Init code `INVALID` halts during contract creation, consuming all gas.
        let init = Bytes::from_static(&[0xfe]);
        let tx = deposit(TxKind::Create, 1_000, U256::ZERO, init, false);
        let gas_limit = tx.gas_limit;
        let result = run(&mut evm, tx);
        assert!(!result.status);
        assert_eq!(result.total_gas_spent, gas_limit);
        assert_eq!(balance(&mut evm, SENDER), U256::from(1_000));
    }

    #[test]
    fn halted_call_deposit_rolls_back_value_transfer() {
        // Deploy `INVALID` at the target so a call to it halts *after* the value transfer,
        // exercising that the transfer is unwound (relied on for parity with the revm reference).
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            &TARGET,
            evm2::AccountInfo {
                code: Some(evm2::bytecode::Bytecode::new_legacy(Bytes::from_static(&[0xfe]))),
                ..Default::default()
            },
        );
        let mut evm = Evm::new(
            SPEC,
            BlockEnv::<BaseEvmTypes>::default(),
            BaseEvmTypes::tx_registry(),
            db,
            Precompiles::base(SPEC.into()),
        );

        let tx = deposit(TxKind::Call(TARGET), 1_000, U256::from(500), Bytes::new(), false);
        let gas_limit = tx.gas_limit;
        let result = run(&mut evm, tx);

        assert!(!result.status);
        assert_eq!(result.total_gas_spent, gas_limit);
        // The value transfer was rolled back with the halted execution: the target keeps nothing,
        // and the sender retains the full mint (value returned).
        assert_eq!(balance(&mut evm, TARGET), U256::ZERO);
        assert_eq!(balance(&mut evm, SENDER), U256::from(1_000));
    }
}
