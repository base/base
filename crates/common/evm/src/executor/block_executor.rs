//! Contains the block executor for base.

use alloc::{boxed::Box, vec::Vec};

use alloy_consensus::{Eip658Value, Header, Transaction, TransactionEnvelope, TxReceipt};
use alloy_eips::{Encodable2718, Typed2718};
use alloy_evm::{
    Database, Evm, FromRecoveredTx, FromTxWithEncoded, RecoveredTx,
    block::{
        BlockExecutionError, BlockExecutionResult, BlockExecutor, BlockValidationError,
        ExecutableTx, GasOutput, StateDB, SystemCaller,
        state_changes::post_block_balance_increments,
    },
    eth::{EthTxResult, receipt_builder::ReceiptBuilderCtx},
};
use base_common_chains::Upgrades;
use base_common_consensus::{DepositReceipt, Predeploys};
use base_common_flz::{flz_compress_len, tx_estimated_size_fjord_from_fastlz_size};
#[cfg(feature = "std")]
use base_execution_eip8130::IntrinsicGas;
use revm::{
    Database as _, DatabaseCommit,
    context::{Block, result::ResultAndState},
    context_interface::Cfg,
    database::DatabaseCommitExt,
    primitives::hardfork::SpecId,
};

use crate::{
    BaseBlockExecutionCtx, BaseBlockExecutionError, BaseReceiptBuilder, BaseTime, BaseTxEnv,
    BaseTxResult, DEPOSIT_TRANSACTION_TYPE, EIP8130_TRANSACTION_TYPE, Eip8130CommittedPhases,
    L1BlockInfo, canyon,
};

/// Block executor for Base.
#[derive(Debug)]
pub struct BaseBlockExecutor<Evm, R: BaseReceiptBuilder, Spec> {
    /// Spec.
    pub spec: Spec,
    /// Receipt builder.
    pub receipt_builder: R,
    /// Context for block execution.
    pub ctx: BaseBlockExecutionCtx,
    /// The EVM used by executor.
    pub evm: Evm,
    /// Receipts of executed transactions.
    pub receipts: Vec<R::Receipt>,
    /// Total gas used by executed transactions.
    pub gas_used: u64,
    /// Regular gas consumed before transaction refunds (EIP-7778/8037).
    pub block_regular_gas_used: u64,
    /// State gas consumed after reservoir refills (EIP-8037).
    pub block_state_gas_used: u64,
    /// DA footprint.
    ///
    /// This is only set for blocks post-Jovian activation.
    /// See [DA footprint block limit spec](https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/jovian/exec-engine.md#da-footprint-block-limit)
    pub da_footprint_used: u64,
    /// Whether Regolith upgrade is active.
    pub is_regolith: bool,
    /// Utility to call system smart contracts.
    pub system_caller: SystemCaller<Spec>,
}

impl<E, R, Spec> BaseBlockExecutor<E, R, Spec>
where
    E: Evm,
    R: BaseReceiptBuilder,
    Spec: Upgrades + Clone,
{
    /// Creates a new [`BaseBlockExecutor`].
    pub fn new(evm: E, ctx: BaseBlockExecutionCtx, spec: Spec, receipt_builder: R) -> Self {
        Self {
            is_regolith: spec
                .is_regolith_active_at_timestamp(evm.block().timestamp().saturating_to()),
            evm,
            system_caller: SystemCaller::new(spec.clone()),
            spec,
            receipt_builder,
            receipts: Vec::new(),
            gas_used: 0,
            block_regular_gas_used: 0,
            block_state_gas_used: 0,
            da_footprint_used: 0,
            ctx,
        }
    }
}

impl<E, R, Spec> BaseBlockExecutor<E, R, Spec>
where
    E: Evm<
            DB: Database + DatabaseCommit + StateDB,
            Tx: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction> + BaseTxEnv,
        >,
    R: BaseReceiptBuilder<Transaction: Transaction + Encodable2718, Receipt: TxReceipt>,
    Spec: Upgrades,
{
    /// Block gas the transaction may consume, reserved against the block gas
    /// limit before execution.
    ///
    /// For every transaction this is the declared `gas_limit`. An EIP-8130
    /// transaction additionally meters `payer_auth` *on top of* `gas_limit` (the
    /// payer reimburses its own authentication beyond the sender-signed limit),
    /// so that portion must be reserved in the block gas budget too. Reserving
    /// `gas_limit` alone could admit a transaction whose true consumption
    /// (`gas_limit + payer_auth`) pushes cumulative block gas over the limit.
    ///
    /// The payer authentication gas is a *conservative upper bound*
    /// ([`IntrinsicGas::max_payer_auth_cost`]): it prices the `payer_auth` bytes
    /// at the EIP-7623 floor rate, which settlement can only meet or undercharge.
    /// Reserving a ceiling can only over-reserve (never admit an over-limit
    /// block), and the same bound is used by block building and validation,
    /// keeping them consistent.
    ///
    /// A transaction whose payer authentication cannot be priced is invalid, so
    /// the failure is a validation error rather than an internal one.
    #[cfg(feature = "std")]
    fn reserved_block_gas(tx_env: &E::Tx, gas_limit: u64) -> Result<u64, BlockExecutionError> {
        let Some(signed) = tx_env.eip8130_signed() else {
            return Ok(gas_limit);
        };
        let payer_auth = IntrinsicGas::max_payer_auth_cost(signed)
            .map_err(|error| BlockExecutionError::Validation(BlockValidationError::other(error)))?;
        Ok(gas_limit.saturating_add(payer_auth))
    }

    /// `no_std` builds reject EIP-8130 execution outright (see `Evm::transact_raw`),
    /// so no payer authentication gas is metered on top of `gas_limit` and the
    /// reserved block gas is just the declared `gas_limit`.
    #[cfg(not(feature = "std"))]
    const fn reserved_block_gas(
        _tx_env: &E::Tx,
        gas_limit: u64,
    ) -> Result<u64, BlockExecutionError> {
        Ok(gas_limit)
    }

    fn jovian_da_footprint_estimation(
        &mut self,
        tx_env: &mut E::Tx,
        tx: impl RecoveredTx<R::Transaction>,
    ) -> Result<u64, BlockExecutionError> {
        // Use the enveloped tx if it exists, caching its FastLZ size so the L1 cost charged during
        // execution reuses it; otherwise use the encoded 2718 bytes.
        let fastlz_size = tx_env
            .cache_encoded_bytes_fastlz_size()
            .unwrap_or_else(|| flz_compress_len(tx.tx().encoded_2718().as_ref()));
        let encoded =
            tx_estimated_size_fjord_from_fastlz_size(fastlz_size).saturating_div(1_000_000);

        // Load the L1 block contract into the cache. If the L1 block contract is not pre-loaded the
        // database will panic when trying to fetch the DA footprint gas scalar.
        self.evm.db_mut().basic(Predeploys::L1_BLOCK_INFO).map_err(BlockExecutionError::other)?;

        let da_footprint_gas_scalar = L1BlockInfo::fetch_da_footprint_gas_scalar(self.evm.db_mut())
            .map_err(BlockExecutionError::other)?
            .into();

        Ok(encoded.saturating_mul(da_footprint_gas_scalar))
    }
}

