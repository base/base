//! Enshrined Base transaction (account-abstraction, type `0x79`) transaction execution.
//!
//! [`TxBaseHandler`] runs a Base transaction through the engine-neutral pipeline shared with
//! the revm reference ([`TxBasePipeline`] and [`TxBaseCalls`], documented in
//! `base-execution-base-tx`), supplying the EVM2 state, call frames, and OP-stack fee routing.
//!
//! Preparation (also the pool-validation path) checks the transaction against the block, runs the
//! storage-backed pre-call pipeline over a gas-free [`Evm2JournalStorageProvider`], and pre-charges
//! the payer the worst-case fee: the full gas budget (plus payer authentication) at the effective
//! gas price, the L1 data fee, and the operator fee on that budget. Execution warms the
//! precompiles, the coinbase (EIP-3651), the sender, and the payer, dispatches the phased `calls`
//! as top-level frames from `sender` (`tx.origin == sender`, `GASPRICE` = the effective price),
//! then settles: the payer is refunded the surplus over the billable gas, and the base fee, the
//! priority fee, the L1 data fee, and the operator fee are credited to their recipients.
//!
//! A read-only simulation ([`TxBaseSimulation`]) resolves the sender from the recovered signer
//! without verifying any signature or touching the nonce, never charges or settles fees, and
//! rolls every state change back while keeping the logs of the reported run.

use alloy_primitives::{Address, U256};
use base_common_consensus::{Call, Predeploys};
use base_common_genesis::BaseFeature;
use base_execution_base_tx::{
    TxBaseBlockContext, TxBaseCallFrame, TxBaseCallHost, TxBaseCalls, TxBaseCallsResult,
    TxBaseError, TxBaseFeeSplit, TxBaseOutcome, TxBasePipeline,
};
use base_precompile_storage::{Evm2JournalStorageProvider, StorageCtx};
use evm2::{
    AccountInfo, AnyError, Evm, EvmFeatures, HostError, TxResult,
    env::{TxEnv, TxEnvExt},
    evm::StateCheckpoint,
    interpreter::{Host, InstrStop, MessageExt, MessageKind},
    registry::{HandlerError, HandlerResult, TxRequest},
    version::GasId,
};

use crate::{
    BaseEvmTypes, BaseTime, BaseTxResultExt, L1Block, TxBaseEnv, TxBaseSimulation,
    TxBaseSimulationMode,
};

/// The pre-call state a Base transaction carries from preparation into execution.
#[derive(Debug, Clone)]
pub struct TxBasePrepared {
    /// The resolved actors and gas/fee parameters.
    pub outcome: TxBaseOutcome,
    /// How the transaction settles.
    pub settlement: TxBaseSettlement,
}

/// How a prepared Base transaction settles.
#[derive(Debug, Clone)]
pub enum TxBaseSettlement {
    /// An included transaction: the payer was pre-charged `prepay`, of which `l1_fee` is the L1
    /// data fee.
    Verified {
        /// The worst-case amount debited from the payer.
        prepay: U256,
        /// The L1 data fee included in `prepay`.
        l1_fee: U256,
    },
    /// A read-only simulation, rolled back to `start` once measured.
    Simulated {
        /// The simulation request.
        simulation: TxBaseSimulation,
        /// The state checkpoint taken before any simulated write.
        start: StateCheckpoint,
    },
}

/// Executes enshrined Base transactions on EVM2.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TxBaseHandler;

