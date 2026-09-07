use alloc::vec::Vec;

use alloy_primitives::{Address, U256};

use super::{
    InitialFrame, LazyAuthorization, access_list_counts, effective_gas_price, floor_gas,
    initial_gas_and_reservoir, intrinsic_gas, prepare_initial_frame, runtime_oog_result,
    settle_initial_frame_gas, validate_block_gas_limit, validate_chain_id,
    validate_create_initcode, validate_execution_gas_limit_cap, validate_floor_gas,
    validate_gas_price, validate_intrinsic_gas, validate_nonce_not_overflow, validate_priority_fee,
    validate_sender, validate_tx_gas_limit_cap, warm_access_list, warm_base_accounts,
};
use crate::{
    Evm, EvmFeatures, EvmTypes, TxResult, Version,
    env::TxEnvExt,
    evm::{
        error_handler,
        handler::{DefaultTxHandlerHooks, GasSettlement, TxHandlerHooks},
    },
    interpreter::{GasTracker, Host, InstrStop, MessageResult, gas::EIP8038_ACCOUNT_WRITE},
    registry::{HandlerError, HandlerResult, TxRequest},
    version::GasId,
};

/// Executes an EIP-7702 transaction using Ethereum rules.
pub fn handle<T: EvmTypes>(
    req: TxRequest<'_, '_, T, super::LazyTxEip7702>,
) -> HandlerResult<TxResult<T>> {
    handle_with_hooks::<T, DefaultTxHandlerHooks>(req)
}