impl<E, R, Spec> BlockExecutor for BaseBlockExecutor<E, R, Spec>
where
    E: Evm<
            DB: Database + DatabaseCommit + StateDB,
            Tx: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction> + BaseTxEnv,
        >,
    R: BaseReceiptBuilder<
            Transaction: Transaction + Encodable2718 + TransactionEnvelope<TxType: Send + 'static>,
            Receipt: TxReceipt,
        >,
    Spec: Upgrades,
    E::Spec: Into<SpecId>,
{
    type Transaction = R::Transaction;
    type Receipt = R::Receipt;
    type Evm = E;
    type Result = BaseTxResult<E::HaltReason, <R::Transaction as TransactionEnvelope>::TxType>;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.system_caller.apply_blockhashes_contract_call(self.ctx.parent_hash, &mut self.evm)?;
        self.system_caller
            .apply_beacon_root_contract_call(self.ctx.parent_beacon_block_root, &mut self.evm)?;

        // Ensure that the create2deployer is force-deployed at the canyon transition. Base
        // blocks will always have at least a single transaction in them (the L1 info transaction),
        // so we can safely assume that this will always be triggered upon the transition and that
        // the above check for empty blocks will never be hit on Base chains.
        canyon::ensure_create2_deployer(
            &self.spec,
            self.evm.block().timestamp().saturating_to(),
            self.evm.db_mut(),
        )
        .map_err(BlockExecutionError::other)?;

        // Install BaseTime before transactions so the activation block's metadata deposit can
        // call it.
        BaseTime::ensure_predeploy(
            &self.spec,
            self.evm.block().timestamp().saturating_to(),
            self.evm.db_mut(),
        )
        .map_err(BlockExecutionError::other)?;

        // At the Everest (EIP-8130) transition, plant a code stub on the code-less
        // enshrined system accounts (the 2D nonce manager) so the persistent state
        // the enshrined path writes to them is not reaped by EIP-161 end-of-block
        // state clearing.
        crate::everest::ensure_eip8130_system_accounts(
            &self.spec,
            self.evm.block().timestamp().saturating_to(),
            self.evm.db_mut(),
        )
        .map_err(BlockExecutionError::other)?;

        Ok(())
    }

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<Self::Result, BlockExecutionError> {
        let (mut tx_env, tx) = tx.into_parts();
        let is_deposit = tx.tx().ty() == DEPOSIT_TRANSACTION_TYPE;

        // The sum of the gas the transaction may consume, Tg, and the gas utilized in this block
        // prior, must be no greater than the block's gasLimit. For EIP-8130 the reserved amount is
        // `gas_limit + payer_auth`, since payer authentication is metered on top of the declared
        // gas_limit (see `reserved_block_gas`); for every other transaction it is `gas_limit`.
        let reserved_gas = Self::reserved_block_gas(&tx_env, tx.tx().gas_limit())?;
        // Amsterdam reserves independent regular and state budgets, as on L1.
        let amsterdam = self.evm.cfg_env().enable_amsterdam_eip8037;
        let gas_used = if amsterdam { self.block_regular_gas_used } else { self.gas_used };
        let block_available_gas = self.evm.block().gas_limit().saturating_sub(gas_used);
        // Deposits bypass the transaction cap and can report their full limit on failure.
        let regular_gas = if amsterdam && !is_deposit {
            reserved_gas.min(self.evm.cfg_env().tx_gas_limit_cap())
        } else {
            reserved_gas
        };
        if regular_gas > block_available_gas && (self.is_regolith || !is_deposit) {
            return Err(BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                transaction_gas_limit: regular_gas,
                block_available_gas,
            }
            .into());
        }
        if amsterdam {
            let state_gas_available =
                self.evm.block().gas_limit().saturating_sub(self.block_state_gas_used);
            if reserved_gas > state_gas_available {
                return Err(BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                    transaction_gas_limit: reserved_gas,
                    block_available_gas: state_gas_available,
                }
                .into());
            }
        }

        let da_footprint_used = if self
            .spec
            .is_jovian_active_at_timestamp(self.evm.block().timestamp().saturating_to())
            && !is_deposit
        {
            let da_footprint_available =
                self.evm.block().gas_limit().saturating_sub(self.da_footprint_used);

            let tx_da_footprint = self.jovian_da_footprint_estimation(&mut tx_env, &tx)?;

            if tx_da_footprint > da_footprint_available {
                return Err(BlockExecutionError::Validation(BlockValidationError::Other(
                    Box::new(BaseBlockExecutionError::TransactionDaFootprintAboveGasLimit {
                        transaction_da_footprint: tx_da_footprint,
                        available_block_da_footprint: da_footprint_available,
                    }),
                )));
            }

            tx_da_footprint
        } else {
            0
        };

        // Execute transaction and return the result
        let result = self.evm.transact(tx_env).map_err(|err| {
            let hash = tx.tx().trie_hash();
            BlockExecutionError::evm(err, hash)
        })?;
        let eip8130_committed_phases = if tx.tx().ty() == EIP8130_TRANSACTION_TYPE {
            Eip8130CommittedPhases::take()
        } else {
            0
        };

        // Fetch the depositor account from the database for the deposit nonce.
        // This *only* needs to be done post-Regolith for deposit transactions.
        let depositor = (self.is_regolith && is_deposit)
            .then(|| self.evm.db_mut().basic(*tx.signer()).map(|acc| acc.unwrap_or_default()))
            .transpose()
            .map_err(BlockExecutionError::other)?;

        Ok(BaseTxResult {
            inner: EthTxResult {
                result,
                blob_gas_used: da_footprint_used,
                tx_type: tx.tx().tx_type(),
            },
            is_deposit,
            sender: *tx.signer(),
            depositor,
            eip8130_committed_phases,
        })
    }

    fn commit_transaction(&mut self, output: Self::Result) -> GasOutput {
        let BaseTxResult {
            inner: EthTxResult { result: ResultAndState { result, state }, blob_gas_used, tx_type },
            is_deposit,
            sender: _,
            depositor,
            eip8130_committed_phases,
        } = output;

        let tx_gas_used = result.tx_gas_used();
        let state_gas_used = result.gas().block_state_gas_used();

        self.gas_used += tx_gas_used;
        self.block_regular_gas_used += result.gas().block_regular_gas_used();
        self.block_state_gas_used += state_gas_used;

        if self.spec.is_jovian_active_at_timestamp(self.evm.block().timestamp().saturating_to())
            && !is_deposit
        {
            self.da_footprint_used = self.da_footprint_used.saturating_add(blob_gas_used);
        }

        self.receipts.push(
            match self.receipt_builder.build_receipt(
                ReceiptBuilderCtx {
                    tx_type,
                    result,
                    cumulative_gas_used: self.gas_used,
                    evm: &self.evm,
                    state: &state,
                },
                eip8130_committed_phases,
            ) {
                Ok(receipt) => receipt,
                Err(ctx) => {
                    let receipt = alloy_consensus::Receipt {
                        status: Eip658Value::Eip658(ctx.result.is_success()),
                        cumulative_gas_used: ctx.cumulative_gas_used,
                        logs: ctx.result.into_logs(),
                    };

                    self.receipt_builder.build_deposit_receipt(DepositReceipt {
                        inner: receipt,
                        deposit_nonce: depositor.map(|account| account.nonce),
                        deposit_receipt_version: (is_deposit
                            && self.spec.is_canyon_active_at_timestamp(
                                self.evm.block().timestamp().saturating_to(),
                            ))
                        .then_some(1),
                    })
                }
            },
        );

        self.evm.db_mut().commit(state);

        GasOutput::with_state_gas(tx_gas_used, state_gas_used)
    }

    fn finish(
        mut self,
    ) -> Result<(Self::Evm, BlockExecutionResult<R::Receipt>), BlockExecutionError> {
        let balance_increments =
            post_block_balance_increments::<Header>(&self.spec, self.evm.block(), &[], None);

        self.evm
            .db_mut()
            .increment_balances(balance_increments)
            .map_err(|_| BlockValidationError::IncrementBalanceFailed)?;

        let legacy_gas_used =
            self.receipts.last().map(|r| r.cumulative_gas_used()).unwrap_or_default();
        let gas_used = if self.evm.cfg_env().enable_amsterdam_eip8037 {
            self.block_regular_gas_used.max(self.block_state_gas_used)
        } else {
            legacy_gas_used
        };

        Ok((
            self.evm,
            BlockExecutionResult {
                receipts: self.receipts,
                requests: Default::default(),
                gas_used,
                blob_gas_used: self.da_footprint_used,
            },
        ))
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        &mut self.evm
    }

    fn evm(&self) -> &Self::Evm {
        &self.evm
    }

    fn receipts(&self) -> &[Self::Receipt] {
        &self.receipts
    }
}

