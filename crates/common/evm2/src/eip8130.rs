//! Enshrined EIP-8130 (account-abstraction, type `0x79`) transaction execution.
//!
//! [`Eip8130Handler`] runs an EIP-8130 transaction through the engine-neutral pipeline shared with
//! the revm reference ([`Eip8130Pipeline`] and [`Eip8130Calls`], documented in
//! `base-execution-eip8130`), supplying the EVM2 state, call frames, and OP-stack fee routing.
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
//! A read-only simulation ([`Eip8130Simulation`]) resolves the sender from the recovered signer
//! without verifying any signature or touching the nonce, never charges or settles fees, and
//! rolls every state change back while keeping the logs of the reported run.

use alloy_primitives::{Address, U256};
use base_common_consensus::{Call, Predeploys};
use base_common_genesis::BaseUpgrade;
use base_execution_eip8130::{
    Eip8130BlockContext, Eip8130CallFrame, Eip8130CallHost, Eip8130Calls, Eip8130CallsResult,
    Eip8130Error, Eip8130FeeSplit, Eip8130Outcome, Eip8130Pipeline,
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
    BaseEvmTypes, BaseTime, BaseTxResultExt, Eip8130Simulation, Eip8130SimulationMode, Eip8130Tx,
    L1Block,
};

/// The pre-call state an EIP-8130 transaction carries from preparation into execution.
#[derive(Debug, Clone)]
pub struct Eip8130Prepared {
    /// The resolved actors and gas/fee parameters.
    pub outcome: Eip8130Outcome,
    /// How the transaction settles.
    pub settlement: Eip8130Settlement,
}

/// How a prepared EIP-8130 transaction settles.
#[derive(Debug, Clone)]
pub enum Eip8130Settlement {
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
        simulation: Eip8130Simulation,
        /// The state checkpoint taken before any simulated write.
        start: StateCheckpoint,
    },
}

/// Executes enshrined EIP-8130 transactions on EVM2.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Eip8130Handler;

impl Eip8130Handler {
    /// Checks the transaction against the block, runs the pre-call pipeline, and pre-charges the
    /// payer (or, for a simulation, resolves the pre-call state without verification). Every
    /// rejection leaves the transaction uncommitted.
    pub fn prepare(
        req: &mut TxRequest<'_, '_, BaseEvmTypes, Eip8130Tx>,
    ) -> HandlerResult<Eip8130Prepared> {
        let signer = req.tx.signer();
        let tx: &Eip8130Tx = *req.tx;
        let host = &mut *req.host;
        let upgrade = host.config_spec_id().upgrade();
        // Consensus-critical: the EIP-8130 transaction type exists only from Everest. Every
        // ingress path gates it, but a block delivered by the Engine API or P2P reaches execution
        // directly.
        if upgrade < BaseUpgrade::Everest {
            return Err(Self::reject(Eip8130Pipeline::not_active_error()));
        }
        host.ext_mut().tx_origin = signer;
        let now = Eip8130Pipeline::block_timestamp(host.block().timestamp).map_err(Self::reject)?;
        let mut block = Eip8130BlockContext {
            chain_id: host.version().chain_id,
            now,
            now_ms: 0,
            base_fee: host.block().basefee.saturating_to(),
            tx_gas_limit_cap: host.version().tx_gas_limit_cap,
            keystore: upgrade >= BaseUpgrade::Zenith,
        };

        if let Some(simulation) = tx.simulation {
            let start = host.state().checkpoint();
            let outcome = Self::run_pre_call(host, |sctx| {
                Eip8130Pipeline::simulate_resolve(
                    sctx,
                    &tx.signed,
                    &tx.enveloped,
                    signer,
                    simulation.sender_actor_id,
                    &block,
                )
            })?;
            host.ext_mut().tx_origin = outcome.sender;
            return Ok(Eip8130Prepared {
                outcome,
                settlement: Eip8130Settlement::Simulated { simulation, start },
            });
        }

        Eip8130Pipeline::check_chain_id(tx.signed.tx(), block.chain_id).map_err(Self::reject)?;
        // Everest implies Denim, so the block's millisecond part is already in `BaseTime`.
        block.now_ms = BaseTime::timestamp_ms(host, now)?;
        L1Block::ensure_fee_params(host)?;
        Eip8130Pipeline::check_validity_window(tx.signed.tx(), block.now_ms)
            .map_err(Self::reject)?;
        let outcome = Self::run_pre_call(host, |sctx| {
            Eip8130Pipeline::authorize_and_apply(sctx, &tx.signed, &tx.enveloped, &block)
        })?;
        let (prepay, l1_fee) = Self::prepay(host, &outcome, tx)?;
        Ok(Eip8130Prepared { outcome, settlement: Eip8130Settlement::Verified { prepay, l1_fee } })
    }

