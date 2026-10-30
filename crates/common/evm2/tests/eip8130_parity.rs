//! Differential parity harness for enshrined EIP-8130 (type `0x79`) execution.
//!
//! Runs signed EIP-8130 transactions through the evm2 engine ([`BaseEvmTypes::tx_registry`],
//! [`BaseBlockExecutor`]) and the revm reference (`base-common-evm`'s `Eip8130Executor` and
//! block executor) from identical pre-state, asserting the engines agree on everything consensus
//! observes: inclusion or rejection (and why), status, gas used, output, logs, committed phases,
//! and the post-state balances, nonces, code, and storage of every account either engine touched.
//!
//! Scenarios cover EOA self-pay, phased calls with a reverting or halting phase, payer
//! sponsorship, account changes (counterfactual create, delegation, config change), 2D and
//! nonce-free nonces with the replay ring and validity window, the policy gate, rejected
//! transactions (insufficient payer funds, invalid authentication, inactive fork), read-only
//! simulation, and a block mixing deposit, standard, and EIP-8130 transactions.

use std::collections::{BTreeMap, BTreeSet};

use alloy_consensus::{SignableTransaction, TxEip1559, TxReceipt, transaction::Recovered};
use alloy_eips::{Encodable2718, Typed2718};
use alloy_evm::{Evm as _, FromTxWithEncoded as _, block::BlockExecutor as _};
use alloy_hardforks::ForkCondition;
use alloy_primitives::{
    Address, B256, Bytes, KECCAK256_EMPTY, Log, Sealed, Signature, TxKind, U256, address, keccak256,
};
use alloy_sol_types::{SolValue, sol};
use base_common_chains::ChainUpgrades;
use base_common_consensus::{
    AccountChange, AccountChangeChannel, BaseReceipt, BaseReceiptEnvelope,
    BaseTxEnvelope as ConsensusTx, Call, ChangeType, CreateEntry, Delegation, Eip8130Constants,
    Eip8130Signed, InitialActor, SignedAccountChanges, SignedChange, TxDeposit, TxEip8130,
};
use base_common_evm::{
    AlloyReceiptBuilder, BaseBlockExecutionCtx as RevmBlockExecutionCtx,
    BaseBlockExecutor as RevmBlockExecutor, BaseSpecId as RevmBaseSpecId, BaseTransaction,
    BaseTransactionError, Builder, DefaultBase, Eip8130CommittedPhases, Eip8130ExecutionMode,
    L1BlockInfo,
};
use base_common_evm2::{
    BaseBlockExecutionCtx, BaseBlockExecutor, BaseEvmExt, BaseEvmTypes, BaseSpecId, BaseTxEnvelope,
    Eip8130Simulation, Eip8130SimulationMode,
};
use base_common_genesis::BaseUpgrade;
use base_common_l1_fees::L1FeeParams;
use base_common_precompiles::NonceManagerStorage;
use base_execution_eip8130::{
    AccountChangeApplier, AccountConfigurationStorage, ConfigChangeAuthorizer, Eip8130Error,
};
use base_precompile_storage::{Handler as _, JournalStorageProvider, StorageCtx};
use evm2::{Evm, env::BlockEnv, evm::InMemoryDB, registry::HandlerError};
use k256::ecdsa::SigningKey;
use revm::{
    Database as _, DatabaseCommit as _,
    context::{BlockEnv as RevmBlockEnv, CfgEnv, Context},
    context_interface::{
        ContextTr as _, JournalTr as _,
        result::{EVMError, ExecutionResult, InvalidTransaction},
    },
    database::{InMemoryDB as RevmDb, State},
    inspector::NoOpInspector,
    state::{AccountInfo as RevmAccountInfo, Bytecode as RevmBytecode},
};

const CHAIN_ID: u64 = 8453;
const BLOCK_NUMBER: u64 = 1;
const BLOCK_SECONDS: u64 = 1_700_000_000;
const BASE_FEE: u64 = 1_000_000_000;
const BLOCK_GAS_LIMIT: u64 = 30_000_000;
const BENEFICIARY: Address = address!("0x00000000000000000000000000000000000000bb");
const RICH: u128 = 10u128.pow(20);

/// Stores `calldata[0..32]` in slot 0, logs memory word 0, and returns it.
const STORER: Address = address!("0x00000000000000000000000000000000000000c1");
/// Reverts with empty data.
const REVERTER: Address = address!("0x00000000000000000000000000000000000000c2");
/// Halts exceptionally (`INVALID`), consuming the whole frame.
const HALTER: Address = address!("0x00000000000000000000000000000000000000c3");
/// Calls `STORER` with all-but-1/64 of its gas, so a too-tight limit starves the callee.
const FORWARDER: Address = address!("0x00000000000000000000000000000000000000c4");
/// An empty account that receives value.
const RECIPIENT: Address = address!("0x00000000000000000000000000000000000000e1");

const fn storer_code() -> Bytes {
    // PUSH1 0 CALLDATALOAD PUSH1 0 SSTORE PUSH1 32 PUSH1 0 LOG0 PUSH1 32 PUSH1 0 RETURN
    Bytes::from_static(&[
        0x60, 0x00, 0x35, 0x60, 0x00, 0x55, 0x60, 0x20, 0x60, 0x00, 0xa0, 0x60, 0x20, 0x60, 0x00,
        0xf3,
    ])
}

fn forwarder_code() -> Bytes {
    // CALL(gas, STORER, 0, 0, 0, 0, 0) with GAS forwarded, then STOP.
    let mut code = vec![0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x73];
    code.extend_from_slice(STORER.as_slice());
    code.extend_from_slice(&[0x5a, 0xf1, 0x50, 0x00]);
    Bytes::from(code)
}