/// Executes an EIP-7702 transaction using Ethereum rules and custom handler hooks.
pub fn handle_with_hooks<T: EvmTypes, H: TxHandlerHooks<T>>(
    req: TxRequest<'_, '_, T, super::LazyTxEip7702>,
) -> HandlerResult<TxResult<T>> {
    let caller = req.tx.signer();
    let tx = req.tx.inner();
    let envelope = req.envelope;
    if tx.authorization_list.is_empty() {
        return Err(HandlerError::EmptyAuthorizationList);
    }
    let max_fee_per_gas = U256::from(tx.max_fee_per_gas);
    let max_priority_fee_per_gas = U256::from(tx.max_priority_fee_per_gas);
    let gas_price =
        effective_gas_price(max_fee_per_gas, max_priority_fee_per_gas, req.host.block.basefee);

    validate_priority_fee(req.host.version(), max_fee_per_gas, max_priority_fee_per_gas)?;
    validate_gas_price(req.host.version(), gas_price, req.host.block.basefee)?;
    validate_chain_id(req.host.version(), Some(tx.chain_id), false)?;
    validate_tx_gas_limit_cap(req.host.version(), tx.gas_limit)?;
    validate_block_gas_limit(req.host.version(), tx.gas_limit, req.host.block.gas_limit)?;
    validate_create_initcode(req.host.version(), tx.to.into(), &tx.input)?;
    validate_nonce_not_overflow(tx.nonce)?;
    let (access_list_accounts, access_list_storage_keys) = access_list_counts(&tx.access_list);
    let mut intrinsic = intrinsic_gas(
        req.host.version(),
        caller,
        tx.to.into(),
        &tx.input,
        access_list_accounts,
        access_list_storage_keys,
        tx.value,
    ) + eip7702_authorization_gas(req.host, tx.authorization_list.len());
    // EIP-2780 (ethereum/EIPs#11844): the per-auth state-dependent charges are applied at the
    // runtime gas phase, so no state gas is charged at the intrinsic phase (pre-Amsterdam there is
    // none either). A transaction that passes the intrinsic check but cannot afford the runtime
    // charges is included as an out-of-gas halt rather than rejected.
    let mut initial_state_gas = 0;
    H::adjust_intrinsic_gas(req.host, envelope, &mut intrinsic, &mut initial_state_gas)?;
    validate_intrinsic_gas(tx.gas_limit, intrinsic, initial_state_gas)?;
    let floor_gas = floor_gas(
        req.host.version(),
        caller,
        tx.to.into(),
        &tx.input,
        access_list_accounts,
        access_list_storage_keys,
        tx.value,
    );
    validate_floor_gas(tx.gas_limit, floor_gas)?;
    validate_execution_gas_limit_cap(req.host.version(), tx.gas_limit, intrinsic, floor_gas)?;

    let max_gas_cost = U256::from(tx.gas_limit) * max_fee_per_gas;
    validate_sender(req.host, caller, tx.nonce, max_gas_cost.saturating_add(tx.value))?;

    warm_base_accounts(req.host, caller, tx.to.into());
    warm_access_list(req.host, &tx.access_list);

    let effective_gas_cost = U256::from(tx.gas_limit) * gas_price;
    req.host.state.account(&caller, false).map_err(error_handler!(req.host))?.bump_nonce();
    H::before_execution(req.host, envelope, caller, effective_gas_cost)?;
    let chain_id = req.host.version().chain_id;
    let tx_env = TxEnvExt {
        origin: caller,
        gas_price,
        chain_id: U256::from(chain_id),
        ..TxEnvExt::default()
    };

    let (execution_gas_limit, reservoir) =
        initial_gas_and_reservoir(req.host.version(), tx.gas_limit, intrinsic, initial_state_gas);
    let mut tx_gas =
        GasTracker::new_with_execution_gas_and_reservoir(execution_gas_limit, reservoir);
    // The delegations span `runtime_checkpoint` so a runtime out-of-gas can drop them; the
    // recipient is read only afterwards (at first-frame creation), so it too stays out of the
    // EIP-7928 block access list on an authorization out-of-gas. Pre-Amsterdam nothing rolls
    // the checkpoint back.
    let runtime_checkpoint = req.host.state.checkpoint();

    // The authorization gas phase. Under EIP-2780 (ethereum/EIPs#11844) the runtime charges are
    // metered on the transaction-level gas tracker as the delegations are applied, stopping at the
    // first unaffordable charge — later authorities are never loaded, keeping them out of the
    // block access list. Pre-Amsterdam the pessimistic per-auth intrinsic charge is refilled
    // instead and never runs out of gas: an execution refund for each already-existing authority,
    // and under EIP-8037 a state refund credited directly back to the reservoir so it stays state
    // gas — per execution-specs `set_delegation` (`state_gas_reservoir += refund`), deliberately
    // not routed through execution gas first.
    let (auth_oog, state_refund, execution_refund) = if req.host.feature(EvmFeatures::EIP2780) {
        let mut auth_charges =
            RuntimeAuthCharges::new(req.host.version(), &mut tx_gas, caller, tx.to, tx.value);
        let oog = apply_auth_list(req.host, chain_id, &tx.authorization_list, &mut auth_charges)?;
        (oog, 0, 0)
    } else {
        let mut auth_refunds = AuthRefunds::new(req.host.version());
        apply_auth_list(req.host, chain_id, &tx.authorization_list, &mut auth_refunds)?;
        let AuthRefunds { state_refund, execution_refund, .. } = auth_refunds;
        tx_gas.set_reservoir(tx_gas.reservoir() + state_refund);
        (false, state_refund, execution_refund)
    };

    // Applies the pre-Amsterdam authorization execution refund (zero under EIP-2780) and settles
    // the transaction with the hook-provided intrinsic state gas (charged upfront, before
    // `runtime_checkpoint`, so it persists on every exit). Every exit below funnels through here.
    let settle = |host: &mut Evm<'_, T>, mut result: MessageResult<T>| {
        result.gas.set_refunded(
            result
                .gas
                .refunded()
                .saturating_add(i64::try_from(execution_refund).unwrap_or(i64::MAX)),
        );
        H::settle_transaction(
            host,
            envelope,
            GasSettlement {
                caller,
                gas_price,
                gas_limit: tx.gas_limit,
                floor_gas,
                initial_state_gas,
                state_refund,
                result,
            },
        )
    };
    // Settles the transaction as an out-of-gas halt when the runtime gas phase (the authorization
    // charges or the first-frame recipient charge) runs out of gas: reverts the authorization
    // checkpoint to drop the applied delegations, then consumes all execution gas and returns the
    // reservoir. Unreachable pre-Amsterdam, where no runtime charge is attempted.
    let settle_oog = |host: &mut Evm<'_, T>| {
        let features = host.version().features;
        host.state.rollback(runtime_checkpoint, features);
        settle(host, runtime_oog_result(execution_gas_limit, reservoir))
    };

    if auth_oog {
        return settle_oog(req.host);
    }
    let Some(InitialFrame { mut message, charged_state_gas }) = prepare_initial_frame(
        req.host,
        caller,
        tx.nonce,
        tx.to.into(),
        &tx.input,
        tx.value,
        &mut tx_gas,
    )?
    else {
        // A depth-0 recipient charge that ran out of gas is part of the runtime gas phase, so it
        // drops the delegations too.
        return settle_oog(req.host);
    };

    // Failed execution has already been rolled back to the message's own checkpoint (past the
    // applied delegations, which stay) inside `execute_message`. The settle merges the frame gas
    // into `tx_gas`, which carries the authorization state gas into the block state-gas
    // accounting.
    let mut result = req.host.execute_message(&tx_env, &mut message);
    settle_initial_frame_gas(&mut tx_gas, &mut result, charged_state_gas);
    settle(req.host, result)
}

