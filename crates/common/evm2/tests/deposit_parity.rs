//! Differential parity harness for deposit execution.
//!
//! Runs the same OP-stack deposit through the `base-common-evm2` handler and the revm-based
//! `base-common-evm` reference, then asserts the two engines agree on the observable outcome:
//! success, gas used, the created contract address, and the post-state (balance, nonce,
//! code hash, and storage) of every account the fixture cares about.
//!
//! Both engines run at the same fork — evm2 [`SpecId::MERGE`] and revm
//! [`BaseUpgrade::Regolith`] (which maps to `MERGE`) — so gas is comparable, and both take the
//! same `base_common_consensus::TxDeposit` fields (shared thanks to the type dedup).

use std::collections::BTreeMap;

use alloy_consensus::transaction::Recovered;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, keccak256};
// revm reference side.
use base_common_evm::{BaseSpecId, BaseTransaction, Builder, DefaultBase, L1BlockInfo};
// evm2 side.
use base_common_evm2::{BaseEvmTypes, BaseTxEnvelope, TxDeposit};
use base_common_genesis::BaseUpgrade;
use evm2::{
    Evm, Precompiles, SpecId, bytecode::Bytecode as Evm2Bytecode, env::BlockEnv, evm::InMemoryDB,
};
use revm::{
    ExecuteEvm,
    bytecode::Bytecode as RevmBytecode,
    context::{CfgEnv, Context, TxEnv},
    context_interface::result::{ExecutionResult, Output, ResultAndState},
    database::InMemoryDB as RevmDb,
    primitives::TxKind as RevmTxKind,
    state::AccountInfo as RevmAccountInfo,
};

const SENDER: Address = Address::repeat_byte(0x11);
const SOURCE_HASH: B256 = B256::repeat_byte(0xab);

/// An account to seed into both engines before execution.
#[derive(Clone)]
struct Seed {
    address: Address,
    balance: U256,
    nonce: u64,
    code: Vec<u8>,
}

/// A deposit scenario plus the accounts/slots whose post-state to compare.
struct Fixture {
    seed: Vec<Seed>,
    to: TxKind,
    mint: u128,
    value: U256,
    gas_limit: u64,
    input: Vec<u8>,
    is_system: bool,
    /// Addresses (with the storage slots) whose post-state is compared across engines.
    compare: Vec<(Address, Vec<U256>)>,
}

/// The normalized post-state of a single account.
#[derive(Debug, PartialEq, Eq)]
struct AccountState {
    balance: U256,
    nonce: u64,
    code_hash: B256,
    storage: BTreeMap<U256, U256>,
}

/// The normalized observable outcome of executing a deposit.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    success: bool,
    gas_used: u64,
    created: Option<Address>,
    accounts: BTreeMap<Address, AccountState>,
}

/// Normalizes an empty/zero code hash to the canonical empty-code keccak so the two engines'
/// representations of a code-less account compare equal.
fn norm_code_hash(hash: B256) -> B256 {
    if hash == B256::ZERO { keccak256([]) } else { hash }
}

/// Executes `fixture` through the evm2 deposit handler and captures its outcome.
fn run_evm2(fixture: &Fixture) -> Outcome {
    let mut db = InMemoryDB::default();
    for seed in &fixture.seed {
        let mut info =
            evm2::AccountInfo { balance: seed.balance, nonce: seed.nonce, ..Default::default() };
        if !seed.code.is_empty() {
            info.code = Some(Evm2Bytecode::new_legacy(Bytes::from(seed.code.clone())));
        }
        db.insert_account_info(&seed.address, info);
    }
    let mut evm = Evm::new(
        SpecId::MERGE,
        BlockEnv::<BaseEvmTypes>::default(),
        BaseEvmTypes::tx_registry(),
        db,
        Precompiles::base(SpecId::MERGE),
    );

    let tx = TxDeposit {
        source_hash: SOURCE_HASH,
        from: SENDER,
        to: fixture.to,
        mint: fixture.mint,
        value: fixture.value,
        gas_limit: fixture.gas_limit,
        is_system_transaction: fixture.is_system,
        input: Bytes::from(fixture.input.clone()),
    };
    let recovered = Recovered::new_unchecked(BaseTxEnvelope::Deposit(tx), SENDER);
    let result = evm.transact(&recovered).expect("deposit must not fatally error").commit();

    let mut accounts = BTreeMap::new();
    for (address, slots) in &fixture.compare {
        let info = evm.state_mut().account_info_untracked(address).unwrap().unwrap_or_default();
        let mut storage = BTreeMap::new();
        for slot in slots {
            let value = evm.state_mut().storage_slot(address, *slot, false).unwrap().current();
            storage.insert(*slot, value);
        }
        accounts.insert(
            *address,
            AccountState {
                balance: info.balance,
                nonce: info.nonce,
                code_hash: norm_code_hash(info.code_hash),
                storage,
            },
        );
    }

    Outcome {
        success: result.status,
        gas_used: result.tx_gas_used(),
        created: result.created_address,
        accounts,
    }
}