fn l1_fee_params() -> L1FeeParams {
    L1FeeParams {
        l1_base_fee: U256::from(1_000_000u64),
        l1_base_fee_scalar: U256::from(1_000u64),
        l1_blob_base_fee: Some(U256::from(10u64)),
        l1_blob_base_fee_scalar: Some(U256::from(100u64)),
        operator_fee_scalar: Some(U256::from(2_000u64)),
        operator_fee_constant: Some(U256::from(7u64)),
        da_footprint_gas_scalar: Some(U256::from(400u64)),
        ..Default::default()
    }
}

fn revm_l1_block_info() -> L1BlockInfo {
    let params = l1_fee_params();
    L1BlockInfo {
        l2_block: Some(U256::from(BLOCK_NUMBER)),
        l1_base_fee: params.l1_base_fee,
        l1_base_fee_scalar: params.l1_base_fee_scalar,
        l1_blob_base_fee: params.l1_blob_base_fee,
        l1_blob_base_fee_scalar: params.l1_blob_base_fee_scalar,
        operator_fee_scalar: params.operator_fee_scalar,
        operator_fee_constant: params.operator_fee_constant,
        da_footprint_gas_scalar: Some(400),
        ..Default::default()
    }
}

fn signing_key(byte: u8) -> SigningKey {
    SigningKey::from_slice(&[byte; 32]).unwrap()
}

fn eoa_address(key: &SigningKey) -> Address {
    let point = key.verifying_key().to_encoded_point(false);
    Address::from_slice(&keccak256(&point.as_bytes()[1..])[12..])
}

/// 65-byte `r || s || v` signature (`v` in `{27, 28}`) over `hash`.
fn eoa_sig(key: &SigningKey, hash: B256) -> Bytes {
    let (signature, recid) = key.sign_prehash_recoverable(hash.as_slice()).unwrap();
    let mut out = signature.to_bytes().to_vec();
    out.push(recid.to_byte() + 27);
    Bytes::from(out)
}

fn base_tx() -> TxEip8130 {
    TxEip8130 {
        chain_id: CHAIN_ID,
        max_priority_fee_per_gas: 1_000_000_000,
        max_fee_per_gas: 5_000_000_000,
        gas_limit: 1_000_000,
        ..Default::default()
    }
}

fn call(to: Address, value: u64, data: &[u8]) -> Call {
    Call { to, value: U256::from(value), data: Bytes::copy_from_slice(data) }
}

/// Signs `tx` for a default-EOA sender.
fn eoa_signed(tx: TxEip8130, key: &SigningKey) -> Eip8130Signed {
    let hash = tx.sender_signature_hash();
    Eip8130Signed::new(tx, eoa_sig(key, hash), Bytes::new())
}

/// Signs `tx` for a configured sender as `K1_AUTHENTICATOR || sig`.
fn configured_signed(tx: TxEip8130, key: &SigningKey) -> Eip8130Signed {
    let hash = tx.sender_signature_hash();
    let mut auth = Eip8130Constants::K1_AUTHENTICATOR.to_vec();
    auth.extend_from_slice(&eoa_sig(key, hash));
    Eip8130Signed::new(tx, Bytes::from(auth), Bytes::new())
}

/// The pre-state both engines start from.
#[derive(Clone)]
struct World {
    upgrade: BaseUpgrade,
    accounts: Vec<(Address, U256, Option<Bytes>)>,
    storage: Vec<(Address, U256, U256)>,
}

impl World {
    /// Funds `funded` at the given fork, deploys the test contracts, and plants the Everest
    /// `NonceManager` stub.
    fn new(upgrade: BaseUpgrade, funded: &[Address]) -> Self {
        let mut accounts: Vec<_> =
            funded.iter().map(|address| (*address, U256::from(RICH), None)).collect();
        accounts.extend([
            (STORER, U256::ZERO, Some(storer_code())),
            (REVERTER, U256::ZERO, Some(Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xfd]))),
            (HALTER, U256::ZERO, Some(Bytes::from_static(&[0xfe]))),
            (FORWARDER, U256::ZERO, Some(forwarder_code())),
            (NonceManagerStorage::ADDRESS, U256::ZERO, Some(Bytes::from_static(&[0xef]))),
        ]);
        Self { upgrade, accounts, storage: Vec::new() }
    }

    fn with_balance(mut self, address: Address, balance: U256) -> Self {
        self.accounts.retain(|(account, ..)| *account != address);
        self.accounts.push((address, balance, None));
        self
    }

    fn with_code(mut self, address: Address, code: Bytes) -> Self {
        self.accounts.retain(|(account, ..)| *account != address);
        self.accounts.push((address, U256::from(RICH), Some(code)));
        self
    }

    /// Seeds storage slots, creating any account that does not exist yet.
    fn with_storage(mut self, storage: impl IntoIterator<Item = (Address, U256, U256)>) -> Self {
        for (address, key, value) in storage {
            if !self.accounts.iter().any(|(account, ..)| *account == address) {
                self.accounts.push((address, U256::ZERO, None));
            }
            self.storage.push((address, key, value));
        }
        self
    }

    fn evm2(&self) -> Evm<'static, BaseEvmTypes> {
        let mut db = InMemoryDB::default();
        for (address, balance, code) in &self.accounts {
            let mut info = evm2::AccountInfo::default().with_balance(*balance);
            if let Some(code) = code {
                info = info.with_code(evm2::bytecode::Bytecode::new_raw(code.clone()));
            }
            db.insert_account_info(address, info);
        }
        for (address, key, value) in &self.storage {
            db.insert_account_storage(address, key, value);
        }
        let spec = BaseSpecId::new(self.upgrade);
        let block = BlockEnv::<BaseEvmTypes> {
            number: U256::from(BLOCK_NUMBER),
            timestamp: U256::from(BLOCK_SECONDS),
            basefee: U256::from(BASE_FEE),
            beneficiary: BENEFICIARY,
            gas_limit: U256::from(BLOCK_GAS_LIMIT),
            ext: l1_fee_params(),
            ..Default::default()
        };
        let mut version = evm2::Version::new(spec.into());
        version.chain_id = CHAIN_ID;
        Evm::new_with_execution_config(
            evm2::ExecutionConfig::for_spec_and_version(spec, version),
            spec,
            block,
            BaseEvmTypes::tx_registry(),
            db,
            BaseEvmTypes::native_precompiles(spec, None),
        )
        .with_ext(BaseEvmExt {
            l1_fee_params_block: Some(U256::from(BLOCK_NUMBER)),
            ..Default::default()
        })
    }

    fn revm_db(&self) -> RevmDb {
        let mut db = RevmDb::default();
        for (address, balance, code) in &self.accounts {
            let mut info = RevmAccountInfo { balance: *balance, ..Default::default() };
            if let Some(code) = code {
                let code = RevmBytecode::new_raw(code.clone());
                info.code_hash = code.hash_slow();
                info.code = Some(code);
            }
            db.insert_account_info(*address, info);
        }
        for (address, key, value) in &self.storage {
            db.insert_account_storage(*address, *key, *value).unwrap();
        }
        db
    }

    fn revm_block() -> RevmBlockEnv {
        RevmBlockEnv {
            number: U256::from(BLOCK_NUMBER),
            timestamp: U256::from(BLOCK_SECONDS),
            basefee: BASE_FEE,
            beneficiary: BENEFICIARY,
            gas_limit: BLOCK_GAS_LIMIT,
            ..Default::default()
        }
    }

    fn revm_cfg(&self) -> CfgEnv<RevmBaseSpecId> {
        CfgEnv::new_with_spec(RevmBaseSpecId::new(self.upgrade)).with_chain_id(CHAIN_ID)
    }

    fn revm(&self) -> base_common_evm::BaseEvm<RevmDb, NoOpInspector> {
        Context::base()
            .with_db(self.revm_db())
            .with_chain(revm_l1_block_info())
            .with_block(Self::revm_block())
            .with_cfg(self.revm_cfg())
            .build_with_inspector(NoOpInspector)
    }
}