fn eip7702_authorization_gas<'a, T: EvmTypes>(host: &Evm<'a, T>, authorizations: usize) -> u64 {
    let per_auth = u64::from(host.version().gas_params.get(GasId::TxEip7702PerEmptyAccountCost));
    (authorizations as u64).saturating_mul(per_auth)
}

/// Outcome of validating one EIP-7702 authorization, carrying the facts needed to compute its gas
/// charges (execution-specs `set_delegation`).
#[derive(Clone, Copy, Debug)]
pub struct AppliedAuth {
    /// Whether the authority account already existed when this authorization was processed.
    pub existed: bool,
    /// Whether the authority's code was a valid delegation at the start of the transaction.
    pub delegated_before_tx: bool,
    /// Whether the authority's code was a valid delegation when this authorization was processed
    /// (i.e. as left by an earlier authorization for the same authority in this transaction).
    pub delegated_now: bool,
    /// Whether this authorization clears the delegation (target is the zero address).
    pub clearing: bool,
}

/// Validates one authorization against current state without applying it. Returns
/// `Some((authority, facts))` for an accepted authorization or `None` for a rejected one. Mirrors
/// execution-specs `validate_authorization`.
pub fn validate_one_auth<'a, T: EvmTypes>(
    host: &mut Evm<'a, T>,
    chain_id: u64,
    authorization: &LazyAuthorization,
) -> HandlerResult<Option<(Address, AppliedAuth)>> {
    if !authorization.chain_id().is_zero() && authorization.chain_id() != &U256::from(chain_id) {
        return Ok(None);
    }
    if authorization.nonce() == u64::MAX {
        return Ok(None);
    }
    let Some(authority) = authorization.authority() else {
        return Ok(None);
    };
    let mut account = host.state.account(&authority, false).map_err(error_handler!(host))?;
    account.warm();
    let existed = account.exists();
    let authority_nonce = account.nonce();
    let code = account.load_code().map_err(error_handler!(host))?;
    // Reject an authority that already carries non-delegation code; otherwise non-empty code is
    // necessarily a valid delegation.
    let delegated_now = !code.is_empty();
    if delegated_now && !code.is_eip7702() {
        return Ok(None);
    }
    if authorization.nonce() != authority_nonce {
        return Ok(None);
    }
    let delegated_before_tx = account.original_code().map_err(error_handler!(host))?.is_eip7702();
    let clearing = authorization.address().is_zero();
    Ok(Some((authority, AppliedAuth { existed, delegated_before_tx, delegated_now, clearing })))
}