    /// Dispatches the prepared transaction's `calls` and settles (or, for a simulation, measures
    /// and rolls back) the transaction.
    pub fn execute(
        req: TxRequest<'_, '_, BaseEvmTypes, Eip8130Tx>,
        prepared: Eip8130Prepared,
    ) -> HandlerResult<TxResult<BaseEvmTypes>> {
        let tx: &Eip8130Tx = *req.tx;
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
            Eip8130Settlement::Verified { prepay, l1_fee } => {
                let calls = Eip8130Calls::execute(
                    &mut calls_host,
                    &tx.signed,
                    &outcome,
                    outcome.execution_gas_available,
                )?;
                let gas_used = Self::settle_fees(host, &outcome, &calls, prepay, l1_fee, tx)?;
                Ok(Self::result(calls, gas_used, l1_fee))
            }
            Eip8130Settlement::Simulated { simulation, start } => {
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
        pipeline: impl FnOnce(StorageCtx<'_>) -> Result<Eip8130Outcome, Eip8130Error>,
    ) -> HandlerResult<Eip8130Outcome> {
        let mut provider = Evm2JournalStorageProvider::new(host, Address::ZERO);
        let outcome = StorageCtx::enter(&mut provider, pipeline);
        if let Some(error) = provider.take_database_error() {
            return Err(HandlerError::Fatal(AnyError::from(error)));
        }
        outcome.map_err(Self::reject)
    }

    /// Bumps the sender's protocol nonce (for `nonce_key == 0`) and pre-charges the payer the
    /// worst-case fee, so the `calls` cannot spend the gas reservation. Returns the amount debited
    /// and its L1 data fee component.
    fn prepay(
        host: &mut Evm<'_, BaseEvmTypes>,
        outcome: &Eip8130Outcome,
        tx: &Eip8130Tx,
    ) -> HandlerResult<(U256, U256)> {
        host.ext_mut().tx_origin = outcome.sender;
        if outcome.bump_protocol_nonce {
            host.state_mut().account(&outcome.sender)?.bump_nonce();
        }

        let upgrade = host.config_spec_id().upgrade();
        let max_gas = outcome.max_chargeable_gas();
        let gas_charge = U256::from(max_gas)
            .checked_mul(U256::from(outcome.effective))
            .ok_or_else(|| Self::invalid("EIP-8130 gas pre-charge overflow"))?;
        let l1_fee = host.block().ext.calculate_tx_l1_cost(&tx.enveloped, upgrade);
        let operator_fee =
            host.block().ext.operator_fee_charge(&tx.enveloped, U256::from(max_gas), upgrade);
        let prepay = gas_charge
            .checked_add(l1_fee)
            .and_then(|amount| amount.checked_add(operator_fee))
            .ok_or_else(|| Self::invalid("EIP-8130 pre-charge overflow"))?;

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
        outcome: &Eip8130Outcome,
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
        outcome: &Eip8130Outcome,
        calls: &Eip8130CallsResult,
        prepay: U256,
        l1_fee: U256,
        tx: &Eip8130Tx,
    ) -> HandlerResult<u64> {
        let billable_gas = calls.billable_gas(outcome);
        let split = Eip8130FeeSplit::new(outcome, billable_gas).map_err(Self::reject)?;
        let upgrade = host.config_spec_id().upgrade();
        let operator_fee =
            host.block().ext.operator_fee_charge(&tx.enveloped, U256::from(billable_gas), upgrade);
        // The pre-charge bounds every settled component, so the refund never underflows.
        let total_cost = split
            .fee
            .checked_add(l1_fee)
            .and_then(|amount| amount.checked_add(operator_fee))
            .ok_or_else(|| Self::invalid("EIP-8130 settled cost overflow"))?;
        let refund = prepay
            .checked_sub(total_cost)
            .ok_or_else(|| Self::invalid("settled fee exceeds the worst-case pre-charge"))?;
        {
            let mut payer = host.state_mut().account(&outcome.payer)?;
            let balance = payer
                .balance()
                .checked_add(refund)
                .ok_or_else(|| Self::invalid("EIP-8130 payer refund balance overflow"))?;
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
    fn result(calls: Eip8130CallsResult, gas: u64, l1_fee: U256) -> TxResult<BaseEvmTypes> {
        TxResult::<BaseEvmTypes> {
            status: !calls.reverted,
            total_gas_spent: gas,
            stop: if calls.reverted { InstrStop::Revert } else { InstrStop::Return },
            output: calls.output,
            ext: BaseTxResultExt { l1_fee, eip8130_committed_phases: Some(calls.committed_phases) },
            ..Default::default()
        }
    }

    /// Maps an EIP-8130 rejection onto the handler error. A payer that cannot cover the fee is
    /// reported as the standard insufficient-funds error, which block builders skip rather than
    /// abort on.
    fn reject(error: Eip8130Error) -> HandlerError {
        match error {
            Eip8130Error::PayerCannotPay { balance, required } => {
                HandlerError::InsufficientFunds { fee: required, balance }
            }
            error @ Eip8130Error::Invalid(_) => HandlerError::external(error),
        }
    }

    fn invalid(reason: &str) -> HandlerError {
        Self::reject(Eip8130Error::invalid(reason))
    }
}

/// The EVM2 state and frames as the [`Eip8130CallHost`] of [`Eip8130Calls::execute`].
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
                "unexpected halt loading an EIP-8130 call target: {stop:?}"
            ))),
        })
    }

    /// Runs the calls at `pool` and reverts every write (and log) they made.
    fn probe(
        &mut self,
        tx: &Eip8130Tx,
        outcome: &Eip8130Outcome,
        pool: u64,
    ) -> HandlerResult<Eip8130CallsResult> {
        let checkpoint = self.checkpoint();
        let calls = Eip8130Calls::execute(self, &tx.signed, outcome, pool)?;
        self.revert(checkpoint);
        Ok(calls)
    }

    /// Measures a simulated transaction's calls, returning the reported run and its gas: the
    /// metered gas of a single run for `eth_call` (or a run that fails even at the full pool), or
    /// the smallest verified-feasible gas limit for `eth_estimateGas`.
    fn simulate(
        &mut self,
        tx: &Eip8130Tx,
        outcome: &Eip8130Outcome,
        mode: Eip8130SimulationMode,
    ) -> HandlerResult<(Eip8130CallsResult, u64)> {
        let ceiling_pool = outcome.execution_gas_available;
        if mode == Eip8130SimulationMode::Estimate {
            let ceiling = self.probe(tx, outcome, ceiling_pool)?;
            if !ceiling.reverted {
                let feasible_pool = Eip8130Calls::search_estimate_pool(
                    ceiling.call_gas_spent,
                    ceiling_pool,
                    |pool| Ok::<_, HandlerError>(self.probe(tx, outcome, pool)?.reverted),
                )?;
                let calls = Eip8130Calls::execute(self, &tx.signed, outcome, feasible_pool)?;
                // The on-chain call pool at this limit is `gas_limit - intrinsic` (payer
                // authentication is billed on top), so it is at least the verified-feasible pool;
                // the EIP-7623 floor keeps a data-heavy estimate admissible.
                let gas = outcome
                    .floored_sender_gas(outcome.sender_intrinsic.saturating_add(feasible_pool))
                    .saturating_add(outcome.payer_auth);
                return Ok((calls, gas));
            }
        }
        let calls = Eip8130Calls::execute(self, &tx.signed, outcome, ceiling_pool)?;
        let gas = calls.gross_gas(outcome);
        Ok((calls, gas))
    }
}

impl Eip8130CallHost for Evm2CallHost<'_, '_> {
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

    fn root_call_access_gas(&mut self, to: Address) -> HandlerResult<u64> {
        let gas_params = &self.host.version().gas_params;
        let warm = gas_params.get(GasId::WarmStorageReadCost);
        let cold = warm.saturating_add(gas_params.cold_account_additional_cost());
        let access = |is_cold: bool| if is_cold { cold } else { warm };
        let target = self.load_account(to)?;
        let mut gas = access(target.is_cold);
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
    ) -> HandlerResult<Eip8130CallFrame> {
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
            input: call.data.clone(),
            value: call.value,
            code,
            code_address,
            ..MessageExt::default()
        };
        // The executed frame's gas is settled: a halt has consumed all of it, and a failed frame's
        // refund is dropped.
        let result = Host::execute_message(self.host, self.tx_env, &mut message)?;
        Ok(Eip8130CallFrame {
            success: result.stop.is_success(),
            gas_spent: gas_limit.saturating_sub(result.gas.remaining()),
            refund: result.gas.refunded(),
            output: result.output,
        })
    }

    fn invalid(error: Eip8130Error) -> HandlerError {
        Eip8130Handler::reject(error)
    }
}