#[cfg(test)]
mod tests {
    use alloc::{string::ToString, vec};

    use alloy_consensus::{SignableTransaction, TxLegacy, transaction::Recovered};
    use alloy_eips::eip2718::WithEncoded;
    use alloy_evm::{
        EvmEnv, EvmFactory, ToTxEnv, block::BlockExecutorFactory, precompiles::PrecompilesMap,
    };
    use alloy_hardforks::ForkCondition;
    use alloy_primitives::{Address, Bytes, Sealable, Signature, U256, address, bytes, keccak256, uint};
    use base_common_chains::{BaseUpgradeExt, ChainUpgrades};
    use base_common_consensus::{
        BaseTxEnvelope, Call, Eip8130Constants, Eip8130Signed, Predeploys, TxDeposit, TxEip8130,
    };
    use base_common_genesis::BaseUpgrade;
    use k256::ecdsa::SigningKey;
    use revm::{
        Context,
        bytecode::Bytecode,
        context::BlockEnv,
        database::{CacheDB, EmptyDB, InMemoryDB},
        inspector::NoOpInspector,
        primitives::HashMap,
        state::AccountInfo,
    };

    use super::*;
    use crate::{
        AlloyReceiptBuilder, BaseBlockExecutorFactory, BaseEvm, BaseEvmFactory, BaseSpecId,
        Builder, DefaultBase, L1BlockInfo,
    };

    #[test]
    fn test_with_encoded() {
        let executor_factory = BaseBlockExecutorFactory::new(
            AlloyReceiptBuilder::default(),
            ChainUpgrades::mainnet(),
            BaseEvmFactory::default(),
        );
        let mut db =
            revm::database::State::builder().with_database(CacheDB::<EmptyDB>::default()).build();
        let evm = executor_factory.evm_factory().create_evm(&mut db, EvmEnv::default());
        let mut executor = executor_factory.create_executor(evm, BaseBlockExecutionCtx::default());
        let tx = Recovered::new_unchecked(
            BaseTxEnvelope::Legacy(TxLegacy::default().into_signed(Signature::new(
                Default::default(),
                Default::default(),
                Default::default(),
            ))),
            Address::ZERO,
        );
        let tx_with_encoded = WithEncoded::new(tx.encoded_2718().into(), tx.clone());

        // make sure we can use both `WithEncoded` and transaction itself as inputs.
        let _ = executor.execute_transaction(&tx);
        let _ = executor.execute_transaction(&tx_with_encoded);
    }