/// Storage written by `seed` against an empty journal, for seeding both engines identically.
fn seeded_storage(seed: impl FnOnce(StorageCtx<'_>)) -> Vec<(Address, U256, U256)> {
    let mut evm = Context::base().with_db(RevmDb::default()).build_base();
    {
        let internals = alloy_evm::EvmInternals::from_context(evm.ctx_mut());
        let mut provider = JournalStorageProvider::new(internals, Address::ZERO);
        StorageCtx::enter(&mut provider, seed);
    }
    let state = evm.ctx_mut().journal_mut().finalize();
    state
        .into_iter()
        .flat_map(|(address, account)| {
            account.storage.into_iter().map(move |(key, slot)| (address, key, slot.present_value))
        })
        .collect()
}

/// Why an engine rejected a transaction.
#[derive(Debug, PartialEq, Eq)]
enum Rejection {
    /// The payer cannot cover the worst-case fee.
    InsufficientFunds,
    /// An EIP-8130 validity rule, with its reason.
    Invalid(String),
}

/// An included transaction as consensus observes it.
#[derive(Debug, PartialEq, Eq)]
struct Included {
    success: bool,
    gas_used: u64,
    output: Bytes,
    logs: Vec<Log>,
    committed_phases: Option<u64>,
}

/// Post-state of an account: balance, nonce, and code hash. Absent and empty accounts compare
/// equal.
type AccountView = (U256, u64, B256);

#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    results: Vec<Result<Included, Rejection>>,
    accounts: BTreeMap<Address, AccountView>,
    storage: BTreeMap<(Address, U256), U256>,
}

fn account_view(balance: U256, nonce: u64, code_hash: B256) -> AccountView {
    let code_hash = if code_hash.is_zero() { KECCAK256_EMPTY } else { code_hash };
    (balance, nonce, code_hash)
}

/// The touched addresses and slots whose post-state the harness compares.
#[derive(Clone, Default)]
struct Touched {
    accounts: BTreeSet<Address>,
    slots: BTreeSet<(Address, U256)>,
}

/// A transaction to run: the signed EIP-8130 transaction, the account it executes as (the
/// recovered signer, or the simulated `from`), and the simulation request, if any.
#[derive(Clone)]
struct Run {
    signed: Eip8130Signed,
    signer: Address,
    simulation: Option<Eip8130Simulation>,
}

impl Run {
    const fn verified(signed: Eip8130Signed, signer: Address) -> Self {
        Self { signed, signer, simulation: None }
    }

    const fn simulated(signed: Eip8130Signed, from: Address, mode: Eip8130SimulationMode) -> Self {
        let simulation = Eip8130Simulation { mode, sender_actor_id: None };
        Self { signed, signer: from, simulation: Some(simulation) }
    }

    fn encoded(&self) -> (ConsensusTx, Bytes) {
        let envelope = ConsensusTx::Eip8130(self.signed.clone());
        let encoded = envelope.encoded_2718().into();
        (envelope, encoded)
    }
}

