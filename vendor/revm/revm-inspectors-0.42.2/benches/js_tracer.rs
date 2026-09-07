#![cfg(feature = "js-tracer")]

//! End-to-end JavaScript tracer benchmarks for step-heavy contracts.

use alloy_hardforks::{ethereum::mainnet::*, EthereumHardfork};
use alloy_primitives::{Address, Bytes, U256};
use alloy_rpc_types_trace::geth::AccountState;
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use revm::{
    context::TxEnv,
    context_interface::{ContextTr, TransactTo},
    database::CacheDB,
    database_interface::EmptyDB,
    inspector::InspectorEvmTr,
    primitives::hardfork::SpecId,
    state::{AccountInfo, Bytecode},
    InspectEvm, MainBuilder, MainContext,
};
use revm_inspectors::tracing::js::JsInspector;
use serde::Deserialize;
use std::{collections::BTreeMap, hint::black_box};

const CONTRACT_REPETITIONS: usize = 5_000;
const RUNDLER_STYLE_REPETITIONS: u16 = 5_000;
const HELPER_ADDRESS: Address = Address::repeat_byte(0x02);
const MAINNET_AA_TX_HASH: &str =
    "0x1e664de3785a6fe2fc71c4a790fadb3b935ba9f6306b1a6a908d703457a84c12";
const MAINNET_AA_BLOCK_NUMBER: u64 = 24_921_426;
const MAINNET_AA_BLOCK_TIMESTAMP: u64 = 1_776_691_931;
const MAINNET_AA_BLOCK_BASE_FEE: u64 = 3_601_324_605;
const MAINNET_AA_BLOCK_COINBASE: Address = Address::new([
    0x48, 0x38, 0xb1, 0x06, 0xfc, 0xe9, 0x64, 0x7b, 0xdf, 0x1e, 0x78, 0x77, 0xbf, 0x73, 0xce, 0x8b,
    0x0b, 0xad, 0x5f, 0x97,
]);
const MAINNET_AA_PRESTATE: &str = include_str!("../testdata/repro/tx-aa-handleops-mainnet.json");
// Vendored from alchemyplatform/rundler @ 073b093112e8b27dbf62ef6ede7664526e09243b.
const RUNDLER_V06_REAL_SCRIPT: &str = include_str!("testdata/validationTracerV0_6.js");
const RUNDLER_V07_REAL_SCRIPT: &str = include_str!("testdata/validationTracerV0_7.js");
const STOP_COLLECTING_TOPIC: [u8; 32] = [
    0xbb, 0x47, 0xee, 0x3e, 0x18, 0x3a, 0x55, 0x8b, 0x1a, 0x2f, 0xf0, 0x87, 0x4b, 0x07, 0x9f, 0x3f,
    0xc5, 0x47, 0x8b, 0x74, 0x54, 0xea, 0xcf, 0x2b, 0xfc, 0x5a, 0xf2, 0xff, 0x58, 0x78, 0xf9, 0x72,
];

fn step_heavy_contract() -> Bytes {
    let mut code = Vec::with_capacity((CONTRACT_REPETITIONS * 6) + 1);
    for _ in 0..CONTRACT_REPETITIONS {
        code.extend_from_slice(&[0x60, 0x01, 0x60, 0x02, 0x01, 0x50]);
    }
    code.push(0x00);
    code.into()
}

fn rundler_style_contract() -> Bytes {
    let mut code = Vec::with_capacity(256);
    for _ in 0..3 {
        code.extend_from_slice(&[0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00]);
        code.push(0x73);
        code.extend_from_slice(HELPER_ADDRESS.as_slice());
        code.extend_from_slice(&[0x61, 0x30, 0x39, 0xf1, 0x50]);
        code.extend_from_slice(&[0x43, 0x50]);
    }
    code.push(0x7f);
    code.extend_from_slice(&STOP_COLLECTING_TOPIC);
    code.extend_from_slice(&[0x60, 0x00, 0x60, 0x00, 0xa1, 0x00]);
    code.into()
}