    fn prepare_jovian_db(da_footprint_gas_scalar: u16) -> revm::database::State<InMemoryDB> {
        const L1_BASE_FEE: U256 = uint!(1_U256);
        const L1_BLOB_BASE_FEE: U256 = uint!(2_U256);
        const L1_BASE_FEE_SCALAR: u64 = 3;
        const L1_BLOB_BASE_FEE_SCALAR: u64 = 4;
        const L1_FEE_SCALARS: U256 = U256::from_limbs([
            0,
            (L1_BASE_FEE_SCALAR << (64 - L1BlockInfo::BASE_FEE_SCALAR_OFFSET * 2))
                | L1_BLOB_BASE_FEE_SCALAR,
            0,
            0,
        ]);
        const OPERATOR_FEE_SCALAR: u8 = 5;
        const OPERATOR_FEE_CONST: u8 = 6;
        let da_footprint_gas_scalar_bytes = da_footprint_gas_scalar.to_be_bytes();
        let mut operator_fee_and_da_footprint = [0u8; 32];
        operator_fee_and_da_footprint[31] = OPERATOR_FEE_CONST;
        operator_fee_and_da_footprint[23] = OPERATOR_FEE_SCALAR;
        operator_fee_and_da_footprint[19] = da_footprint_gas_scalar_bytes[1];
        operator_fee_and_da_footprint[18] = da_footprint_gas_scalar_bytes[0];
        let operator_fee_and_da_footprint_u256 = U256::from_be_bytes(operator_fee_and_da_footprint);

        let mut db = revm::database::State::builder().with_database(InMemoryDB::default()).build();

        db.insert_account_with_storage(
            Predeploys::L1_BLOCK_INFO,
            Default::default(),
            HashMap::from_iter([
                (L1BlockInfo::L1_BASE_FEE_SLOT, L1_BASE_FEE),
                (L1BlockInfo::ECOTONE_L1_FEE_SCALARS_SLOT, L1_FEE_SCALARS),
                (L1BlockInfo::ECOTONE_L1_BLOB_BASE_FEE_SLOT, L1_BLOB_BASE_FEE),
                (L1BlockInfo::OPERATOR_FEE_SCALARS_SLOT, operator_fee_and_da_footprint_u256),
            ]),
        );

        db.insert_account(
            Address::ZERO,
            AccountInfo { balance: U256::from(400_000_000), ..Default::default() },
        );

        db
    }