/// Executes `fixture` through the revm-based `base-common-evm` reference and captures its outcome.
fn run_revm(fixture: &Fixture) -> Outcome {
    let mut db = RevmDb::default();
    for seed in &fixture.seed {
        let mut info =
            RevmAccountInfo { balance: seed.balance, nonce: seed.nonce, ..Default::default() };
        if !seed.code.is_empty() {
            info.code = Some(RevmBytecode::new_legacy(Bytes::from(seed.code.clone())));
        }
        db.insert_account_info(seed.address, info);
    }
    let ctx = Context::base()
        .with_db(db)
        .with_chain(L1BlockInfo {
            l2_block: Some(U256::ZERO),
            operator_fee_scalar: Some(U256::ZERO),
            operator_fee_constant: Some(U256::ZERO),
            ..Default::default()
        })
        .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Regolith)));
    let mut evm = ctx.build_base();

    let kind = match fixture.to {
        TxKind::Call(address) => RevmTxKind::Call(address),
        TxKind::Create => RevmTxKind::Create,
    };
    let mut builder = BaseTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(SENDER)
                .kind(kind)
                .gas_limit(fixture.gas_limit)
                .value(fixture.value)
                .data(Bytes::from(fixture.input.clone())),
        )
        .source_hash(SOURCE_HASH)
        .mint(fixture.mint);
    if fixture.is_system {
        builder = builder.is_system_transaction();
    }
    let tx = builder.build_fill();

    let ResultAndState { result, state } = evm.transact(tx).unwrap();

    let created = match &result {
        ExecutionResult::Success { output: Output::Create(_, address), .. } => *address,
        _ => None,
    };
    let mut accounts = BTreeMap::new();
    for (address, slots) in &fixture.compare {
        let account = state.get(address);
        let mut storage = BTreeMap::new();
        for slot in slots {
            let value = account
                .and_then(|a| a.storage.get(slot))
                .map(|s| s.present_value())
                .unwrap_or_default();
            storage.insert(*slot, value);
        }
        accounts.insert(
            *address,
            AccountState {
                balance: account.map(|a| a.info.balance).unwrap_or_default(),
                nonce: account.map(|a| a.info.nonce).unwrap_or_default(),
                code_hash: norm_code_hash(account.map(|a| a.info.code_hash).unwrap_or_default()),
                storage,
            },
        );
    }

    Outcome { success: result.is_success(), gas_used: result.tx_gas_used(), created, accounts }
}

/// Asserts the two engines produce identical outcomes for `fixture`.
fn assert_parity(fixture: Fixture) {
    let evm2 = run_evm2(&fixture);
    let revm = run_revm(&fixture);
    assert_eq!(evm2, revm, "evm2 and revm deposit outcomes diverged");
}

#[test]
fn parity_mint_only_to_eoa() {
    assert_parity(Fixture {
        seed: vec![],
        to: TxKind::Call(Address::repeat_byte(0x22)),
        mint: 1_000,
        value: U256::ZERO,
        gas_limit: 100_000,
        input: vec![],
        is_system: false,
        compare: vec![(SENDER, vec![]), (Address::repeat_byte(0x22), vec![])],
    });
}

#[test]
fn parity_mint_and_value_transfer() {
    assert_parity(Fixture {
        seed: vec![],
        to: TxKind::Call(Address::repeat_byte(0x22)),
        mint: 1_000,
        value: U256::from(300),
        gas_limit: 100_000,
        input: vec![],
        is_system: false,
        compare: vec![(SENDER, vec![]), (Address::repeat_byte(0x22), vec![])],
    });
}

#[test]
fn parity_call_contract_with_sstore() {
    // Runtime code: PUSH1 0x2a, PUSH1 0x00, SSTORE, STOP  (stores 42 at slot 0).
    let target = Address::repeat_byte(0x33);
    assert_parity(Fixture {
        seed: vec![Seed {
            address: target,
            balance: U256::ZERO,
            nonce: 1,
            code: vec![0x60, 0x2a, 0x60, 0x00, 0x55, 0x00],
        }],
        to: TxKind::Call(target),
        mint: 1_000,
        value: U256::from(50),
        gas_limit: 200_000,
        input: vec![],
        is_system: false,
        compare: vec![(SENDER, vec![]), (target, vec![U256::ZERO])],
    });
}

#[test]
fn parity_create_deposit() {
    // Init code: PUSH1 0x00, PUSH1 0x00, RETURN  (deploys empty runtime code, succeeds).
    assert_parity(Fixture {
        seed: vec![],
        to: TxKind::Create,
        mint: 1_000,
        value: U256::ZERO,
        gas_limit: 200_000,
        input: vec![0x60, 0x00, 0x60, 0x00, 0xf3],
        is_system: false,
        compare: vec![(SENDER, vec![]), (SENDER.create(0), vec![])],
    });
}

#[test]
fn parity_reverted_deposit() {
    // Init code: PUSH1 0x00, PUSH1 0x00, REVERT.
    assert_parity(Fixture {
        seed: vec![],
        to: TxKind::Create,
        mint: 1_000,
        value: U256::ZERO,
        gas_limit: 200_000,
        input: vec![0x60, 0x00, 0x60, 0x00, 0xfd],
        is_system: false,
        compare: vec![(SENDER, vec![])],
    });
}

#[test]
fn parity_halted_deposit() {
    // Init code: INVALID.
    assert_parity(Fixture {
        seed: vec![],
        to: TxKind::Create,
        mint: 1_000,
        value: U256::ZERO,
        gas_limit: 200_000,
        input: vec![0xfe],
        is_system: false,
        compare: vec![(SENDER, vec![])],
    });
}

#[test]
fn parity_system_transaction_rejected() {
    assert_parity(Fixture {
        seed: vec![],
        to: TxKind::Call(Address::repeat_byte(0x22)),
        mint: 1_000,
        value: U256::ZERO,
        gas_limit: 100_000,
        input: vec![],
        is_system: true,
        compare: vec![(SENDER, vec![])],
    });
}