impl TxBaseHandler {
    /// Checks the transaction against the block, runs the pre-call pipeline, and pre-charges the
    /// payer (or, for a simulation, resolves the pre-call state without verification). Every
    /// rejection leaves the transaction uncommitted.
    pub fn prepare(
        req: &mut TxRequest<'_, '_, BaseEvmTypes, TxBaseEnv>,
    ) -> HandlerResult<TxBasePrepared> {
        let signer = req.tx.signer();
        let tx: &TxBaseEnv = *req.tx;
        let host = &mut *req.host;
        let upgrade = host.config_spec_id().upgrade();
        // Consensus-critical: the Base transaction type exists only from Everest. Every
        // ingress path gates it, but a block delivered by the Engine API or P2P reaches execution
        // directly.
        if !upgrade.is_feature_enabled(BaseFeature::TxBase) {
            return Err(Self::reject(TxBasePipeline::not_active_error()));
        }
        host.ext_mut().tx_origin = signer;
        let now = TxBasePipeline::block_timestamp(host.block().timestamp).map_err(Self::reject)?;
        let mut block = TxBaseBlockContext {
            chain_id: host.version().chain_id,
            now_ms: 0,
            base_fee: host.block().basefee.saturating_to(),
            tx_gas_limit_cap: host.version().tx_gas_limit_cap,
            // Simulation never checks the payer's balance; the verified path sets this below.
            fee_charge: false,
        };

        if let Some(simulation) = tx.simulation {
            let start = host.state().checkpoint();
            let outcome = Self::run_pre_call(host, |sctx| {
                TxBasePipeline::simulate_resolve(sctx, &tx.signed, signer, &block)
            })?;
            // Bump the protocol nonce as inclusion does; the simulation rolls back to `start`.
            Self::consume_sender_nonce(host, &outcome)?;
            return Ok(TxBasePrepared {
                outcome,
                settlement: TxBaseSettlement::Simulated { simulation, start },
            });
        }

        TxBasePipeline::check_chain_id(tx.signed.tx(), block.chain_id).map_err(Self::reject)?;
        // `eth_call`, `eth_estimateGas` and unvalidated `eth_simulateV1` disable fee charging:
        // nothing is pre-charged or settled and the payer's balance is not checked.
        block.fee_charge = host.feature(EvmFeatures::FEE_CHARGE);
        // Everest implies Denim, so the block's millisecond part is already in `BaseTime`.
        block.now_ms = BaseTime::timestamp_ms(host, now)?;
        L1Block::ensure_fee_params(host)?;
        TxBasePipeline::check_validity_window(tx.signed.tx(), block.now_ms)
            .map_err(Self::reject)?;
        let outcome = Self::run_pre_call(host, |sctx| {
            TxBasePipeline::authorize_and_apply(sctx, &tx.signed, &block)
        })?;
        Self::consume_sender_nonce(host, &outcome)?;
        let (prepay, l1_fee) = if block.fee_charge {
            Self::prepay(host, &outcome, tx)?
        } else {
            (U256::ZERO, U256::ZERO)
        };
        Ok(TxBasePrepared { outcome, settlement: TxBaseSettlement::Verified { prepay, l1_fee } })
    }

    /// Dispatches the prepared transaction's `calls` and settles (or, for a simulation, measures
    /// and rolls back) the transaction.
    pub fn execute(
        req: TxRequest<'_, '_, BaseEvmTypes, TxBaseEnv>,
        prepared: TxBasePrepared,
    ) -> HandlerResult<TxResult<BaseEvmTypes>> {
        let tx: &TxBaseEnv = *req.tx;
        let host = req.host;
        let outcome = prepared.outcome;
        Self::warm_pre_call_accounts(host, &outcome)?;
        let tx_env = TxEnvExt {
            origin: outcome.sender,
            gas_price: U256::from(outcome.effective),
            chain_id: U256::from(host.version().chain_id),
            ..TxEnvExt::default()
        };
        let mut calls_host = Evm2CallHost { host: &mut *host, tx_env: &tx_env };
        match prepared.settlement {
            TxBaseSettlement::Verified { prepay, l1_fee } => {
                let calls = TxBaseCalls::execute(
                    &mut calls_host,
                    &tx.signed,
                    &outcome,
                    outcome.execution_gas_available,
                )?;
                // With fee charging disabled nothing was pre-charged, so there is nothing to
                // settle: report the metered gas, as `eth_call` does for every other transaction
                // type, instead of the reservation that would have been billed.
                let metered = calls.billable_gas(&outcome, false);
                let gas_used = if host.feature(EvmFeatures::FEE_CHARGE) {
                    Self::settle_fees(host, &outcome, &calls, prepay, l1_fee, tx)?
                } else {
                    metered
                };
                let mut result = Self::result(calls, gas_used, l1_fee);
                // For the builder's transaction events: what this transaction would have been
                // billed without gas-limit billing, and the payer authentication the reserved
                // budget adds on top of `gas_limit`.
                result.ext.metered_gas_used = Some(metered);
                result.ext.payer_auth_gas_reserved = Some(outcome.payer_auth_cost);
                Ok(result)
            }
            TxBaseSettlement::Simulated { simulation, start } => {
                let (calls, gas) = calls_host.simulate(tx, &outcome, simulation.mode)?;
                // Roll every simulated write back, keeping the logs of the reported run.
                let features = host.version().features;
                let logs_len = host.logs().len();
                host.state_mut()
                    .rollback(StateCheckpoint::new(start.journal_len(), logs_len), features);
                Ok(Self::result(calls, gas, U256::ZERO))
            }
        }
    }