fn rundler_style_helper_contract() -> Bytes {
    const MEMORY_WORD: [u8; 32] = [0x11; 32];
    const HELPER_LOG_TOPIC: [u8; 32] = [0x22; 32];

    let mut code = Vec::with_capacity(256);
    code.extend_from_slice(&[
        0x61,
        (RUNDLER_STYLE_REPETITIONS >> 8) as u8,
        RUNDLER_STYLE_REPETITIONS as u8,
    ]);

    let loop_start = code.len();
    code.push(0x5b);
    code.extend_from_slice(&[0x80, 0x15, 0x61, 0x00, 0x00, 0x57]);
    let loop_end_patch_offset = loop_start + 4;

    code.push(0x7f);
    code.extend_from_slice(&MEMORY_WORD);
    code.extend_from_slice(&[0x60, 0x00, 0x52]);
    code.extend_from_slice(&[0x60, 0x20, 0x60, 0x00, 0x20, 0x50]);
    code.extend_from_slice(&[0x60, 0x00, 0x54, 0x50]);
    code.extend_from_slice(&[0x42, 0x50, 0x5a, 0x50]);
    code.push(0x73);
    code.extend_from_slice(Address::repeat_byte(0x01).as_slice());
    code.extend_from_slice(&[0x3b, 0x50]);

    code.extend_from_slice(&[0x60, 0x01, 0x03, 0x61, 0x00, 0x00, 0x56]);
    let loop_back_patch_offset = code.len() - 3;

    let loop_end = code.len();
    code.push(0x5b);
    code.extend_from_slice(&[0x50]);
    code.push(0x7f);
    code.extend_from_slice(&HELPER_LOG_TOPIC);
    code.extend_from_slice(&[0x60, 0x00, 0x60, 0x00, 0xa1, 0x00]);

    code[loop_end_patch_offset] = ((loop_end >> 8) & 0xff) as u8;
    code[loop_end_patch_offset + 1] = (loop_end & 0xff) as u8;
    code[loop_back_patch_offset] = ((loop_start >> 8) & 0xff) as u8;
    code[loop_back_patch_offset + 1] = (loop_start & 0xff) as u8;

    code.into()
}