fn run_evm2(world: &World, runs: &[Run], touched: &mut Touched) -> Outcome {
    let mut evm = world.evm2();
    let mut results = Vec::new();
    for run in runs {
        let (_, encoded) = run.encoded();
        let envelope = match run.simulation {
            Some(simulation) => {
                BaseTxEnvelope::eip8130_simulation(run.signed.clone(), encoded, simulation)
            }
            None => BaseTxEnvelope::eip8130(run.signed.clone(), encoded),
        };
        match evm.transact(&Recovered::new_unchecked(envelope, run.signer)) {
            Ok(executed) => {
                for changes in executed.account_changes() {
                    touched.accounts.insert(changes.address);
                    for (key, _) in changes.storage.changed_slots() {
                        touched.slots.insert((changes.address, *key));
                    }
                }
                let result = executed.commit();
                results.push(Ok(Included {
                    success: result.status,
                    gas_used: result.tx_gas_used(),
                    output: result.output.clone(),
                    logs: result.logs.clone(),
                    // Compared for included transactions only: the revm reference reports the
                    // count solely through its block-execution handoff.
                    committed_phases: result
                        .ext
                        .eip8130_committed_phases
                        .filter(|_| run.simulation.is_none()),
                }));
            }
            Err(error) => results.push(Err(evm2_rejection(error))),
        }
    }
    let state = evm.state_mut();
    let accounts = touched
        .accounts
        .iter()
        .map(|address| {
            let info = state.account_info_untracked(address).unwrap().unwrap_or_default();
            (*address, account_view(info.balance, info.nonce, info.code_hash))
        })
        .collect();
    let storage = touched
        .slots
        .iter()
        .map(|(address, key)| {
            ((*address, *key), state.storage_slot_untracked(address, key).unwrap())
        })
        .collect();
    Outcome { results, accounts, storage }
}

fn evm2_rejection(error: HandlerError) -> Rejection {
    match error {
        HandlerError::InsufficientFunds { .. } => Rejection::InsufficientFunds,
        error => match error.external_ref::<Eip8130Error>() {
            Some(Eip8130Error::Invalid(reason)) => Rejection::Invalid(reason.clone()),
            _ => panic!("unexpected evm2 error: {error}"),
        },
    }
}

fn run_revm(world: &World, runs: &[Run], touched: &mut Touched) -> Outcome {
    let mut evm = world.revm();
    let mut results = Vec::new();
    for run in runs {
        let (envelope, encoded) = run.encoded();
        let mut tx = BaseTransaction::from_encoded_tx(&envelope, run.signer, encoded);
        if let (Some(simulation), Some(parts)) = (run.simulation, tx.eip8130.as_mut()) {
            parts.mode = match simulation.mode {
                Eip8130SimulationMode::Call => Eip8130ExecutionMode::Call,
                Eip8130SimulationMode::Estimate => Eip8130ExecutionMode::Simulate,
            };
            parts.simulation_sender_actor_id = simulation.sender_actor_id;
        }
        match evm.transact_raw(tx) {
            Ok(outcome) => {
                let committed_phases = Eip8130CommittedPhases::take();
                for (address, account) in &outcome.state {
                    touched.accounts.insert(*address);
                    for key in account.storage.keys() {
                        touched.slots.insert((*address, *key));
                    }
                }
                let result = outcome.result;
                let output = match &result {
                    ExecutionResult::Success { output, .. } => output.data().clone(),
                    ExecutionResult::Revert { output, .. } => output.clone(),
                    ExecutionResult::Halt { .. } => Bytes::new(),
                };
                results.push(Ok(Included {
                    success: result.is_success(),
                    gas_used: result.tx_gas_used(),
                    output,
                    logs: result.logs().to_vec(),
                    // The revm executor reports the count through the thread-local handoff, and
                    // only for included (verified) transactions.
                    committed_phases: run.simulation.is_none().then_some(committed_phases),
                }));
                evm.db_mut().commit(outcome.state);
            }
            Err(EVMError::Transaction(BaseTransactionError::Base(
                InvalidTransaction::LackOfFundForMaxFee { .. },
            ))) => results.push(Err(Rejection::InsufficientFunds)),
            Err(EVMError::Transaction(BaseTransactionError::Eip8130(reason))) => {
                results.push(Err(Rejection::Invalid(reason)))
            }
            Err(error) => panic!("unexpected revm error: {error:?}"),
        }
    }
    let db = evm.db_mut();
    let accounts = touched
        .accounts
        .iter()
        .map(|address| {
            let info = db.basic(*address).unwrap().unwrap_or_default();
            (*address, account_view(info.balance, info.nonce, info.code_hash))
        })
        .collect();
    let storage = touched
        .slots
        .iter()
        .map(|(address, key)| ((*address, *key), db.storage(*address, *key).unwrap()))
        .collect();
    Outcome { results, accounts, storage }
}

/// Runs `runs` in order on both engines from `world` and asserts identical outcomes. Each engine
/// reads back the union of what either touched, so a write only one engine made is caught.
fn assert_parity(world: &World, runs: &[Run]) -> Outcome {
    let mut touched = Touched::default();
    run_evm2(world, runs, &mut touched);
    run_revm(world, runs, &mut touched);
    let evm2 = run_evm2(world, runs, &mut touched.clone());
    let revm = run_revm(world, runs, &mut touched);
    assert_eq!(evm2, revm, "evm2 and revm diverged");
    evm2
}

fn included(outcome: &Outcome, index: usize) -> &Included {
    outcome.results[index].as_ref().expect("transaction should be included")
}

#[test]
fn eoa_self_pay_single_call() {
    let key = signing_key(0x21);
    let sender = eoa_address(&key);
    let mut tx = base_tx();
    tx.calls = vec![vec![call(STORER, 0, &[0x2a; 32]), call(RECIPIENT, 12_345, &[])]];
    let outcome = assert_parity(
        &World::new(BaseUpgrade::Everest, &[sender]),
        &[Run::verified(eoa_signed(tx, &key), sender)],
    );
    let result = included(&outcome, 0);
    assert!(result.success);
    assert_eq!(result.committed_phases, Some(1));
    assert_eq!(result.logs.len(), 1);
    assert_eq!(outcome.storage[&(STORER, U256::ZERO)], U256::from_be_bytes([0x2a; 32]));
    assert_eq!(outcome.accounts[&RECIPIENT].0, U256::from(12_345));
    assert_eq!(outcome.accounts[&sender].1, 1, "the protocol nonce is bumped");
}