    /// Runs a storage-backed pre-call pipeline over a gas-free view of the EVM state. A state
    /// failure observed anywhere in the pipeline is fatal, even when the pipeline stringified it
    /// into a transaction error: the state it read cannot be trusted, and reporting it as an
    /// invalid transaction would wrongly reject the block instead of failing the node.
    fn run_pre_call(
        host: &mut Evm<'_, BaseEvmTypes>,
        pipeline: impl FnOnce(StorageCtx<'_>) -> Result<TxBaseOutcome, TxBaseError>,
    ) -> HandlerResult<TxBaseOutcome> {
        let mut provider = Evm2JournalStorageProvider::new(host, Address::ZERO);
        let outcome = StorageCtx::enter(&mut provider, pipeline);
        if let Some(error) = provider.take_database_error() {
            return Err(HandlerError::Fatal(AnyError::from(error)));
        }
        outcome.map_err(Self::reject)
    }

    /// Publishes the resolved sender as `tx.origin` and bumps its protocol nonce (for
    /// `nonce_key == 0`). Runs for every executed transaction, whether or not fees are charged,
    /// and for simulations, so their `calls` observe the nonce bump and the real origin.
    fn consume_sender_nonce(
        host: &mut Evm<'_, BaseEvmTypes>,
        outcome: &TxBaseOutcome,
    ) -> HandlerResult<()> {
        host.ext_mut().tx_origin = outcome.sender;
        if outcome.bump_protocol_nonce {
            host.state_mut().account(&outcome.sender)?.bump_nonce();
        }
        Ok(())
    }

    /// Pre-charges the payer the worst-case fee, so the `calls` cannot spend the gas
    /// reservation. Returns the amount debited and its L1 data fee component. Skipped when fee
    /// charging is disabled.
    fn prepay(
        host: &mut Evm<'_, BaseEvmTypes>,
        outcome: &TxBaseOutcome,
        tx: &TxBaseEnv,
    ) -> HandlerResult<(U256, U256)> {
        let upgrade = host.config_spec_id().upgrade();
        let max_gas = outcome.max_chargeable_gas();
        let gas_charge = U256::from(max_gas)
            .checked_mul(U256::from(outcome.effective))
            .ok_or_else(|| Self::invalid("Base transaction gas pre-charge overflow"))?;
        let l1_fee = L1Block::tx_l1_fee(host, &tx.enveloped);
        let operator_fee =
            host.block().ext.operator_fee_charge(&tx.enveloped, U256::from(max_gas), upgrade);
        let prepay = gas_charge
            .checked_add(l1_fee)
            .and_then(|amount| amount.checked_add(operator_fee))
            .ok_or_else(|| Self::invalid("Base transaction pre-charge overflow"))?;

        let mut payer = host.state_mut().account(&outcome.payer)?;
        let balance = payer.balance();
        let debited = balance
            .checked_sub(prepay)
            .ok_or(HandlerError::InsufficientFunds { fee: prepay, balance })?;
        payer.set_balance(debited);
        Ok((prepay, l1_fee))
    }

    /// Warms what a standard transaction starts with warm — the precompiles and, from EIP-3651,
    /// the coinbase — plus the sender and the payer.
    fn warm_pre_call_accounts(
        host: &mut Evm<'_, BaseEvmTypes>,
        outcome: &TxBaseOutcome,
    ) -> HandlerResult<()> {
        host.warm_precompiles();
        if host.feature(EvmFeatures::EIP3651) {
            let coinbase = host.block().beneficiary;
            host.state_mut().prewarm(&coinbase);
        }
        for account in [outcome.sender, outcome.payer] {
            host.state_mut().account(&account)?.warm();
        }
        Ok(())
    }

    /// Settles the fee against the pre-charge: refunds the payer the surplus over the billable
    /// gas and credits the base fee, the priority fee, the L1 data fee, and the operator fee to
    /// their recipients. Returns the billable gas.
    fn settle_fees(
        host: &mut Evm<'_, BaseEvmTypes>,
        outcome: &TxBaseOutcome,
        calls: &TxBaseCallsResult,
        prepay: U256,
        l1_fee: U256,
        tx: &TxBaseEnv,
    ) -> HandlerResult<u64> {
        let upgrade = host.config_spec_id().upgrade();
        let billable_gas =
            calls.billable_gas(outcome, upgrade.is_feature_enabled(BaseFeature::GasLimitBilling));
        let split = TxBaseFeeSplit::new(outcome, billable_gas).map_err(Self::reject)?;
        let operator_fee =
            host.block().ext.operator_fee_charge(&tx.enveloped, U256::from(billable_gas), upgrade);
        // The pre-charge bounds every settled component, so the refund never underflows.
        let total_cost = split
            .fee
            .checked_add(l1_fee)
            .and_then(|amount| amount.checked_add(operator_fee))
            .ok_or_else(|| Self::invalid("Base transaction settled cost overflow"))?;
        let refund = prepay
            .checked_sub(total_cost)
            .ok_or_else(|| Self::invalid("settled fee exceeds the worst-case pre-charge"))?;
        {
            let mut payer = host.state_mut().account(&outcome.payer)?;
            let balance = payer
                .balance()
                .checked_add(refund)
                .ok_or_else(|| Self::invalid("Base transaction payer refund balance overflow"))?;
            payer.set_balance(balance);
        }

        let beneficiary = host.block().beneficiary;
        for (recipient, amount) in [
            (Predeploys::BASE_FEE_VAULT, split.base_fee_amount),
            (beneficiary, split.priority_amount),
            (Predeploys::L1_FEE_VAULT, l1_fee),
            (Predeploys::OPERATOR_FEE_VAULT, operator_fee),
        ] {
            host.state_mut().account(&recipient)?.add_balance(amount);
        }
        Ok(billable_gas)
    }

    /// The transaction result: success when every phase committed, with the reported gas already
    /// net of refunds.
    fn result(calls: TxBaseCallsResult, gas: u64, l1_fee: U256) -> TxResult<BaseEvmTypes> {
        TxResult::<BaseEvmTypes> {
            status: !calls.reverted,
            total_gas_spent: gas,
            stop: if calls.reverted { InstrStop::Revert } else { InstrStop::Return },
            output: calls.output,
            ext: BaseTxResultExt {
                l1_fee,
                base_tx_committed_phases: Some(calls.committed_phases),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Maps a Base transaction rejection onto the handler error. A payer that cannot cover the fee is
    /// reported as the standard insufficient-funds error, which block builders skip rather than
    /// abort on.
    fn reject(error: TxBaseError) -> HandlerError {
        match error {
            TxBaseError::PayerCannotPay { balance, required } => {
                HandlerError::InsufficientFunds { fee: required, balance }
            }
            error @ TxBaseError::Invalid(_) => HandlerError::external(error),
        }
    }

    fn invalid(reason: &str) -> HandlerError {
        Self::reject(TxBaseError::invalid(reason))
    }
}

/// The EVM2 state and frames as the [`TxBaseCallHost`] of [`TxBaseCalls::execute`].
struct Evm2CallHost<'h, 'a> {
    host: &'h mut Evm<'a, BaseEvmTypes>,
    tx_env: &'h TxEnv<BaseEvmTypes>,
}

impl Evm2CallHost<'_, '_> {
    /// Loads (and warms) `address` with its code.
    fn load_account(&mut self, address: Address) -> HandlerResult<evm2::evm::AccountLoad> {
        Host::load_account(self.host, &address, true, false).map_err(|error| match error {
            HostError::Execution(error) => error.into(),
            HostError::Halt(stop) => HandlerError::Fatal(AnyError::from(format!(
                "unexpected halt loading a Base transaction call target: {stop:?}"
            ))),
        })
    }

    /// Runs the calls at `pool` and reverts every write (and log) they made.
    fn probe(
        &mut self,
        tx: &TxBaseEnv,
        outcome: &TxBaseOutcome,
        pool: u64,
    ) -> HandlerResult<TxBaseCallsResult> {
        let checkpoint = self.checkpoint();
        let calls = TxBaseCalls::execute(self, &tx.signed, outcome, pool)?;
        self.revert(checkpoint);
        Ok(calls)
    }

    /// Measures a simulated transaction's calls, returning the reported run and its gas: the
    /// metered gas of a single run for `eth_call` (or a run that fails even at the full pool), or
    /// the smallest verified-feasible gas limit for `eth_estimateGas`.
    fn simulate(
        &mut self,
        tx: &TxBaseEnv,
        outcome: &TxBaseOutcome,
        mode: TxBaseSimulationMode,
    ) -> HandlerResult<(TxBaseCallsResult, u64)> {
        let ceiling_pool = outcome.execution_gas_available;
        if mode == TxBaseSimulationMode::Estimate {
            let ceiling = self.probe(tx, outcome, ceiling_pool)?;
            if !ceiling.reverted {
                let feasible_pool = TxBaseCalls::search_estimate_pool(
                    ceiling.call_gas_spent,
                    ceiling_pool,
                    |pool| Ok::<_, HandlerError>(self.probe(tx, outcome, pool)?.reverted),
                )?;
                let calls = TxBaseCalls::execute(self, &tx.signed, outcome, feasible_pool)?;
                // The on-chain call pool at this limit is `gas_limit - intrinsic` (payer
                // authentication is billed on top), so it is at least the verified-feasible pool;
                // the EIP-7623 floor keeps a data-heavy estimate admissible. Under gas-limit
                // billing settlement adds `payer_auth_cost` on top of the declared limit, so the
                // estimate is the sender budget alone.
                let sender_budget = outcome
                    .floored_sender_gas(outcome.sender_intrinsic.saturating_add(feasible_pool));
                let gas = if self
                    .host
                    .config_spec_id()
                    .upgrade()
                    .is_feature_enabled(BaseFeature::GasLimitBilling)
                {
                    sender_budget
                } else {
                    sender_budget.saturating_add(outcome.payer_auth_cost)
                };
                return Ok((calls, gas));
            }
        }
        let calls = TxBaseCalls::execute(self, &tx.signed, outcome, ceiling_pool)?;
        let gas = calls.gross_gas(outcome);
        Ok((calls, gas))
    }
}

impl TxBaseCallHost for Evm2CallHost<'_, '_> {
    type Checkpoint = StateCheckpoint;
    type Error = HandlerError;

    fn checkpoint(&mut self) -> StateCheckpoint {
        self.host.state().checkpoint()
    }

    fn revert(&mut self, checkpoint: StateCheckpoint) {
        let features = self.host.version().features;
        self.host.state_mut().rollback(checkpoint, features);
    }

    /// EVM2 checkpoints are journal positions: the phase's writes simply stay in the transaction
    /// journal, revertible until the transaction is finalized.
    fn commit(&mut self, _checkpoint: StateCheckpoint) {}

    fn root_call_access_gas(&mut self, to: Address, target_prepaid: bool) -> HandlerResult<u64> {
        let gas_params = &self.host.version().gas_params;
        let warm = gas_params.get(GasId::WarmStorageReadCost);
        let cold = warm.saturating_add(gas_params.cold_account_additional_cost());
        let access = |is_cold: bool| if is_cold { cold } else { warm };
        let target = self.load_account(to)?;
        let mut gas = if target_prepaid { 0 } else { access(target.is_cold) };
        if let Some(delegate) = target.code.eip7702_address() {
            gas += access(self.load_account(delegate)?.is_cold);
        }
        Ok(gas)
    }

    fn creates_account(
        &mut self,
        sender: Address,
        to: Address,
        value: U256,
    ) -> HandlerResult<bool> {
        if value.is_zero() || to == sender {
            return Ok(false);
        }
        let state = self.host.state_mut();
        if !state.account(&to)?.get().is_none_or(AccountInfo::is_empty) {
            return Ok(false);
        }
        Ok(state.account(&sender)?.balance() >= value)
    }

    fn run_call(
        &mut self,
        caller: Address,
        call: &Call,
        gas_limit: u64,
    ) -> HandlerResult<TxBaseCallFrame> {
        let target = self.load_account(call.to)?;
        let (code, code_address) = match target.code.eip7702_address() {
            Some(delegate) => (self.load_account(delegate)?.code, delegate),
            None => (target.code, call.to),
        };
        let mut message = MessageExt {
            kind: MessageKind::Call,
            depth: self.host.version().initial_call_depth,
            gas_limit,
            destination: call.to,
            call_target: call.to,
            caller,
            input: call.data.clone().into(),
            value: call.value,
            code,
            code_address,
            ..MessageExt::default()
        };
        // The executed frame's gas is settled: a halt has consumed all of it, and a failed frame's
        // refund is dropped.
        let result = Host::execute_message(self.host, self.tx_env, &mut message)?;
        Ok(TxBaseCallFrame {
            success: result.stop.is_success(),
            gas_spent: gas_limit.saturating_sub(result.gas.remaining()),
            refund: result.gas.refunded(),
            output: result.output,
        })
    }

    fn invalid(error: TxBaseError) -> HandlerError {
        TxBaseHandler::reject(error)
    }
}
