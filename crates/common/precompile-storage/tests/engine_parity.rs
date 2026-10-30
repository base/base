//! Engine parity: the same native precompile body produces byte-identical results, gas, refunds,
//! state gas, logs, and storage on the revm and evm2 storage providers.

use alloy_evm::{EvmInternals, eth::EthEvmContext, precompiles::PrecompileInput};
use alloy_primitives::{Address, B256, Bytes, Log, LogData, U256, address, b256};
use base_precompile_storage::{
    Bytecode, BytesLikeHandler, Evm2JournalStorageProvider, Evm2PrecompileStorageProvider,
    Evm2TxOrigin, EvmPrecompileStorageProvider, Handler, JournalStorageProvider, PrecompileResult,
    StorageCtx, StorageFeatures,
};
use evm2::{
    BaseEvmConfigSelector, Evm, EvmTypesHost, SpecId as Evm2SpecId, Version,
    env::BlockEnvExt,
    ethereum::TxEnvelope,
    evm::{
        AccountInfo as Evm2AccountInfo, InMemoryDB as Evm2InMemoryDB, precompile::NoPrecompiles,
    },
    interpreter::{MessageExt, MessageKind},
    registry::TxRegistry,
};
use revm::{
    context_interface::cfg::GasParams as RevmGasParams,
    database::InMemoryDB as RevmInMemoryDB,
    primitives::hardfork::SpecId as RevmSpecId,
    state::{AccountInfo as RevmAccountInfo, Bytecode as RevmBytecode},
};

const PRECOMPILE: Address = address!("0x00000000000000000000000000000000000b2000");
const OTHER: Address = address!("0x00000000000000000000000000000000000b2001");
const CALLER: Address = address!("0x00000000000000000000000000000000000ca11e");
const ORIGIN: Address = address!("0x000000000000000000000000000000000000041d");
const BENEFICIARY: Address = address!("0x000000000000000000000000000000000000beef");
const TOPIC: B256 = b256!("0x1111111111111111111111111111111111111111111111111111111111111111");

/// evm2 type family whose instance extension supplies `tx.origin`.
struct TestTypes;

impl EvmTypesHost for TestTypes {
    type ConfigSelector = BaseEvmConfigSelector;
    type SpecId = Evm2SpecId;
    type Tx = TxEnvelope;
    type EvmExt = TxOrigin;
    type MessageExt = ();
    type MessageResultExt = ();
    type TxEnvExt = ();
    type TxResultExt = ();
    type BlockEnvExt = ();
    type Host<'a> = Evm<'a, Self>;
}

#[derive(Default)]
struct TxOrigin(Address);

impl Evm2TxOrigin for TxOrigin {
    fn tx_origin(&self) -> Address {
        self.0
    }
}

/// Pre-existing account state shared by both engines.
#[derive(Clone, Default)]
struct Account {
    address: Address,
    balance: U256,
    code: Option<Bytes>,
    storage: Vec<(U256, U256)>,
}

/// One precompile call: pre-state, call frame, and the slots to compare afterwards.
#[derive(Clone)]
struct Call {
    accounts: Vec<Account>,
    gas_limit: u64,
    is_static: bool,
    storage_features: StorageFeatures,
    observed_slots: Vec<(Address, U256)>,
}

impl Default for Call {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            gas_limit: 1_000_000,
            is_static: false,
            storage_features: StorageFeatures::Legacy,
            observed_slots: Vec::new(),
        }
    }
}

/// Everything an engine exposes after a precompile call.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    result: PrecompileResult,
    logs: Vec<Log>,
    storage: Vec<U256>,
}