/// Gas accounting for one regime of [`apply_auth_list`].
///
/// The loop validates each authorization and applies the accepted ones; the accounting decides
/// what each outcome costs or refunds. [`RuntimeAuthCharges`] meters the EIP-2780 runtime charges
/// and can abort the list, [`AuthRefunds`] accumulates the pessimistic-intrinsic refunds and never
/// fails; handlers with their own authorization pricing supply their own implementation.
pub trait AuthAccounting {
    /// Called for a rejected authorization before moving to the next entry.
    fn rejected(&mut self);

    /// Called for an accepted authorization before its delegation is applied. Returning
    /// out-of-gas aborts the list: the delegation is not applied and no later authority is
    /// loaded.
    fn accepted(&mut self, authority: Address, auth: &AppliedAuth) -> Result<(), InstrStop>;
}

/// EIP-2780 runtime accounting: meters the state-dependent charges on the transaction-level gas
/// tracker as the delegations are applied (ethereum/EIPs#11844, #11891).
///
/// Per accepted authority: the new-account state gas when the authority does not exist,
/// `ACCOUNT_WRITE` execution gas on the first write to the authority's leaf (unless already paid —
/// the sender at inclusion, the recipient of a value-bearing transaction, or a preceding valid
/// authorization on the same authority), and the net-new delegation-indicator state gas.
///
/// Rejected authorizations charge nothing (the intrinsic `REGULAR_PER_AUTH_BASE_COST` already
/// covers their work) and are not refunded.
#[derive(Debug)]
pub struct RuntimeAuthCharges<'g> {
    gas: &'g mut GasTracker,
    new_account_state_gas: u64,
    delegation_bytes_state_gas: u64,
    account_write_cost: u64,
    /// Accounts whose leaf write this transaction has already paid for.
    written: Vec<Address>,
    /// Authorities whose net-new delegation bytes were already charged; the charge applies at most
    /// once per authority (covering a set-clear-set sequence within one transaction).
    charged_delegation_bytes: Vec<Address>,
}

impl<'g> RuntimeAuthCharges<'g> {
    /// Creates the runtime accounting for a transaction from `caller` to `recipient` carrying
    /// `value`. The sender's leaf write is priced into `TX_BASE` and the value-bearing recipient's
    /// into `TX_VALUE_COST`, so neither pays `ACCOUNT_WRITE` again.
    pub fn new(
        version: &Version,
        gas: &'g mut GasTracker,
        caller: Address,
        recipient: Address,
        value: U256,
    ) -> Self {
        let mut written = Vec::new();
        written.push(caller);
        if !value.is_zero() {
            written.push(recipient);
        }
        Self {
            gas,
            new_account_state_gas: version.gas_params.new_account_state_gas(),
            delegation_bytes_state_gas: u64::from(
                version.gas_params.get(GasId::TxEip7702PerAuthState),
            ),
            account_write_cost: u64::from(EIP8038_ACCOUNT_WRITE),
            written,
            charged_delegation_bytes: Vec::new(),
        }
    }
}

impl AuthAccounting for RuntimeAuthCharges<'_> {
    fn rejected(&mut self) {}

    fn accepted(&mut self, authority: Address, auth: &AppliedAuth) -> Result<(), InstrStop> {
        // Non-existent authority: pay for the new account leaf's state bytes.
        if !auth.existed {
            self.gas.spend_state(self.new_account_state_gas)?;
        }
        // First write to the authority's leaf within the transaction pays `ACCOUNT_WRITE`.
        if !self.written.contains(&authority) {
            self.gas.spend(self.account_write_cost)?;
            self.written.push(authority);
        }
        // Net-new delegation bytes: the 23-byte designator written into a previously empty slot.
        if !auth.clearing
            && !auth.delegated_now
            && !auth.delegated_before_tx
            && !self.charged_delegation_bytes.contains(&authority)
        {
            self.gas.spend_state(self.delegation_bytes_state_gas)?;
            self.charged_delegation_bytes.push(authority);
        }
        Ok(())
    }
}