fn normalize_tracer_script(script: &str) -> String {
    script.trim().trim_end_matches(";export{};").to_owned()
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PrestateResponse {
    Direct(BTreeMap<Address, AccountState>),
    Wrapped { result: BTreeMap<Address, AccountState> },
}

impl PrestateResponse {
    fn into_prestate(self) -> BTreeMap<Address, AccountState> {
        match self {
            Self::Direct(prestate) => prestate,
            Self::Wrapped { result } => result,
        }
    }
}

fn spec_id_from_ethereum_hardfork(hardfork: EthereumHardfork) -> SpecId {
    match hardfork {
        EthereumHardfork::Frontier => SpecId::FRONTIER,
        EthereumHardfork::Homestead => SpecId::HOMESTEAD,
        EthereumHardfork::Dao => SpecId::HOMESTEAD,
        EthereumHardfork::Tangerine => SpecId::TANGERINE,
        EthereumHardfork::SpuriousDragon => SpecId::SPURIOUS_DRAGON,
        EthereumHardfork::Byzantium => SpecId::BYZANTIUM,
        EthereumHardfork::Constantinople => SpecId::PETERSBURG,
        EthereumHardfork::Petersburg => SpecId::PETERSBURG,
        EthereumHardfork::Istanbul => SpecId::ISTANBUL,
        EthereumHardfork::MuirGlacier => SpecId::ISTANBUL,
        EthereumHardfork::Berlin => SpecId::BERLIN,
        EthereumHardfork::London => SpecId::LONDON,
        EthereumHardfork::ArrowGlacier => SpecId::LONDON,
        EthereumHardfork::GrayGlacier => SpecId::LONDON,
        EthereumHardfork::Paris => SpecId::MERGE,
        EthereumHardfork::Shanghai => SpecId::SHANGHAI,
        EthereumHardfork::Cancun => SpecId::CANCUN,
        EthereumHardfork::Prague => SpecId::PRAGUE,
        EthereumHardfork::Osaka => SpecId::OSAKA,
        _ => SpecId::PRAGUE,
    }
}

fn spec_id_from_block(block_number: u64) -> SpecId {
    let hardfork = if block_number >= MAINNET_PRAGUE_BLOCK {
        EthereumHardfork::Prague
    } else if block_number >= MAINNET_CANCUN_BLOCK {
        EthereumHardfork::Cancun
    } else if block_number >= MAINNET_SHANGHAI_BLOCK {
        EthereumHardfork::Shanghai
    } else if block_number >= MAINNET_PARIS_BLOCK {
        EthereumHardfork::Paris
    } else if block_number >= MAINNET_GRAY_GLACIER_BLOCK {
        EthereumHardfork::GrayGlacier
    } else if block_number >= MAINNET_ARROW_GLACIER_BLOCK {
        EthereumHardfork::ArrowGlacier
    } else if block_number >= MAINNET_LONDON_BLOCK {
        EthereumHardfork::London
    } else if block_number >= MAINNET_BERLIN_BLOCK {
        EthereumHardfork::Berlin
    } else if block_number >= MAINNET_MUIR_GLACIER_BLOCK {
        EthereumHardfork::MuirGlacier
    } else if block_number >= MAINNET_ISTANBUL_BLOCK {
        EthereumHardfork::Istanbul
    } else if block_number >= MAINNET_PETERSBURG_BLOCK {
        EthereumHardfork::Petersburg
    } else if block_number >= MAINNET_BYZANTIUM_BLOCK {
        EthereumHardfork::Byzantium
    } else if block_number >= MAINNET_SPURIOUS_DRAGON_BLOCK {
        EthereumHardfork::SpuriousDragon
    } else if block_number >= MAINNET_TANGERINE_BLOCK {
        EthereumHardfork::Tangerine
    } else if block_number >= MAINNET_DAO_BLOCK {
        EthereumHardfork::Dao
    } else if block_number >= MAINNET_HOMESTEAD_BLOCK {
        EthereumHardfork::Homestead
    } else {
        EthereumHardfork::Frontier
    };

    spec_id_from_ethereum_hardfork(hardfork)
}

fn build_db_from_prestate(prestate: &BTreeMap<Address, AccountState>) -> CacheDB<EmptyDB> {
    let mut db = CacheDB::new(EmptyDB::default());

    for (address, state) in prestate {
        let balance = state.balance.unwrap_or_default();
        let nonce = state.nonce.unwrap_or_default();
        let code = state.code.as_ref().map(|code| revm::bytecode::Bytecode::new_raw(code.clone()));

        db.insert_account_info(
            *address,
            AccountInfo {
                balance,
                nonce,
                code_hash: code.as_ref().map(|code| code.hash_slow()).unwrap_or_default(),
                code,
                ..Default::default()
            },
        );

        for (slot, value) in &state.storage {
            db.insert_account_storage(*address, (*slot).into(), (*value).into()).unwrap();
        }
    }

    db
}

fn mainnet_aa_db() -> CacheDB<EmptyDB> {
    let response: PrestateResponse = serde_json::from_str(MAINNET_AA_PRESTATE).unwrap();
    build_db_from_prestate(&response.into_prestate())
}

fn mainnet_aa_tx_env() -> TxEnv {
    TxEnv {
        caller: Address::new([0x43, 0x37, 0x01, 0x46, 0x36, 0x6c, 0xfe, 0xa2, 0x13, 0x02, 0x96, 0x2a, 0xe3, 0xa5, 0x58, 0xd7, 0x2b, 0xbc, 0xc4, 0x87]),
        kind: TransactTo::Call(Address::new([0x5f, 0xf1, 0x37, 0xd4, 0xb0, 0xfd, 0xcd, 0x49, 0xdc, 0xa3, 0x0c, 0x7c, 0xf5, 0x7e, 0x57, 0x8a, 0x02, 0x6d, 0x27, 0x89])),
        data: Bytes::from_static(&alloy_primitives::hex!(
            "1fad948c000000000000000000000000000000000000000000000000000000000000004000000000000000000000000043370146366cfea21302962ae3a558d72bbcc4870000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000002000000000000000000000000093e56cb043e611a9d469b94488e4d35ee151798f000000000000000000000000000000000000019dab17438e000000000000000000000000000000000000000000000000000000000000000000000000000001600000000000000000000000000000000000000000000000000000000000000180000000000000000000000000000000000000000000000000000000000007a1200000000000000000000000000000000000000000000000000000000000033bb6000000000000000000000000000000000000000000000000000000000000ec78000000000000000000000000000000000000000000000000000000030b3417700000000000000000000000000000000000000000000000000000000098c5642e000000000000000000000000000000000000000000000000000000000000034000000000000000000000000000000000000000000000000000000000000003e000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000184b61d27f600000000000000000000000028b5a0e9c621a5badaa536219b3a228c8168cf5d0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000e48e0250ee000000000000000000000000000000000000000000000000000000000020318b000000000000000000000000000000000000000000000000000000000000000600000000000000000000000093e56cb043e611a9d469b94488e4d35ee151798f000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000626666666666667849c56f2850848ce1c4da65c68b00000069e62d2a000000000000d47e9f8a20182a29e87fe5e4b4652426a016771fdb998408aeb65391df49cbfc5590376a07c17aff0f1aab8471a0aa9ba1ef97efa259d9b02b60b091c87e6a5b1b00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000e0000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000004164fca7710342d737ccee3255b317d979480f5503be0981a38743c652163fd5c30f97a3d3220cf930245954855e02992057c98dfb485291402527bf276622adf81b00000000000000000000000000000000000000000000000000000000000000"
        )),
        nonce: 44_455,
        gas_limit: 1_019_195,
        gas_price: 5_690_493_805,
        gas_priority_fee: Some(5_690_493_805),
        value: U256::ZERO,
        ..Default::default()
    }
}

fn run_trace(script: &str, contract: &Bytes, helper_contract: Option<&Bytes>) -> serde_json::Value {
    let contract_address = Address::repeat_byte(0x01);
    let mut db = CacheDB::new(EmptyDB::default());

    db.insert_account_info(
        Address::ZERO,
        AccountInfo { balance: U256::from(1e18), ..Default::default() },
    );
    db.insert_account_info(
        contract_address,
        AccountInfo { code: Some(Bytecode::new_legacy(contract.clone())), ..Default::default() },
    );
    if let Some(helper_contract) = helper_contract {
        db.insert_account_info(
            HELPER_ADDRESS,
            AccountInfo {
                code: Some(Bytecode::new_legacy(helper_contract.clone())),
                ..Default::default()
            },
        );
    }

    let inspector = JsInspector::new(script.to_owned(), serde_json::Value::Null).unwrap();
    let mut evm = revm::Context::mainnet()
        .modify_cfg_chained(|cfg| cfg.spec = SpecId::CANCUN)
        .with_db(db)
        .build_mainnet_with_inspector(inspector);

    let res = evm
        .inspect_tx(TxEnv {
            gas_price: 1024,
            gas_limit: 20_000_000,
            gas_priority_fee: None,
            kind: TransactTo::Call(contract_address),
            ..Default::default()
        })
        .expect("transaction should execute");

    let (ctx, inspector) = evm.ctx_inspector();
    let tx = ctx.tx().clone();
    let block = ctx.block().clone();
    inspector.json_result(res, &tx, &block, ctx.db_mut()).unwrap()
}

fn run_mainnet_aa_trace(script: &str, db: CacheDB<EmptyDB>) -> serde_json::Value {
    let inspector = JsInspector::new(script.to_owned(), serde_json::Value::Null).unwrap();
    let mut evm = revm::Context::mainnet()
        .with_db(db)
        .modify_cfg_chained(|cfg| cfg.spec = spec_id_from_block(MAINNET_AA_BLOCK_NUMBER))
        .modify_block_chained(|block| {
            block.number = U256::from(MAINNET_AA_BLOCK_NUMBER);
            block.timestamp = U256::from(MAINNET_AA_BLOCK_TIMESTAMP);
            block.basefee = MAINNET_AA_BLOCK_BASE_FEE;
            block.beneficiary = MAINNET_AA_BLOCK_COINBASE;
        })
        .build_mainnet_with_inspector(inspector);

    let res = evm.inspect_tx(mainnet_aa_tx_env()).unwrap_or_else(|error| {
        panic!("mainnet AA tx {MAINNET_AA_TX_HASH} should execute: {error:?}")
    });

    let (ctx, inspector) = evm.ctx_inspector();
    let tx = ctx.tx().clone();
    let block = ctx.block().clone();
    inspector.json_result(res, &tx, &block, ctx.db_mut()).unwrap()
}

fn js_tracer_benches(c: &mut Criterion) {
    let contract = step_heavy_contract();
    let rundler_contract = rundler_style_contract();
    let helper_contract = rundler_style_helper_contract();
    let mainnet_aa_db = mainnet_aa_db();
    let rundler_v06_script = normalize_tracer_script(RUNDLER_V06_REAL_SCRIPT);
    let rundler_v07_script = normalize_tracer_script(RUNDLER_V07_REAL_SCRIPT);
    let noop_script = r#"{
        step: function() {},
        fault: function() {},
        result: function() { return 0; }
    }"#;
    let accessor_script = r#"{
        acc: 0,
        step: function(log, db) {
            this.acc += log.op.toNumber();
            this.acc += log.stack.length();
            this.acc += log.memory.length();
            if (db.exists(log.contract.getAddress())) {
                this.acc += 1;
            }
        },
        fault: function() {},
        result: function() { return this.acc; }
    }"#;
    let rundler_v06_style_script = r#"{
        acc: 0,
        phases: 0,
        step: function(log, db) {
            var op = log.op.toString();
            this.acc += log.getGas();
            this.acc += log.getCost();
            this.acc += log.getDepth();
            this.acc += log.stack.length();
            this.acc += log.memory.length();

            if (op === 'NUMBER') {
                this.phases += 1;
            }

            if (op === 'KECCAK256' || op === 'SHA3') {
                if (log.memory.length() >= 32) {
                    this.acc += log.memory.slice(0, 32).length;
                }
            }

            if (op === 'SLOAD' || op === 'SSTORE') {
                this.acc += db.getState(log.contract.getAddress(), log.stack.peek(0)).toString(16).length;
            }

            if (op === 'EXTCODESIZE' || op === 'CALL') {
                this.acc += db.getCode(log.contract.getAddress()).length;
                if (db.exists(log.contract.getAddress())) {
                    this.acc += 1;
                }
            }
        },
        enter: function(frame) {
            this.acc += frame.getGas();
            this.acc += frame.getInput().length;
            this.acc += frame.getType().length;
        },
        exit: function(frame) {
            this.acc += frame.getGasUsed();
            this.acc += frame.getOutput().length;
            if (frame.getError()) {
                this.acc += frame.getError().length;
            }
        },
        fault: function(log) {
            this.acc += log.getPC();
        },
        result: function() { return this.acc + this.phases; }
    }"#;
    let rundler_v07_style_script = r#"{
        acc: 0,
        opcounts: {},
        calls: 0,
        logs: 0,
        step: function(log, db) {
            var op = log.op.toString();
            this.opcounts[op] = (this.opcounts[op] || 0) + 1;
            this.acc += log.getRefund();

            if (op === 'KECCAK256' || op === 'SHA3') {
                if (log.memory.length() >= 32) {
                    this.acc += log.memory.getUint(0).length;
                }
            }

            if (op === 'LOG1') {
                this.logs += 1;
                this.acc += log.stack.peek(0).toString(16).length;
                this.acc += log.stack.peek(1).toString(16).length;
            }

            if (op === 'SLOAD' || op === 'SSTORE') {
                this.acc += db.getState(log.contract.getAddress(), log.stack.peek(0)).toString(16).length;
            }

            if (op === 'EXTCODESIZE' || op === 'CALL') {
                this.acc += db.getCode(log.contract.getAddress()).length;
            }
        },
        enter: function(frame) {
            this.calls += 1;
            this.acc += frame.getFrom().length;
            this.acc += frame.getTo().length;
            this.acc += frame.getInput().length;
        },
        exit: function(frame) {
            this.acc += frame.getGasUsed();
            this.acc += frame.getOutput().length;
        },
        fault: function(log) {
            this.acc += log.getCost();
        },
        result: function() { return this.acc + this.calls + this.logs; }
    }"#;

    let mut group = c.benchmark_group("js_tracer");
    group.sample_size(10);

    group.bench_function("step_noop", |b| {
        b.iter_batched(
            || contract.clone(),
            |contract| {
                black_box(run_trace(noop_script, &contract, None));
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("step_accessors", |b| {
        b.iter_batched(
            || contract.clone(),
            |contract| {
                black_box(run_trace(accessor_script, &contract, None));
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("rundler_v06_style", |b| {
        b.iter_batched(
            || (rundler_contract.clone(), helper_contract.clone()),
            |(contract, helper)| {
                black_box(run_trace(rundler_v06_style_script, &contract, Some(&helper)));
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("rundler_v07_style", |b| {
        b.iter_batched(
            || (rundler_contract.clone(), helper_contract.clone()),
            |(contract, helper)| {
                black_box(run_trace(rundler_v07_style_script, &contract, Some(&helper)));
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("rundler_v06_real", |b| {
        b.iter_batched(
            || (rundler_contract.clone(), helper_contract.clone()),
            |(contract, helper)| {
                black_box(run_trace(&rundler_v06_script, &contract, Some(&helper)));
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("mainnet_aa_v06_real", |b| {
        b.iter_batched(
            || mainnet_aa_db.clone(),
            |db| {
                black_box(run_mainnet_aa_trace(&rundler_v06_script, db));
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("rundler_v07_real", |b| {
        b.iter_batched(
            || (rundler_contract.clone(), helper_contract.clone()),
            |(contract, helper)| {
                black_box(run_trace(&rundler_v07_script, &contract, Some(&helper)));
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(benches, js_tracer_benches);
criterion_main!(benches);