type Body = fn(StorageCtx<'_>) -> PrecompileResult;

/// Which storage provider a body runs on: the metered precompile provider or the gas-free
/// journal provider of the enshrined EIP-8130 pipeline.
#[derive(Clone, Copy)]
enum Provider {
    Metered,
    GasFree,
}

fn run_revm(call: &Call, body: Body) -> Outcome {
    run_revm_with(call, body, Provider::Metered)
}

fn run_revm_with(call: &Call, body: Body, provider: Provider) -> Outcome {
    let mut db = RevmInMemoryDB::default();
    for account in &call.accounts {
        let mut info = RevmAccountInfo { balance: account.balance, ..Default::default() };
        if let Some(code) = &account.code {
            let code = RevmBytecode::new_raw(code.clone());
            info.code_hash = code.hash_slow();
            info.code = Some(code);
        }
        db.insert_account_info(account.address, info);
        for (key, value) in &account.storage {
            db.insert_account_storage(account.address, *key, *value).unwrap();
        }
    }
    let mut ctx = EthEvmContext::new(db, RevmSpecId::OSAKA);
    ctx.block.number = U256::from(7u64);
    ctx.block.timestamp = U256::from(1_700_000_000u64);
    ctx.block.beneficiary = BENEFICIARY;
    ctx.tx.caller = ORIGIN;

    let result = if let Provider::GasFree = provider {
        let mut provider =
            JournalStorageProvider::new(EvmInternals::from_context(&mut ctx), CALLER);
        StorageCtx::enter(&mut provider, body)
    } else {
        let input = PrecompileInput {
            data: &[],
            gas: call.gas_limit,
            reservoir: 0,
            caller: CALLER,
            value: U256::ZERO,
            target_address: PRECOMPILE,
            is_static: call.is_static,
            bytecode_address: PRECOMPILE,
            internals: EvmInternals::from_context(&mut ctx),
        };
        let mut provider = EvmPrecompileStorageProvider::new_with_storage_features(
            input,
            RevmGasParams::new_spec(RevmSpecId::OSAKA),
            call.storage_features,
        );
        StorageCtx::enter(&mut provider, body)
    };

    let state = &ctx.journaled_state.inner.state;
    let storage = call
        .observed_slots
        .iter()
        .map(|(address, key)| {
            state
                .get(address)
                .and_then(|account| account.storage.get(key))
                .map(|slot| slot.present_value)
                .unwrap_or_default()
        })
        .collect();
    Outcome { result, logs: ctx.journaled_state.inner.logs.clone(), storage }
}

fn run_evm2(call: &Call, body: Body) -> Outcome {
    run_evm2_with(call, body, Provider::Metered)
}

fn run_evm2_with(call: &Call, body: Body, provider: Provider) -> Outcome {
    let mut db = Evm2InMemoryDB::default();
    for account in &call.accounts {
        let mut info = Evm2AccountInfo::default().with_balance(account.balance);
        if let Some(code) = &account.code {
            info = info.with_code(evm2::bytecode::Bytecode::new_raw(code.clone()));
        }
        db.insert_account_info(&account.address, info);
        for (key, value) in &account.storage {
            db.insert_account_storage(&account.address, key, value);
        }
    }
    let block = BlockEnvExt {
        number: U256::from(7u64),
        timestamp: U256::from(1_700_000_000u64),
        beneficiary: BENEFICIARY,
        ..Default::default()
    };
    let mut evm = Evm::<TestTypes>::new_with_ext(
        Evm2SpecId::OSAKA,
        block,
        TxRegistry::new(),
        db,
        NoPrecompiles::default(),
        TxOrigin(ORIGIN),
    );
    let message = MessageExt {
        kind: if call.is_static { MessageKind::StaticCall } else { MessageKind::Call },
        gas_limit: call.gas_limit,
        destination: PRECOMPILE,
        call_target: PRECOMPILE,
        caller: CALLER,
        code_address: PRECOMPILE,
        ..MessageExt::default()
    };

    let result = if let Provider::GasFree = provider {
        let mut provider = Evm2JournalStorageProvider::new(&mut evm, CALLER);
        StorageCtx::enter(&mut provider, body)
    } else {
        let mut provider = Evm2PrecompileStorageProvider::new(
            &mut evm,
            &message,
            Version::base(Evm2SpecId::OSAKA).gas_params,
            call.storage_features,
        );
        StorageCtx::enter(&mut provider, body)
    };

    let storage = call
        .observed_slots
        .iter()
        .map(|(address, key)| evm.state().get_storage(address, key).unwrap_or_default())
        .collect();
    Outcome { result, logs: evm.logs().to_vec(), storage }
}

/// Runs `body` on both engines, asserts identical outcomes, and returns the shared outcome.
fn assert_engine_parity(call: Call, body: Body) -> Outcome {
    let revm = run_revm(&call, body);
    let evm2 = run_evm2(&call, body);
    assert_eq!(revm, evm2, "revm and evm2 providers diverged");
    revm
}

fn output(outcome: &Outcome) -> &base_precompile_storage::PrecompileOutput {
    outcome.result.as_ref().expect("precompile must not fail fatally")
}

fn slot(key: u64) -> U256 {
    U256::from(key)
}

#[test]
fn sstore_set_reset_clear_sequence_matches_with_refunds() {
    let call = Call {
        accounts: vec![Account {
            address: PRECOMPILE,
            storage: vec![(slot(1), U256::from(5u64))],
            ..Default::default()
        }],
        observed_slots: vec![(PRECOMPILE, slot(1)), (PRECOMPILE, slot(2))],
        ..Default::default()
    };

    let outcome = assert_engine_parity(call, |ctx| {
        let result = (|| {
            // 0 → 7 → 0: set a fresh slot, then restore its original zero.
            ctx.sstore(PRECOMPILE, slot(2), U256::from(7u64))?;
            ctx.sstore(PRECOMPILE, slot(2), U256::ZERO)?;
            // 5 → 9 → 0 → 5: reset, clear, then restore the original value.
            ctx.sstore(PRECOMPILE, slot(1), U256::from(9u64))?;
            ctx.sstore(PRECOMPILE, slot(1), U256::ZERO)?;
            ctx.sstore(PRECOMPILE, slot(1), U256::from(5u64))?;
            // Final clear keeps a net clearing refund.
            ctx.sstore(PRECOMPILE, slot(1), U256::ZERO)
        })();
        ctx.result_output(result, |()| Bytes::new())
    });

    let output = output(&outcome);
    assert!(output.is_success());
    assert!(output.gas_refunded > 0, "clearing an originally set slot must refund");
    assert_eq!(outcome.storage, vec![U256::ZERO, U256::ZERO]);
}

#[test]
fn cold_then_warm_sload_matches() {
    let call = Call {
        accounts: vec![Account {
            address: PRECOMPILE,
            storage: vec![(slot(3), U256::from(42u64))],
            ..Default::default()
        }],
        ..Default::default()
    };

    let cold_only = assert_engine_parity(call.clone(), |ctx| {
        let value = ctx.sload(PRECOMPILE, slot(3));
        ctx.result_output(value, |value| value.to_be_bytes::<32>().into())
    });
    let cold_then_warm = assert_engine_parity(call, |ctx| {
        let value = ctx.sload(PRECOMPILE, slot(3)).and_then(|_| ctx.sload(PRECOMPILE, slot(3)));
        ctx.result_output(value, |value| value.to_be_bytes::<32>().into())
    });

    let cold = output(&cold_only);
    let both = output(&cold_then_warm);
    assert_eq!(U256::from_be_slice(&both.bytes), U256::from(42u64));
    assert!(both.gas_used - cold.gas_used < cold.gas_used, "the second read must be warm");
}

#[test]
fn account_reads_warm_the_account_like_revm() {
    let call = Call {
        accounts: vec![Account { address: OTHER, balance: U256::from(9u64), ..Default::default() }],
        ..Default::default()
    };

    assert_engine_parity(call, |ctx| {
        // Reading a slot warms the account, so the following account read is warm.
        let result = ctx
            .sload(OTHER, slot(1))
            .and_then(|_| ctx.with_account_info(OTHER, |info| Ok(info.balance)))
            .and_then(|balance| {
                ctx.with_account_info(Address::repeat_byte(0x77), |info| {
                    Ok((balance, info.is_empty_code_hash(), info.nonce))
                })
            });
        ctx.result_output(result, |(balance, empty, nonce)| {
            [balance.to_be_bytes::<32>().as_slice(), &[u8::from(empty)], &nonce.to_be_bytes()]
                .concat()
                .into()
        })
    });
}

#[test]
fn cobalt_bytes_shrink_refund_matches() {
    let call = Call {
        storage_features: StorageFeatures::Cobalt,
        observed_slots: vec![(PRECOMPILE, slot(10))],
        ..Default::default()
    };

    let outcome = assert_engine_parity(call, |ctx| {
        let result = (|| {
            let mut bytes = BytesLikeHandler::<Bytes>::new(slot(10), PRECOMPILE, ctx);
            bytes.write(Bytes::from(vec![0x11; 64]))?;
            bytes.write(Bytes::from_static(b"short"))?;
            bytes.read()
        })();
        ctx.result_output(result, |bytes| bytes)
    });

    let output = output(&outcome);
    assert_eq!(output.bytes, Bytes::from_static(b"short"));
    assert!(output.gas_refunded > 0, "clearing long-data slots must record a refund");
}

#[test]
fn set_code_charges_new_and_prefunded_accounts_alike() {
    const PREFUNDED: Address = Address::repeat_byte(0x43);
    const FRESH: Address = Address::repeat_byte(0x44);
    const EXISTING: Address = Address::repeat_byte(0x45);
    let call = Call {
        accounts: vec![
            Account { address: PREFUNDED, balance: U256::from(1u64), ..Default::default() },
            Account {
                address: EXISTING,
                code: Some(Bytes::from_static(&[0x60, 0x01])),
                ..Default::default()
            },
        ],
        ..Default::default()
    };

    let outcome = assert_engine_parity(call, |ctx| {
        let result = (|| {
            let stub = || Bytecode::new_legacy(Bytes::from_static(&[0xef]));
            let gas_before = ctx.gas_used();
            ctx.set_code(FRESH, stub())?;
            let fresh_gas = ctx.gas_used() - gas_before;
            ctx.set_code(PREFUNDED, stub())?;
            let prefunded_gas = ctx.gas_used() - gas_before - fresh_gas;
            ctx.set_code(EXISTING, Bytecode::new_eip7702(OTHER))?;
            let hash = ctx.with_account_info(EXISTING, |info| Ok(info.code_hash))?;
            let delegate = ctx.with_account_code(EXISTING, |code| Ok(code.eip7702_address()))?;
            Ok((fresh_gas, prefunded_gas, hash, delegate))
        })();
        ctx.result_output(result, |(fresh_gas, prefunded_gas, hash, delegate)| {
            assert_eq!(fresh_gas, prefunded_gas);
            assert_eq!(delegate, Some(OTHER));
            [fresh_gas.to_be_bytes().as_slice(), hash.as_slice()].concat().into()
        })
    });

    assert!(output(&outcome).is_success());
}

#[test]
fn static_call_violations_match() {
    let call = Call { is_static: true, ..Default::default() };

    for body in [
        (|ctx| {
            let result = ctx.sstore(PRECOMPILE, slot(1), U256::from(1u64));
            ctx.result_output(result, |()| Bytes::new())
        }) as Body,
        |ctx| {
            let result = ctx.set_code(OTHER, Bytecode::new_legacy(Bytes::from_static(&[0xef])));
            ctx.result_output(result, |()| Bytes::new())
        },
        |ctx| {
            let result = ctx.emit_event(PRECOMPILE, LogData::new_unchecked(vec![], Bytes::new()));
            ctx.result_output(result, |()| Bytes::new())
        },
        |ctx| {
            let result = ctx.tstore(PRECOMPILE, slot(1), U256::from(1u64));
            ctx.result_output(result, |()| Bytes::new())
        },
    ] {
        let outcome = assert_engine_parity(call.clone(), body);
        assert!(output(&outcome).is_revert());
    }
}

#[test]
fn sstore_stipend_sentry_halts_out_of_gas() {
    let call = Call { gas_limit: 2_300, ..Default::default() };

    let outcome = assert_engine_parity(call, |ctx| {
        let result = ctx.sstore(PRECOMPILE, slot(1), U256::from(1u64));
        ctx.result_output(result, |()| Bytes::new())
    });

    assert!(output(&outcome).is_halt());
}

#[test]
fn logs_transient_storage_and_environment_match() {
    let outcome = assert_engine_parity(Call::default(), |ctx| {
        let result = (|| {
            ctx.emit_event(
                PRECOMPILE,
                LogData::new_unchecked(vec![TOPIC], Bytes::from_static(b"payload")),
            )?;
            ctx.tstore(PRECOMPILE, slot(4), U256::from(11u64))?;
            let transient = ctx.tload(PRECOMPILE, slot(4))?;
            let hash = ctx.metered_keccak256(b"base")?;
            let environment = [
                U256::from(ctx.chain_id()),
                U256::from(ctx.block_number()),
                ctx.timestamp(),
                U256::from_be_slice(ctx.beneficiary().as_slice()),
                U256::from_be_slice(ctx.origin().as_slice()),
                U256::from_be_slice(ctx.caller().as_slice()),
                transient,
                U256::from_be_bytes(hash.0),
            ];
            Ok(environment)
        })();
        ctx.result_output(result, |words| {
            words.iter().flat_map(|word| word.to_be_bytes::<32>()).collect::<Vec<_>>().into()
        })
    });

    assert_eq!(outcome.logs.len(), 1);
    assert_eq!(outcome.logs[0].address, PRECOMPILE);
}

#[test]
fn checkpoints_and_caller_override_match() {
    let call = Call {
        observed_slots: vec![(PRECOMPILE, slot(5)), (PRECOMPILE, slot(6))],
        ..Default::default()
    };

    let outcome = assert_engine_parity(call, |ctx| {
        let result = (|| {
            {
                let _reverted = ctx.checkpoint();
                ctx.sstore(PRECOMPILE, slot(5), U256::from(1u64))?;
                ctx.emit_event(PRECOMPILE, LogData::new_unchecked(vec![], Bytes::new()))?;
            }
            let committed = ctx.checkpoint();
            ctx.sstore(PRECOMPILE, slot(6), U256::from(2u64))?;
            committed.commit();
            let overridden = ctx.with_caller(OTHER, || ctx.caller());
            Ok((overridden, ctx.caller()))
        })();
        ctx.result_output(result, |(overridden, restored)| {
            [overridden.as_slice(), restored.as_slice()].concat().into()
        })
    });

    assert!(outcome.logs.is_empty(), "the reverted checkpoint must drop its log");
    assert_eq!(outcome.storage, vec![U256::ZERO, U256::from(2u64)]);
}

#[test]
fn out_of_gas_storage_access_is_reverted_on_both_engines() {
    let call = Call { gas_limit: 2_000, ..Default::default() };

    assert_engine_parity(call, |ctx| {
        // The cold read cannot be paid; a following read must still be cold on both engines.
        let first = ctx.sload(OTHER, slot(1));
        let gas_after_failure = ctx.gas_used();
        let second = ctx.sload(OTHER, slot(1));
        assert!(first.is_err() && second.is_err());
        ctx.result_output(Ok(gas_after_failure), |gas| gas.to_be_bytes().to_vec().into())
    });
}

#[test]
fn gas_free_journal_providers_match() {
    let call = Call {
        accounts: vec![
            Account { address: OTHER, balance: U256::from(9u64), ..Default::default() },
            Account {
                address: PRECOMPILE,
                storage: vec![(slot(1), U256::from(5u64))],
                ..Default::default()
            },
        ],
        observed_slots: vec![(PRECOMPILE, slot(1)), (PRECOMPILE, slot(2)), (OTHER, slot(3))],
        ..Default::default()
    };
    let body: Body = |ctx| {
        let result = (|| {
            let value = ctx.sload(PRECOMPILE, slot(1))?;
            ctx.sstore(PRECOMPILE, slot(2), value + U256::from(1u64))?;
            ctx.emit_event(PRECOMPILE, LogData::new_unchecked(vec![TOPIC], Bytes::new()))?;
            let reverted = ctx.checkpoint();
            ctx.sstore(OTHER, slot(3), U256::from(77u64))?;
            drop(reverted);
            ctx.set_code(
                Address::repeat_byte(0x55),
                Bytecode::new_legacy(Bytes::from_static(&[0])),
            )?;
            let code_hash =
                ctx.with_account_info(Address::repeat_byte(0x55), |info| Ok(info.code_hash))?;
            let balance = ctx.with_account_info(OTHER, |info| Ok(info.balance))?;
            Ok::<_, base_precompile_storage::BasePrecompileError>((
                value,
                balance,
                code_hash,
                ctx.origin(),
            ))
        })();
        ctx.result_output(result, |(value, balance, code_hash, origin)| {
            [
                value.to_be_bytes::<32>().as_slice(),
                &balance.to_be_bytes::<32>(),
                code_hash.as_slice(),
                origin.as_slice(),
            ]
            .concat()
            .into()
        })
    };
    let revm = run_revm_with(&call, body, Provider::GasFree);
    let evm2 = run_evm2_with(&call, body, Provider::GasFree);
    assert_eq!(revm, evm2, "revm and evm2 gas-free providers diverged");
    let output = output(&revm);
    assert_eq!(output.gas_used, 0, "the journal providers meter no gas");
    assert_eq!(revm.storage, vec![U256::from(5u64), U256::from(6u64), U256::ZERO]);
    assert_eq!(revm.logs.len(), 1);
}