/// Pre-EIP-2780 accounting: refunds against the pessimistic per-authorization intrinsic charge.
///
/// Follows execution-specs `set_delegation`. The per-authorization state and execution gas charged
/// in the intrinsic cost is refilled when it turns out not to be needed: the state refund is
/// credited to the reservoir (so it stays state gas) and the execution refund is routed through
/// the capped refund counter.
///
/// Before EIP-8037 (Prague) there is no state gas: only the per-existing-account execution refund
/// applies and rejected authorizations refund nothing.
#[derive(Clone, Copy, Debug)]
pub struct AuthRefunds {
    is_eip8037: bool,
    new_account: u64,
    auth_base: u64,
    execution_per_auth: u64,
    /// Accumulated state-gas refund.
    pub state_refund: u64,
    /// Accumulated execution-gas refund.
    pub execution_refund: u64,
}

impl AuthRefunds {
    /// Creates refund accounting with `version`'s authorization prices.
    pub fn new(version: &Version) -> Self {
        Self {
            is_eip8037: version.feature(EvmFeatures::EIP8037),
            new_account: version.gas_params.new_account_state_gas(),
            auth_base: u64::from(version.gas_params.get(GasId::TxEip7702PerAuthState)),
            execution_per_auth: u64::from(version.gas_params.get(GasId::TxEip7702AuthRefund)),
            state_refund: 0,
            execution_refund: 0,
        }
    }
}

impl AuthAccounting for AuthRefunds {
    fn rejected(&mut self) {
        // Rejected authorization. Under EIP-8037 its full intrinsic state gas (account + bytecode)
        // refills the reservoir and the speculative account write is refunded; before EIP-8037
        // nothing is refunded.
        if self.is_eip8037 {
            self.state_refund = self.state_refund.saturating_add(self.new_account + self.auth_base);
            self.execution_refund = self.execution_refund.saturating_add(self.execution_per_auth);
        }
    }

    fn accepted(&mut self, _authority: Address, auth: &AppliedAuth) -> Result<(), InstrStop> {
        // Existing authority: the worst-case `ACCOUNT_WRITE` execution gas was not needed. This
        // refund applies in every regime (it is the only authorization refund before EIP-8037).
        if auth.existed {
            self.execution_refund = self.execution_refund.saturating_add(self.execution_per_auth);
        }

        // The remaining refunds are state gas, which only exists under EIP-8037.
        if !self.is_eip8037 {
            return Ok(());
        }

        let mut refund = 0u64;
        // Existing authority: its `NEW_ACCOUNT` state gas was not needed.
        if auth.existed {
            refund += self.new_account;
        }
        // Bytecode (`AUTH_BASE`) refunds.
        if auth.clearing {
            refund += self.auth_base;
            // Clearing a delegation freshly installed earlier in this transaction refills the
            // bytecode state gas a second time.
            if auth.delegated_now && !auth.delegated_before_tx {
                refund += self.auth_base;
            }
        } else if auth.delegated_now || auth.delegated_before_tx {
            refund += self.auth_base;
        }
        self.state_refund = self.state_refund.saturating_add(refund);
        Ok(())
    }
}

/// Validates and applies an EIP-7702 authorization list, driving `accounting` with each
/// per-authorization outcome.
///
/// Each authorization is validated against current state ([`validate_one_auth`]); the accounting
/// is told about rejected entries and charges (or accumulates refunds) for accepted ones before
/// their delegation is applied. An accounting out-of-gas aborts the list without loading the
/// remaining authorities — keeping them out of the EIP-7928 block access list — and returns
/// `true`; the caller is responsible for rolling back the partially applied delegations.
pub fn apply_auth_list<'a, T: EvmTypes>(
    host: &mut Evm<'a, T>,
    chain_id: u64,
    authorizations: &[LazyAuthorization],
    accounting: &mut impl AuthAccounting,
) -> HandlerResult<bool> {
    for authorization in authorizations {
        let Some((authority, auth)) = validate_one_auth(host, chain_id, authorization)? else {
            accounting.rejected();
            continue;
        };
        if accounting.accepted(authority, &auth).is_err() {
            return Ok(true);
        }
        host.state
            .account(&authority, false)
            .map_err(error_handler!(host))?
            .set_delegation(*authorization.address());
    }
    Ok(false)
}