#[test]
fn sstore_refunds_are_capped_identically() {
    let key = signing_key(0x30);
    let sender = eoa_address(&key);
    // Clearing pre-set slots earns SSTORE refunds, capped at a fifth of the gross sender gas.
    let world = World::new(BaseUpgrade::Everest, &[sender]).with_storage([(
        STORER,
        U256::ZERO,
        U256::from(7u64),
    )]);
    let mut tx = base_tx();
    tx.calls = vec![vec![call(STORER, 0, &[0; 32])], vec![call(STORER, 0, &[1; 32])]];
    let outcome = assert_parity(&world, &[Run::verified(eoa_signed(tx, &key), sender)]);
    assert!(included(&outcome, 0).success);
}

#[test]
fn multi_phase_with_reverting_and_halting_phases() {
    let key = signing_key(0x22);
    let sender = eoa_address(&key);
    for failing in [REVERTER, HALTER] {
        let mut tx = base_tx();
        tx.calls = vec![
            vec![call(STORER, 0, &[0x01; 32])],
            vec![call(RECIPIENT, 1, &[]), call(failing, 0, &[])],
            vec![call(STORER, 0, &[0x02; 32])],
        ];
        let outcome = assert_parity(
            &World::new(BaseUpgrade::Everest, &[sender]),
            &[Run::verified(eoa_signed(tx, &key), sender)],
        );
        let result = included(&outcome, 0);
        assert!(!result.success, "a failing phase reverts the transaction");
        assert_eq!(result.committed_phases, Some(1), "only the first phase commits");
        assert_eq!(outcome.storage[&(STORER, U256::ZERO)], U256::from_be_bytes([0x01; 32]));
        assert_eq!(outcome.accounts[&RECIPIENT].0, U256::ZERO, "the failed phase rolled back");
    }
}

#[test]
fn payer_sponsored_transaction() {
    let key = signing_key(0x23);
    let sender = eoa_address(&key);
    let payer_key = signing_key(0x24);
    let payer = eoa_address(&payer_key);
    let tx = TxEip8130 {
        payer: Some(Eip8130Constants::OPEN_PAYER),
        calls: vec![vec![call(STORER, 0, &[0x03; 32])]],
        ..base_tx()
    };
    let sender_auth = eoa_sig(&key, tx.sender_signature_hash());
    let payer_auth = eoa_sig(&payer_key, tx.payer_signature_hash(sender));
    let signed = Eip8130Signed::new(tx, sender_auth, payer_auth);
    let world = World::new(BaseUpgrade::Everest, &[payer]).with_balance(sender, U256::ZERO);
    let outcome = assert_parity(&world, &[Run::verified(signed, sender)]);
    assert!(included(&outcome, 0).success);
    assert!(outcome.accounts[&payer].0 < U256::from(RICH), "the payer is charged");
    assert_eq!(outcome.accounts[&sender].0, U256::ZERO, "the sender is not charged");
}

#[test]
fn counterfactual_create_and_call() {
    let key = signing_key(0x25);
    let owner = eoa_address(&key);
    let mut actor_id = [0u8; 32];
    actor_id[12..].copy_from_slice(owner.as_slice());
    let initial_actors =
        vec![InitialActor::owner(B256::from(actor_id), Eip8130Constants::K1_AUTHENTICATOR)];
    let create = CreateEntry { user_salt: B256::ZERO, code: storer_code(), initial_actors };
    let account = AccountChangeApplier::compute_address(
        create.user_salt,
        &create.code,
        &create.initial_actors,
    )
    .unwrap();
    let mut tx = base_tx();
    tx.sender = Some(account);
    tx.account_changes = vec![AccountChange::Create(create)];
    tx.calls = vec![vec![call(account, 0, &[0x04; 32])]];
    // Account creation needs the Keystore (Zenith).
    let world = World::new(BaseUpgrade::Zenith, &[]).with_balance(account, U256::from(RICH));
    let outcome = assert_parity(&world, &[Run::verified(configured_signed(tx, &key), account)]);
    assert!(included(&outcome, 0).success);
    assert_eq!(outcome.accounts[&account].2, keccak256(storer_code()), "the code is installed");
    assert_eq!(outcome.storage[&(account, U256::ZERO)], U256::from_be_bytes([0x04; 32]));
}

#[test]
fn delegation_then_call_runs_the_delegate() {
    let key = signing_key(0x26);
    let sender = eoa_address(&key);
    let mut tx = base_tx();
    tx.account_changes = vec![AccountChange::Delegation(Delegation { target: STORER })];
    tx.calls = vec![vec![call(sender, 0, &[0x05; 32])]];
    let outcome = assert_parity(
        &World::new(BaseUpgrade::Everest, &[sender]),
        &[Run::verified(eoa_signed(tx, &key), sender)],
    );
    assert!(included(&outcome, 0).success);
    assert_eq!(outcome.storage[&(sender, U256::ZERO)], U256::from_be_bytes([0x05; 32]));

    // Delegating over ordinary contract code is rejected identically.
    let mut tx = base_tx();
    tx.account_changes = vec![AccountChange::Delegation(Delegation { target: STORER })];
    let world = World::new(BaseUpgrade::Everest, &[]).with_code(sender, storer_code());
    let outcome = assert_parity(&world, &[Run::verified(eoa_signed(tx, &key), sender)]);
    assert!(matches!(outcome.results[0], Err(Rejection::Invalid(_))));
}

sol! {
    struct ActorConfigAbi {
        address authenticator;
        uint48 expiry;
        uint16 scope;
    }
}