    fn build_executor<'a>(
        db: &'a mut revm::database::State<InMemoryDB>,
        receipt_builder: &'a AlloyReceiptBuilder,
        base_chain_upgrades: &'a ChainUpgrades,
        gas_limit: u64,
        jovian_timestamp: u64,
    ) -> BaseBlockExecutor<
        BaseEvm<&'a mut revm::database::State<InMemoryDB>, NoOpInspector, PrecompilesMap>,
        &'a AlloyReceiptBuilder,
        &'a ChainUpgrades,
    > {
        let ctx = Context::base()
            .with_db(db)
            .with_chain(L1BlockInfo {
                operator_fee_scalar: Some(U256::from(2)),
                operator_fee_constant: Some(U256::from(50)),
                ..Default::default()
            })
            .with_block(BlockEnv {
                timestamp: U256::from(jovian_timestamp),
                gas_limit,
                ..Default::default()
            })
            .modify_cfg_chained(|cfg| cfg.spec = BaseSpecId::new(BaseUpgrade::Jovian));

        let evm = ctx.build_with_inspector(NoOpInspector {});

        BaseBlockExecutor::new(
            evm,
            BaseBlockExecutionCtx::default(),
            base_chain_upgrades,
            receipt_builder,
        )
    }

    #[test]
    fn test_jovian_da_footprint_estimation() {
        const DA_FOOTPRINT_GAS_SCALAR: u16 = 7;
        const GAS_LIMIT: u64 = 100_000;
        const JOVIAN_TIMESTAMP: u64 = 1746806402;

        let mut db = prepare_jovian_db(DA_FOOTPRINT_GAS_SCALAR);
        let base_chain_upgrades = ChainUpgrades::new(
            BaseUpgrade::mainnet()
                .into_iter()
                .chain(vec![(BaseUpgrade::Jovian, ForkCondition::Timestamp(JOVIAN_TIMESTAMP))]),
        );

        let receipt_builder = AlloyReceiptBuilder::default();
        let mut executor = build_executor(
            &mut db,
            &receipt_builder,
            &base_chain_upgrades,
            GAS_LIMIT,
            JOVIAN_TIMESTAMP,
        );

        let tx_inner = TxLegacy { gas_limit: GAS_LIMIT, ..Default::default() };

        let tx = Recovered::new_unchecked(
            BaseTxEnvelope::Legacy(tx_inner.into_signed(Signature::new(
                Default::default(),
                Default::default(),
                Default::default(),
            ))),
            Address::ZERO,
        );
        let mut tx_env = tx.to_tx_env();

        assert!(executor.da_footprint_used == 0);

        let expected_da_footprint =
            executor.jovian_da_footprint_estimation(&mut tx_env, &tx).unwrap();

        // The FastLZ size is cached on the transaction env for the L1 cost charged at execution.
        assert_eq!(
            tx_env.enveloped_tx_fastlz_size,
            Some(base_common_flz::flz_compress_len(&tx.encoded_2718()))
        );

        // make sure we can use both `WithEncoded` and transaction itself as inputs.
        let res = executor.execute_transaction(&tx);
        assert!(res.is_ok());

        assert!(executor.da_footprint_used == expected_da_footprint);
    }

    #[test]
    fn test_jovian_da_footprint_estimation_out_of_gas() {
        const DA_FOOTPRINT_GAS_SCALAR: u16 = 7;
        const JOVIAN_TIMESTAMP: u64 = 1746806402;
        const GAS_LIMIT: u64 = 100;

        let mut db = prepare_jovian_db(DA_FOOTPRINT_GAS_SCALAR);
        let base_chain_upgrades = ChainUpgrades::new(
            BaseUpgrade::mainnet()
                .into_iter()
                .chain(vec![(BaseUpgrade::Jovian, ForkCondition::Timestamp(JOVIAN_TIMESTAMP))]),
        );

        let receipt_builder = AlloyReceiptBuilder::default();
        let mut executor = build_executor(
            &mut db,
            &receipt_builder,
            &base_chain_upgrades,
            GAS_LIMIT,
            JOVIAN_TIMESTAMP,
        );

        let tx_inner = TxLegacy { gas_limit: GAS_LIMIT, ..Default::default() };

        let tx = Recovered::new_unchecked(
            BaseTxEnvelope::Legacy(tx_inner.into_signed(Signature::new(
                Default::default(),
                Default::default(),
                Default::default(),
            ))),
            Address::ZERO,
        );
        let mut tx_env = tx.to_tx_env();

        assert!(executor.da_footprint_used == 0);

        let expected_da_footprint =
            executor.jovian_da_footprint_estimation(&mut tx_env, &tx).unwrap();

        // make sure we can use both `WithEncoded` and transaction itself as inputs.
        let res = executor.execute_transaction(&tx);
        assert!(res.is_err());
        let err = res.unwrap_err();
        match err {
            BlockExecutionError::Validation(BlockValidationError::Other(err)) => {
                assert_eq!(
                    err.to_string(),
                    BaseBlockExecutionError::TransactionDaFootprintAboveGasLimit {
                        transaction_da_footprint: expected_da_footprint,
                        available_block_da_footprint: GAS_LIMIT,
                    }
                    .to_string(),
                );
            }
            _ => panic!("expected TransactionDaFootprintAboveGasLimit error"),
        }
    }

    #[test]
    fn test_jovian_da_footprint_estimation_maxed_out_da_footprint() {
        const DA_FOOTPRINT_GAS_SCALAR: u16 = 2000;
        const JOVIAN_TIMESTAMP: u64 = 1746806402;
        const GAS_LIMIT: u64 = 200_000;

        let mut db = prepare_jovian_db(DA_FOOTPRINT_GAS_SCALAR);
        let base_chain_upgrades = ChainUpgrades::new(
            BaseUpgrade::mainnet()
                .into_iter()
                .chain(vec![(BaseUpgrade::Jovian, ForkCondition::Timestamp(JOVIAN_TIMESTAMP))]),
        );

        let receipt_builder = AlloyReceiptBuilder::default();
        let mut executor = build_executor(
            &mut db,
            &receipt_builder,
            &base_chain_upgrades,
            GAS_LIMIT,
            JOVIAN_TIMESTAMP,
        );

        let tx_inner = TxLegacy { gas_limit: GAS_LIMIT, ..Default::default() };

        let tx = Recovered::new_unchecked(
            BaseTxEnvelope::Legacy(tx_inner.into_signed(Signature::new(
                Default::default(),
                Default::default(),
                Default::default(),
            ))),
            Address::ZERO,
        );
        let mut tx_env = tx.to_tx_env();

        assert!(executor.da_footprint_used == 0);

        let expected_da_footprint =
            executor.jovian_da_footprint_estimation(&mut tx_env, &tx).unwrap();

        // make sure we can use both `WithEncoded` and transaction itself as inputs.
        let gas_used_tx = executor.execute_transaction(&tx).expect("failed to execute transaction");

        // The gas used when executing the transaction should be the legacy value...
        assert!(gas_used_tx.tx_gas_used() < expected_da_footprint);

        // The gas used when finishing the executor should be the DA footprint since this is higher
        // than the legacy gas used and jovian is active...
        let (_, result) = executor.finish().expect("failed to finish executor");
        assert_eq!(result.blob_gas_used, expected_da_footprint);
        assert_eq!(result.gas_used, gas_used_tx.tx_gas_used());
        assert!(result.blob_gas_used > result.gas_used);
    }

    /// Failed deposits report uncapped regular gas, so admission reserves their full limit.
    #[test]
    fn amsterdam_block_gas_failed_deposit_above_cap() {
        let mut db = prepare_jovian_db(0);
        let target = Address::with_last_byte(0x45);
        let code = bytes!("fe");
        db.insert_account(
            target,
            AccountInfo {
                code_hash: keccak256(&code),
                code: Some(Bytecode::new_raw(code)),
                ..Default::default()
            },
        );
        let receipt_builder = AlloyReceiptBuilder::default();
        let upgrades = ChainUpgrades::mainnet();
        let evm = Context::base()
            .with_db(&mut db)
            .with_block(BlockEnv {
                timestamp: U256::from(1_900_000_000u64),
                gas_limit: 40_000_000,
                ..Default::default()
            })
            .modify_cfg_chained(|cfg| {
                cfg.set_spec_and_mainnet_gas_params(BaseSpecId::new(BaseUpgrade::Denim));
            })
            .build_with_inspector(NoOpInspector {});
        let mut executor = BaseBlockExecutor::new(
            evm,
            BaseBlockExecutionCtx::default(),
            &upgrades,
            &receipt_builder,
        );
        let deposit = |gas_limit| {
            Recovered::new_unchecked(
                BaseTxEnvelope::from(
                    TxDeposit {
                        from: Address::ZERO,
                        to: target.into(),
                        gas_limit,
                        ..Default::default()
                    }
                    .seal_slow(),
                ),
                Address::ZERO,
            )
        };
        let output = executor
            .execute_transaction_without_commit(&deposit(10_000_000))
            .expect("first failed deposit is included");
        assert!(!output.inner.result.result.is_success(), "invalid opcode fails deposit");
        executor.commit_transaction(output);
        assert!(
            matches!(
                executor.execute_transaction_without_commit(&deposit(35_000_000)),
                Err(BlockExecutionError::Validation(
                    BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                        transaction_gas_limit: 35_000_000,
                        block_available_gas: 30_000_000,
                    }
                ))
            ),
            "above-cap deposit cannot overfill regular capacity"
        );
        let output = executor
            .execute_transaction_without_commit(&deposit(30_000_000))
            .expect("uncapped deposit exactly fills remaining regular budget");
        executor.commit_transaction(output);
        let (_, result) = executor.finish().expect("finish failed-deposit block");
        assert_eq!(result.gas_used, 40_000_000, "full failed-deposit gas counts");
        assert_eq!(
            result.receipts.last().unwrap().cumulative_gas_used(),
            40_000_000,
            "failed-deposit receipts retain full charged gas"
        );
    }

    /// Ordinary storage refunds reduce receipt fees but not Amsterdam block capacity.
    #[test]
    fn amsterdam_block_gas_refunds() {
        for upgrade in [BaseUpgrade::Jovian, BaseUpgrade::Denim] {
            let mut db = prepare_jovian_db(0);
            let target = Address::with_last_byte(0x44);
            let code = bytes!("60005f5500");
            db.insert_account_with_storage(
                target,
                AccountInfo {
                    code_hash: keccak256(&code),
                    code: Some(Bytecode::new_raw(code)),
                    ..Default::default()
                },
                HashMap::from_iter([(U256::ZERO, U256::from(1))]),
            );
            let upgrades = ChainUpgrades::mainnet();
            let receipt_builder = AlloyReceiptBuilder::default();
            let evm = Context::base()
                .with_db(&mut db)
                .with_block(BlockEnv { gas_limit: 120_000_000, ..Default::default() })
                .modify_cfg_chained(|cfg| {
                    cfg.set_spec_and_mainnet_gas_params(BaseSpecId::new(upgrade));
                })
                .build_with_inspector(NoOpInspector {});
            let mut executor = BaseBlockExecutor::new(
                evm,
                BaseBlockExecutionCtx::default(),
                &upgrades,
                &receipt_builder,
            );
            let tx = Recovered::new_unchecked(
                BaseTxEnvelope::Legacy(
                    TxLegacy {
                        gas_limit: 100_000,
                        to: target.into(),
                        ..Default::default()
                    }
                    .into_signed(Signature::new(
                        Default::default(),
                        Default::default(),
                        Default::default(),
                    )),
                ),
                Address::ZERO,
            );
            let output = executor.execute_transaction_without_commit(&tx).expect("clear storage");
            assert!(output.inner.result.result.is_success(), "storage clear succeeds");
            let regular = output.inner.result.result.gas().block_regular_gas_used();
            let state = output.inner.result.result.gas().block_state_gas_used();
            let fee = executor.commit_transaction(output).tx_gas_used();
            // PUSH1 + PUSH0 cost 5. Jovian: 21_000 + 5_000 + 5, minus 4_800 refund.
            // Amsterdam: 15_000 + 12_100 + 5, minus min(11_616, 27_105 / 5) = 5_421.
            let (expected_regular, expected_fee, expected_header) = if upgrade == BaseUpgrade::Denim {
                (27_105, 21_684, 27_105)
            } else {
                (26_005, 21_205, 21_205)
            };
            assert_eq!(regular, expected_regular, "storage clear regular gas before refund");
            assert_eq!(state, 0, "clearing an existing slot creates no state");
            assert_eq!(fee, expected_fee, "storage clear charged gas after capped refund");
            let (_, result) = executor.finish().expect("finish clear block");
            assert_eq!(
                result.receipts[0].cumulative_gas_used(),
                expected_fee,
                "storage clear receipt retains the exact capped refund"
            );
            assert_eq!(result.gas_used, expected_header, "fork-appropriate storage clear header gas");
        }
    }

    /// Checks real deployment and transfer results through the public executor API.
    #[test]
    fn amsterdam_block_gas_deployment_and_mixed_block() {
        for upgrade in [BaseUpgrade::Jovian, BaseUpgrade::Denim, BaseUpgrade::Everest] {
            let amsterdam = upgrade != BaseUpgrade::Jovian;
            let mut db = prepare_jovian_db(0);
            db.insert_account(
                Address::ZERO,
                AccountInfo {
                    balance: U256::from(10u64).pow(U256::from(18u64)),
                    ..Default::default()
                },
            );
            let upgrades = ChainUpgrades::mainnet();
            let receipt_builder = AlloyReceiptBuilder::default();
            let evm = Context::base()
                .with_db(&mut db)
                .with_block(BlockEnv {
                    timestamp: U256::from(1_900_000_000u64),
                    gas_limit: 120_000_000,
                    ..Default::default()
                })
                .modify_cfg_chained(|cfg| {
                    cfg.set_spec_and_mainnet_gas_params(BaseSpecId::new(upgrade));
                })
                .build_with_inspector(NoOpInspector {});
            let mut executor = BaseBlockExecutor::new(
                evm,
                BaseBlockExecutionCtx::default(),
                &upgrades,
                &receipt_builder,
            );
            // Return 64 KiB of zero bytecode; Jovian retains its 24 KiB code-size limit.
            let mut initcode = bytes!("62010000600e5f39620100005ff3").to_vec();
            initcode.resize(14 + 65_536, 0);
            let deployment = TxLegacy {
                gas_limit: 115_000_000,
                input: initcode.into(),
                ..Default::default()
            };
            let deployment = Recovered::new_unchecked(
                BaseTxEnvelope::Legacy(deployment.into_signed(Signature::new(
                    Default::default(),
                    Default::default(),
                    Default::default(),
                ))),
                Address::ZERO,
            );
            // Pre-Amsterdam rejects the oversized deployment; test historical accounting
            // with a transfer instead, without changing its code-size or gas-cap rules.
            let first = if amsterdam {
                deployment
            } else {
                Recovered::new_unchecked(
                    BaseTxEnvelope::Legacy(
                        TxLegacy {
                            gas_limit: 100_000,
                            to: Address::ZERO.into(),
                            ..Default::default()
                        }
                        .into_signed(Signature::new(
                            Default::default(),
                            Default::default(),
                            Default::default(),
                        )),
                    ),
                    Address::ZERO,
                )
            };
            let output = executor.execute_transaction_without_commit(&first).expect("first tx");
            assert!(output.inner.result.result.is_success(), "first transaction succeeds");
            let regular = output.inner.result.result.gas().block_regular_gas_used();
            let state = output.inner.result.result.gas().block_state_gas_used();
            let fee = executor.commit_transaction(output).tx_gas_used();
            if amsterdam {
                // EIP-2780/7976 floor: (12_000 + 12_000) + 65_550 * 64 = 4_219_200.
                // Spent regular: 24_000 + (65_540 * 4 + 10 * 16) + 2_049 * 2
                // + 16 opcode gas + 2_048 * 3 copy + (2_048 * 3 + 2_048^2 / 512)
                // + 2_048 * 6 deployed-code hashing = 323_202.
                // State: (120 + 65_536) * 1_530 = 100_453_680; no refund.
                assert_eq!(regular, 4_219_200, "64 KiB regular gas is the calldata floor");
                assert_eq!(state, 100_453_680, "64 KiB state gas at L1 CPSB 1530");
                assert_eq!(fee, 100_776_882, "64 KiB receipt gas is spent regular plus state");
            } else {
                assert_eq!(regular, 21_000, "historical first self-transfer regular gas");
                assert_eq!(state, 0, "historical first self-transfer creates no state");
                assert_eq!(fee, 21_000, "historical first self-transfer charged gas");
            }
            let transfer = Recovered::new_unchecked(
                BaseTxEnvelope::Legacy(
                    TxLegacy {
                        nonce: 1,
                        gas_limit: 100_000,
                        to: Address::ZERO.into(),
                        ..Default::default()
                    }
                    .into_signed(Signature::new(
                        Default::default(),
                        Default::default(),
                        Default::default(),
                    )),
                ),
                Address::ZERO,
            );
            let output = executor.execute_transaction_without_commit(&transfer).expect("mixed tx");
            let transfer_regular = output.inner.result.result.gas().block_regular_gas_used();
            let transfer_state = output.inner.result.result.gas().block_state_gas_used();
            let transfer_fee = executor.commit_transaction(output).tx_gas_used();
            let (_, result) = executor.finish().expect("finish block");
            let expected_transfer = if amsterdam { 12_000 } else { 21_000 };
            assert_eq!(transfer_regular, expected_transfer, "second self-transfer regular gas");
            assert_eq!(transfer_state, 0, "second self-transfer creates no state");
            assert_eq!(transfer_fee, expected_transfer, "second self-transfer charged gas");
            assert_eq!(
                result.receipts[0].cumulative_gas_used(),
                if amsterdam { 100_776_882 } else { 21_000 },
                "first receipt contains exact charged gas"
            );
            assert_eq!(
                result.receipts.last().unwrap().cumulative_gas_used(),
                if amsterdam { 100_788_882 } else { 42_000 },
                "receipts retain scalar charged gas"
            );
            if amsterdam {
                assert_eq!(
                    result.gas_used,
                    100_453_680,
                    "header is max(4_219_200 + 12_000, 100_453_680 + 0)"
                );
            } else {
                assert_eq!(result.gas_used, 42_000, "historical header sums both 21_000 receipts");
            }
        }
    }

    /// Builds a signed EIP-8130 transaction with the given `gas_limit`. When
    /// `with_payer` is set, a distinct payer is named with a resolvable k1 payer
    /// auth blob (`K1_AUTHENTICATOR || sig`), so `payer_auth` is non-zero.
    fn signed_eip8130(gas_limit: u64, with_payer: bool) -> Eip8130Signed {
        let payer_auth = if with_payer {
            let mut blob = vec![];
            blob.extend_from_slice(Eip8130Constants::K1_AUTHENTICATOR.as_slice());
            blob.extend_from_slice(&[0u8; 65]);
            Bytes::from(blob)
        } else {
            Bytes::new()
        };
        let tx = TxEip8130 {
            gas_limit,
            payer: with_payer.then(|| Address::with_last_byte(0x11)),
            ..Default::default()
        };
        Eip8130Signed::new(tx, Bytes::new(), payer_auth)
    }

    /// The block gas reservation for an EIP-8130 transaction must include
    /// `payer_auth` on top of the declared `gas_limit`: a transaction that fits
    /// on `gas_limit` alone is still rejected when `gas_limit + payer_auth`
    /// exceeds the available block gas.
    #[test]
    fn eip8130_block_gas_reservation_includes_payer_auth() {
        const GAS_LIMIT: u64 = 100_000;
        const JOVIAN_TIMESTAMP: u64 = 1746806402;

        let signed = signed_eip8130(GAS_LIMIT, true);
        let payer_auth = IntrinsicGas::max_payer_auth_cost(&signed).expect("payer auth cost");
        assert!(payer_auth > 0, "payer auth must be metered on top of gas_limit");

        // Block admits `gas_limit` alone but not `gas_limit + payer_auth`.
        let block_gas_limit = GAS_LIMIT + payer_auth - 1;

        let mut db = prepare_jovian_db(0);
        let base_chain_upgrades = ChainUpgrades::new(
            BaseUpgrade::mainnet()
                .into_iter()
                .chain(vec![(BaseUpgrade::Jovian, ForkCondition::Timestamp(JOVIAN_TIMESTAMP))]),
        );
        let receipt_builder = AlloyReceiptBuilder::default();
        let mut executor = build_executor(
            &mut db,
            &receipt_builder,
            &base_chain_upgrades,
            block_gas_limit,
            JOVIAN_TIMESTAMP,
        );

        let tx = Recovered::new_unchecked(
            BaseTxEnvelope::Eip8130(signed),
            Address::with_last_byte(0x22),
        );
        let err = executor.execute_transaction(&tx).expect_err("reservation must reject");
        match err {
            BlockExecutionError::Validation(
                BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                    transaction_gas_limit,
                    block_available_gas,
                },
            ) => {
                assert_eq!(transaction_gas_limit, GAS_LIMIT + payer_auth);
                assert_eq!(block_available_gas, block_gas_limit);
            }
            other => panic!("expected TransactionGasLimitMoreThanAvailableBlockGas, got {other:?}"),
        }
    }

    /// A sponsored transaction whose payer authentication cannot be priced is
    /// invalid: the block is rejected through a validation error, never an
    /// internal (node-fatal) one.
    #[test]
    fn unpriceable_payer_auth_is_a_validation_error() {
        const GAS_LIMIT: u64 = 100_000;
        const JOVIAN_TIMESTAMP: u64 = 1746806402;

        let tx = TxEip8130 {
            gas_limit: GAS_LIMIT,
            payer: Some(Address::with_last_byte(0x11)),
            ..Default::default()
        };
        let mut unscheduled_payer_auth = Address::repeat_byte(0xee).to_vec();
        unscheduled_payer_auth.extend_from_slice(&[0u8; 65]);
        let signed = Eip8130Signed::new(tx, Bytes::new(), Bytes::from(unscheduled_payer_auth));
        assert!(IntrinsicGas::max_payer_auth_cost(&signed).is_err());

        let mut db = prepare_jovian_db(0);
        let base_chain_upgrades = ChainUpgrades::new(
            BaseUpgrade::mainnet()
                .into_iter()
                .chain(vec![(BaseUpgrade::Jovian, ForkCondition::Timestamp(JOVIAN_TIMESTAMP))]),
        );
        let receipt_builder = AlloyReceiptBuilder::default();
        let mut executor = build_executor(
            &mut db,
            &receipt_builder,
            &base_chain_upgrades,
            GAS_LIMIT,
            JOVIAN_TIMESTAMP,
        );

        let tx = Recovered::new_unchecked(
            BaseTxEnvelope::Eip8130(signed),
            Address::with_last_byte(0x22),
        );
        let err = executor.execute_transaction(&tx).expect_err("unpriceable payer auth");
        assert!(
            matches!(err, BlockExecutionError::Validation(BlockValidationError::Other(_))),
            "expected a validation error, got {err:?}",
        );
    }

    /// A self-paying EIP-8130 transaction meters no `payer_auth`, so its
    /// reservation is exactly `gas_limit`: it must not be rejected by the block
    /// gas pre-check when the block admits its declared `gas_limit`.
    #[test]
    fn self_pay_eip8130_block_gas_reservation_is_gas_limit() {
        const GAS_LIMIT: u64 = 100_000;
        const JOVIAN_TIMESTAMP: u64 = 1746806402;

        let signed = signed_eip8130(GAS_LIMIT, false);
        assert_eq!(
            IntrinsicGas::max_payer_auth_cost(&signed).expect("payer auth cost"),
            0,
            "self-pay meters no payer authentication",
        );

        let mut db = prepare_jovian_db(0);
        let base_chain_upgrades = ChainUpgrades::new(
            BaseUpgrade::mainnet()
                .into_iter()
                .chain(vec![(BaseUpgrade::Jovian, ForkCondition::Timestamp(JOVIAN_TIMESTAMP))]),
        );
        let receipt_builder = AlloyReceiptBuilder::default();
        let mut executor = build_executor(
            &mut db,
            &receipt_builder,
            &base_chain_upgrades,
            GAS_LIMIT,
            JOVIAN_TIMESTAMP,
        );

        let tx = Recovered::new_unchecked(
            BaseTxEnvelope::Eip8130(signed),
            Address::with_last_byte(0x22),
        );
        // Execution itself may fail for this synthetic transaction, but the block
        // gas pre-check must not: reserving exactly `gas_limit` fits the block.
        if let Err(BlockExecutionError::Validation(
            BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas { .. },
        )) = executor.execute_transaction(&tx)
        {
            panic!("self-pay transaction must not be rejected by the block gas pre-check");
        }
    }

    /// The committed phase count of an EIP-8130 transaction travels with its execution result, so
    /// a receipt built on a different thread from the one that executed the transaction (as in a
    /// split execute-on-worker, commit-on-coordinator pipeline) still sees it.
    #[test]
    fn eip8130_committed_phases_survive_execution_on_another_thread() {
        const CHAIN_ID: u64 = 8453;
        const TIMESTAMP: u64 = 1_000;
        const BLOCK_GAS_LIMIT: u64 = 30_000_000;
        const BASE_FEE: u64 = 1_000_000_000;
        let ok_target = address!("0x00000000000000000000000000000000000000c1");
        let reverter = address!("0x00000000000000000000000000000000000000c2");

        let key = SigningKey::from_slice(&[0x33; 32]).expect("valid key");
        let point = key.verifying_key().to_encoded_point(false);
        let sender = Address::from_slice(&keccak256(&point.as_bytes()[1..])[12..]);

        let tx = TxEip8130 {
            chain_id: CHAIN_ID,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 5_000_000_000,
            gas_limit: 1_000_000,
            calls: vec![
                vec![Call { to: ok_target, value: U256::ZERO, data: Bytes::new() }],
                vec![Call { to: reverter, value: U256::ZERO, data: Bytes::new() }],
            ],
            ..Default::default()
        };
        let (signature, recid) =
            key.sign_prehash_recoverable(tx.sender_signature_hash().as_slice()).expect("sign");
        let mut sender_auth = signature.to_bytes().to_vec();
        sender_auth.push(recid.to_byte() + 27);
        let signed = Eip8130Signed::new(tx, Bytes::from(sender_auth), Bytes::new());

        // The executor is not `Send`, so the worker owns its own state and executor, as a
        // parallel-execution worker would, and hands only the result back.
        let tx = Recovered::new_unchecked(BaseTxEnvelope::Eip8130(signed), sender);
        let result = std::thread::spawn(move || {
            let mut db =
                revm::database::State::builder().with_database(InMemoryDB::default()).build();
            db.insert_account(
                sender,
                AccountInfo {
                    balance: U256::from(10u64).pow(U256::from(18u64)),
                    ..Default::default()
                },
            );
            // PUSH1 0, PUSH1 0, REVERT
            let revert_code = bytes!("60006000fd");
            db.insert_account(
                reverter,
                AccountInfo {
                    code_hash: keccak256(&revert_code),
                    code: Some(Bytecode::new_raw(revert_code)),
                    ..Default::default()
                },
            );

            let evm = Context::base()
                .with_db(&mut db)
                .with_block(BlockEnv {
                    number: U256::from(1u64),
                    timestamp: U256::from(TIMESTAMP),
                    gas_limit: BLOCK_GAS_LIMIT,
                    basefee: BASE_FEE,
                    ..Default::default()
                })
                .modify_cfg_chained(|cfg| {
                    cfg.set_spec_and_mainnet_gas_params(BaseSpecId::new(BaseUpgrade::Everest));
                    cfg.chain_id = CHAIN_ID;
                })
                .build_with_inspector(NoOpInspector {});
            let receipt_builder = AlloyReceiptBuilder::default();
            let chain_upgrades = ChainUpgrades::mainnet();
            let mut executor = BaseBlockExecutor::new(
                evm,
                BaseBlockExecutionCtx::default(),
                &chain_upgrades,
                &receipt_builder,
            );
            executor.execute_transaction_without_commit(&tx)
        })
        .join()
        .expect("execution thread panicked")
        .expect("EIP-8130 transaction is included");

        assert_eq!(result.eip8130_committed_phases, 1);
        let regular = result.inner.result.result.gas().block_regular_gas_used();
        let state = result.inner.result.result.gas().block_state_gas_used();
        let charged = result.inner.result.result.tx_gas_used();
        let mut db = prepare_jovian_db(0);
        let receipt_builder = AlloyReceiptBuilder::default();
        let upgrades = ChainUpgrades::mainnet();
        let evm = Context::base()
            .with_db(&mut db)
            .modify_cfg_chained(|cfg| {
                cfg.set_spec_and_mainnet_gas_params(BaseSpecId::new(BaseUpgrade::Everest));
            })
            .build_with_inspector(NoOpInspector {});
        let mut executor = BaseBlockExecutor::new(
            evm,
            BaseBlockExecutionCtx::default(),
            &upgrades,
            &receipt_builder,
        );
        executor.commit_transaction(result);
        let (_, block) = executor.finish().expect("finish native transaction block");
        assert_eq!(block.gas_used, regular.max(state), "native tx reports count in block gas");
        assert_eq!(block.receipts[0].cumulative_gas_used(), charged, "native receipt uses charged gas");
    }
}