#[test]
fn config_change_authorizes_an_actor_at_zenith() {
    let owner_key = signing_key(0x27);
    let owner = eoa_address(&owner_key);
    let session = eoa_address(&signing_key(0x28));
    let session_actor = AccountConfigurationStorage::self_actor_id(session);
    let config = ActorConfigAbi {
        authenticator: Eip8130Constants::K1_AUTHENTICATOR,
        expiry: alloy_primitives::aliases::U48::from(0u64),
        scope: Eip8130Constants::SCOPE_SELF_PAYER,
    };
    let payload = Bytes::from((session_actor, config, Bytes::new()).abi_encode_params());
    let mut change = SignedAccountChanges {
        channel: AccountChangeChannel::Local,
        sequence: 0,
        changes: vec![SignedChange { change_type: ChangeType::AuthorizeActor, payload }],
        signature: Bytes::new(),
    };
    let digest = ConfigChangeAuthorizer::changes_digest(owner, CHAIN_ID, &change);
    let mut signature = Eip8130Constants::K1_AUTHENTICATOR.to_vec();
    signature.extend_from_slice(&eoa_sig(&owner_key, digest));
    change.signature = Bytes::from(signature);
    let mut tx = base_tx();
    tx.sender = Some(owner);
    tx.account_changes = vec![AccountChange::ConfigChange(change)];
    tx.calls = vec![vec![call(STORER, 0, &[0x06; 32])]];
    let signed = configured_signed(tx, &owner_key);
    let world = World::new(BaseUpgrade::Zenith, &[owner]);
    let outcome = assert_parity(&world, &[Run::verified(signed.clone(), owner)]);
    let result = included(&outcome, 0);
    assert!(result.success);
    assert!(result.logs.iter().any(|log| log.address != STORER), "ActorAuthorized is logged");
    assert!(outcome.storage.values().any(|value| !value.is_zero()), "the actor is configured");
    // The same change through simulation applies and commits nothing.
    assert_parity(&world, &[Run::simulated(signed, owner, Eip8130SimulationMode::Call)]);
}

#[test]
fn policy_gate_blocks_unauthorized_targets() {
    let signer = signing_key(0x29);
    let signer_addr = eoa_address(&signer);
    let account = address!("0x00000000000000000000000000000000000000a5");
    let actor_id = AccountConfigurationStorage::self_actor_id(signer_addr);
    let storage = seeded_storage(|sctx| {
        let mut acc = AccountConfigurationStorage::new(sctx);
        let scope = Eip8130Constants::SCOPE_POLICY
            | Eip8130Constants::SCOPE_SELF_PAYER
            | Eip8130Constants::SCOPE_NONCE;
        let packed = U256::from_be_slice(Eip8130Constants::K1_AUTHENTICATOR.as_slice())
            | (U256::from(scope) << 208);
        acc.actors.at_mut(&actor_id).at_mut(&account).write(packed).unwrap();
        acc.set_policy(account, actor_id, STORER, B256::ZERO).unwrap();
    });
    let world = World::new(BaseUpgrade::Zenith, &[account]).with_storage(storage);
    for (target, allowed) in [(STORER, true), (REVERTER, false)] {
        let mut tx = base_tx();
        tx.sender = Some(account);
        tx.calls = vec![vec![call(target, 0, &[0x07; 32])]];
        let outcome =
            assert_parity(&world, &[Run::verified(configured_signed(tx, &signer), account)]);
        assert_eq!(included(&outcome, 0).success, allowed, "policy gate for {target}");
    }
}

#[test]
fn two_dimensional_nonce_advances_its_channel() {
    let key = signing_key(0x2a);
    let sender = eoa_address(&key);
    let runs: Vec<_> = (0..2)
        .map(|sequence| {
            let mut tx = base_tx();
            tx.nonce_key = U256::from(5u64);
            tx.nonce_sequence = sequence;
            tx.calls = vec![vec![call(STORER, 0, &[0x08; 32])]];
            Run::verified(eoa_signed(tx, &key), sender)
        })
        .collect();
    // Replaying the first sequence after both landed is rejected.
    let replay = runs[0].clone();
    let outcome = assert_parity(
        &World::new(BaseUpgrade::Everest, &[sender]),
        &[runs[0].clone(), runs[1].clone(), replay],
    );
    assert!(included(&outcome, 1).success);
    assert!(matches!(outcome.results[2], Err(Rejection::Invalid(_))));
    assert_eq!(outcome.accounts[&sender].1, 0, "channel nonces leave the protocol nonce alone");
}

#[test]
fn nonce_free_replay_and_validity_window() {
    const MILLIS_PART: u64 = 250;
    let now_ms = BLOCK_SECONDS * 1_000 + MILLIS_PART;
    let key = signing_key(0x2b);
    let sender = eoa_address(&key);
    let world = World::new(BaseUpgrade::Everest, &[sender]).with_storage([(
        base_common_consensus::Predeploys::BASE_TIME,
        U256::ZERO,
        U256::from(MILLIS_PART),
    )]);
    let signed = |nonce_free: bool, valid_after: u64, valid_before: u64| {
        let mut tx = base_tx();
        if nonce_free {
            tx.nonce_key = Eip8130Constants::NONCE_KEY_MAX;
        }
        tx.valid_after = valid_after;
        tx.valid_before = valid_before;
        tx.calls = vec![vec![call(STORER, 0, &[0x09; 32])]];
        Run::verified(eoa_signed(tx, &key), sender)
    };

    // A nonce-free transaction lands once; its replay is rejected by the ring.
    let nonce_free = signed(true, 0, now_ms + 20_000);
    let outcome = assert_parity(&world, &[nonce_free.clone(), nonce_free]);
    assert!(included(&outcome, 0).success);
    assert!(matches!(outcome.results[1], Err(Rejection::Invalid(_))));

    // The inclusive millisecond window, for both nonce modes.
    for nonce_free in [false, true] {
        let outcome = assert_parity(
            &world,
            &[
                signed(nonce_free, now_ms + 1, now_ms + 20_000),
                signed(nonce_free, 0, now_ms - 1),
                signed(nonce_free, now_ms, now_ms + 20_000),
            ],
        );
        assert!(matches!(outcome.results[0], Err(Rejection::Invalid(_))), "not yet valid");
        assert!(matches!(outcome.results[1], Err(Rejection::Invalid(_))), "expired");
        assert!(included(&outcome, 2).success, "valid at the lower bound");
    }
}

#[test]
fn insufficient_payer_funds_are_rejected() {
    let key = signing_key(0x2c);
    let sender = eoa_address(&key);
    let mut tx = base_tx();
    tx.calls = vec![vec![call(STORER, 0, &[0x0a; 32])]];
    let world = World::new(BaseUpgrade::Everest, &[]).with_balance(sender, U256::from(1_000u64));
    let outcome = assert_parity(&world, &[Run::verified(eoa_signed(tx, &key), sender)]);
    assert_eq!(outcome.results[0], Err(Rejection::InsufficientFunds));
    assert!(outcome.accounts.is_empty(), "a rejected transaction writes nothing");
}

#[test]
fn invalid_authentication_and_inactive_fork_are_rejected() {
    let key = signing_key(0x2d);
    let sender = eoa_address(&key);
    let mut tx = base_tx();
    tx.calls = vec![vec![call(STORER, 0, &[0x0b; 32])]];
    let signed = eoa_signed(tx.clone(), &key);
    let truncated = signed.sender_auth().slice(..64);
    let signed = Eip8130Signed::new(signed.tx().clone(), truncated, Bytes::new());
    let world = World::new(BaseUpgrade::Everest, &[sender]);
    let outcome = assert_parity(&world, &[Run::verified(signed, sender)]);
    assert!(matches!(outcome.results[0], Err(Rejection::Invalid(_))));

    let mut foreign = tx.clone();
    foreign.chain_id = CHAIN_ID + 1;
    let outcome = assert_parity(&world, &[Run::verified(eoa_signed(foreign, &key), sender)]);
    assert_eq!(outcome.results[0], Err(Rejection::Invalid("chain id mismatch".into())));

    let pre_everest = World::new(BaseUpgrade::Denim, &[sender]);
    let outcome = assert_parity(&pre_everest, &[Run::verified(eoa_signed(tx, &key), sender)]);
    assert!(matches!(outcome.results[0], Err(Rejection::Invalid(_))));
}

#[test]
fn simulation_measures_gas_and_commits_nothing() {
    let key = signing_key(0x2e);
    let sender = eoa_address(&key);
    let world = World::new(BaseUpgrade::Everest, &[sender]);
    for calls in [
        vec![vec![call(STORER, 0, &[0x0c; 32])]],
        vec![vec![call(FORWARDER, 0, &[])]],
        vec![vec![call(STORER, 0, &[0x0d; 32])], vec![call(REVERTER, 0, &[])]],
    ] {
        let mut tx = base_tx();
        tx.calls = calls;
        // Simulation never verifies the signature.
        let signed = Eip8130Signed::new(tx, Bytes::from(vec![0u8; 65]), Bytes::new());
        for mode in [Eip8130SimulationMode::Call, Eip8130SimulationMode::Estimate] {
            let outcome = assert_parity(&world, &[Run::simulated(signed.clone(), sender, mode)]);
            assert!(included(&outcome, 0).gas_used > 0);
            assert_eq!(
                outcome.accounts.get(&sender).map(|account| (account.0, account.1)),
                Some((U256::from(RICH), 0))
            );
            assert!(outcome.storage.values().all(U256::is_zero), "no simulated write persists");
        }
    }
}

/// The receipt fields consensus commits to.
#[derive(Debug, PartialEq, Eq)]
struct ReceiptView {
    ty: u8,
    status: bool,
    cumulative_gas_used: u64,
    logs: Vec<Log>,
    committed_phases: Option<u64>,
    deposit_nonce: Option<u64>,
}

impl From<&BaseReceipt> for ReceiptView {
    fn from(receipt: &BaseReceipt) -> Self {
        let (committed_phases, deposit_nonce) = match receipt {
            BaseReceipt::Eip8130(receipt) => (Some(receipt.committed_phases), None),
            BaseReceipt::Deposit(receipt) => (None, receipt.deposit_nonce),
            _ => (None, None),
        };
        Self {
            ty: receipt.ty(),
            status: receipt.status(),
            cumulative_gas_used: receipt.cumulative_gas_used(),
            logs: receipt.logs().to_vec(),
            committed_phases,
            deposit_nonce,
        }
    }
}

impl From<&BaseReceiptEnvelope> for ReceiptView {
    fn from(receipt: &BaseReceiptEnvelope) -> Self {
        let (committed_phases, deposit_nonce) = match receipt {
            BaseReceiptEnvelope::Eip8130(receipt) => (Some(receipt.receipt.committed_phases), None),
            BaseReceiptEnvelope::Deposit(receipt) => (None, receipt.receipt.deposit_nonce),
            _ => (None, None),
        };
        Self {
            ty: receipt.ty(),
            status: receipt.status(),
            cumulative_gas_used: receipt.cumulative_gas_used(),
            logs: receipt.logs().to_vec(),
            committed_phases,
            deposit_nonce,
        }
    }
}

/// `L1Block` predeploy storage: base fee, Ecotone scalars, blob base fee, and the packed Isthmus
/// operator fee and Jovian DA-footprint scalar.
fn l1_block_storage() -> [(Address, U256, U256); 4] {
    let mut scalars = [0u8; 32];
    scalars[16..20].copy_from_slice(&1_000u32.to_be_bytes());
    scalars[20..24].copy_from_slice(&100u32.to_be_bytes());
    let mut operator = [0u8; 32];
    operator[18..20].copy_from_slice(&400u16.to_be_bytes());
    operator[20..24].copy_from_slice(&2_000u32.to_be_bytes());
    operator[24..32].copy_from_slice(&7u64.to_be_bytes());
    let l1_block = base_common_consensus::Predeploys::L1_BLOCK_INFO;
    [
        (l1_block, L1BlockInfo::L1_BASE_FEE_SLOT, U256::from(1_000_000u64)),
        (l1_block, L1BlockInfo::ECOTONE_L1_FEE_SCALARS_SLOT, U256::from_be_bytes(scalars)),
        (l1_block, L1BlockInfo::ECOTONE_L1_BLOB_BASE_FEE_SLOT, U256::from(10u64)),
        (l1_block, L1BlockInfo::OPERATOR_FEE_SCALARS_SLOT, U256::from_be_bytes(operator)),
    ]
}

#[test]
fn block_mixing_deposit_standard_and_eip8130_matches_revm() {
    let key = signing_key(0x2f);
    let sender = eoa_address(&key);
    let depositor = Address::repeat_byte(0x44);
    // Both engines load the block's L1 fee parameters (including the Jovian DA-footprint scalar)
    // from the `L1Block` predeploy, as block execution does.
    let world = World::new(BaseUpgrade::Everest, &[sender]).with_storage(l1_block_storage());

    let deposit = TxDeposit {
        source_hash: B256::repeat_byte(0xab),
        from: depositor,
        to: TxKind::Call(RECIPIENT),
        mint: 1_000_000,
        value: U256::from(1_000u64),
        gas_limit: 100_000,
        ..Default::default()
    };
    let standard = TxEip1559 {
        chain_id: CHAIN_ID,
        nonce: 0,
        gas_limit: 100_000,
        max_fee_per_gas: 5_000_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
        to: TxKind::Call(STORER),
        input: Bytes::from_static(&[0x0e; 32]),
        ..Default::default()
    };
    let phased = |sequence: u64, failing: Address| {
        let mut tx = base_tx();
        tx.nonce_key = U256::from(3u64);
        tx.nonce_sequence = sequence;
        tx.calls = vec![vec![call(STORER, 0, &[0x0f; 32])], vec![call(failing, 0, &[])]];
        ConsensusTx::Eip8130(eoa_signed(tx, &key))
    };
    let txs = [
        Recovered::new_unchecked(ConsensusTx::Deposit(Sealed::new(deposit)), depositor),
        Recovered::new_unchecked(
            ConsensusTx::Eip1559(standard.into_signed(Signature::test_signature())),
            sender,
        ),
        Recovered::new_unchecked(phased(0, RECIPIENT), sender),
        Recovered::new_unchecked(phased(1, REVERTER), sender),
    ];

    let mut evm2_evm = world.evm2();
    evm2_evm.ext_mut().l1_fee_params_block = None;
    let mut evm2 = BaseBlockExecutor::new(evm2_evm, BaseBlockExecutionCtx::default());
    for tx in &txs {
        let encoded = tx.inner().encoded_2718().into();
        let envelope = BaseTxEnvelope::from_signed(tx.inner().clone(), encoded);
        evm2.execute_transaction(&Recovered::new_unchecked(envelope, tx.signer()))
            .expect("evm2 executes the transaction");
    }
    let (mut evm2_evm, evm2_result, _) = evm2.finish();

    let upgrades = ChainUpgrades::new(
        BaseUpgrade::EXECUTION_VARIANTS
            .into_iter()
            .filter(|upgrade| *upgrade <= BaseUpgrade::Everest)
            .map(|upgrade| (upgrade, ForkCondition::Timestamp(0))),
    );
    let mut db = State::builder().with_database(world.revm_db()).build();
    let revm_evm = Context::base()
        .with_db(&mut db)
        .with_chain(L1BlockInfo::default())
        .with_block(World::revm_block())
        .with_cfg(world.revm_cfg())
        .build_with_inspector(NoOpInspector);
    let receipt_builder = AlloyReceiptBuilder::default();
    let mut revm = RevmBlockExecutor::new(
        revm_evm,
        RevmBlockExecutionCtx::default(),
        &upgrades,
        &receipt_builder,
    );
    for tx in &txs {
        revm.execute_transaction(tx).expect("revm executes the transaction");
    }
    let (_, revm_result) = revm.finish().expect("revm finishes the block");
    let revm_receipts: Vec<ReceiptView> =
        revm_result.receipts.iter().map(ReceiptView::from).collect();

    let evm2_receipts: Vec<ReceiptView> =
        evm2_result.receipts.iter().map(ReceiptView::from).collect();
    assert_eq!(evm2_receipts, revm_receipts, "block receipts diverged");
    assert_eq!(evm2_result.gas_used, revm_result.gas_used);
    assert_eq!(evm2_result.blob_gas_used, revm_result.blob_gas_used, "DA footprint diverged");
    assert!(evm2_result.blob_gas_used > 0, "the Jovian DA footprint counts EIP-8130 bytes");
    assert_eq!(evm2_receipts[2].committed_phases, Some(2));
    assert_eq!(evm2_receipts[3].committed_phases, Some(1));
    assert!(!evm2_receipts[3].status);

    for address in [sender, depositor, RECIPIENT, STORER, BENEFICIARY, NonceManagerStorage::ADDRESS]
        .into_iter()
        .chain([
            base_common_consensus::Predeploys::BASE_FEE_VAULT,
            base_common_consensus::Predeploys::L1_FEE_VAULT,
            base_common_consensus::Predeploys::OPERATOR_FEE_VAULT,
        ])
    {
        let evm2_info =
            evm2_evm.state_mut().account_info_untracked(&address).unwrap().unwrap_or_default();
        let revm_info = db.basic(address).unwrap().unwrap_or_default();
        assert_eq!(
            account_view(evm2_info.balance, evm2_info.nonce, evm2_info.code_hash),
            account_view(revm_info.balance, revm_info.nonce, revm_info.code_hash),
            "post-block state of {address} diverged",
        );
    }
    let slot = evm2_evm.state_mut().storage_slot_untracked(&STORER, &U256::ZERO).unwrap();
    assert_eq!(slot, db.storage(STORER, U256::ZERO).unwrap());
}
