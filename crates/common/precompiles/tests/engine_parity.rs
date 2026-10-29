//! Engine parity: Base native precompiles dispatched through the revm `BasePrecompiles` map and the
//! evm2 `BaseEvm2Precompiles` provider return identical output, gas, refunds, state gas, logs, and
//! storage.

use alloy_evm::{
    EvmInternals,
    eth::EthEvmContext,
    precompiles::{Precompile as _, PrecompileInput},
};
use alloy_primitives::{Address, Bytes, Log, U256};
use alloy_sol_types::SolCall;
use base_common_genesis::BaseUpgrade;
use base_common_precompiles::{
    ActivationAdminConfig, ActivationFeature, ActivationRegistryStorage, B20_MAX_SUPPLY_CAP,
    B20StablecoinInit, B20StablecoinStorage, B20Variant, BaseEvm2Precompiles, BasePrecompiles,
    IB20, IPolicyRegistry, PolicyRegistryStorage, TokenAccounting, UpgradeGatedStorageFeatures,
};
use base_precompile_storage::{Evm2TxOrigin, HashMapStorageProvider, StorageCtx};
use evm2::{
    BaseEvmConfigSelector, Evm, EvmTypesHost, PrecompileError, Precompiles, SpecId as Evm2SpecId,
    env::BlockEnvExt,
    ethereum::TxEnvelope,
    evm::{
        AccountInfo as Evm2AccountInfo, InMemoryDB as Evm2InMemoryDB,
        precompile::{NoPrecompiles, PrecompileProvider},
    },
    interpreter::{GasTracker, MessageExt, MessageKind},
    registry::TxRegistry,
};
use revm::{
    database::InMemoryDB as RevmInMemoryDB,
    handler::precompile_output_to_interpreter_result,
    primitives::hardfork::SpecId as RevmSpecId,
    state::{AccountInfo as RevmAccountInfo, Bytecode as RevmBytecode},
};
use rstest::rstest;

const CHAIN_ID: u64 = 1;
const ALICE: Address = Address::repeat_byte(0xA1);
const BOB: Address = Address::repeat_byte(0xB0);
const ADMIN: Address = Address::repeat_byte(0xAD);
const ORIGIN: Address = Address::repeat_byte(0x0F);
const GAS_LIMIT: u64 = 1_000_000;
/// Code `#[contract]` storage installs on an initialized token account.
const TOKEN_MARKER_CODE: [u8; 1] = [0xef];

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

/// How a precompile frame ended.
#[derive(Debug, PartialEq, Eq)]
enum Status {
    Success,
    Revert,
    Halt,
}

/// Everything a precompile frame exposes to the calling engine.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    status: Status,
    output: Bytes,
    gas_spent: u64,
    gas_refunded: i64,
    state_gas_spent: i64,
    logs: Vec<Log>,
    storage: Vec<(Address, U256, U256)>,
}

/// Pre-state shared by both engines: storage slots plus accounts carrying the token marker code.
struct PreState {
    storage: Vec<(Address, U256, U256)>,
    coded_accounts: Vec<Address>,
}

/// One native precompile call.
struct Call {
    upgrade: BaseUpgrade,
    caller: Address,
    target: Address,
    calldata: Bytes,
    /// Runs the precompile in the caller's account context, as `DELEGATECALL` does.
    delegated: bool,
}

impl Call {
    /// Account whose context the precompile runs in.
    const fn context(&self) -> Address {
        if self.delegated { self.caller } else { self.target }
    }
}

fn stablecoin_address() -> Address {
    let mut bytes = [0u8; 20];
    bytes[0] = B20Variant::PREFIX_BYTE;
    bytes[10] = B20Variant::Stablecoin.discriminant();
    bytes[19] = 1;
    Address::from(bytes)
}

/// Initializes a stablecoin at `token` and funds `holder`, as committed pre-state.
fn funded_stablecoin(upgrade: BaseUpgrade, token: Address, holder: Address) -> PreState {
    let mut storage = HashMapStorageProvider::new_with_storage_features(
        CHAIN_ID,
        UpgradeGatedStorageFeatures::from_upgrade(upgrade),
    );
    StorageCtx::enter(&mut storage, |ctx| {
        let mut coin = B20StablecoinStorage::from_address(token, ctx);
        coin.initialize(B20StablecoinInit {
            name: "Base USD".into(),
            symbol: "BUSD".into(),
            supply_cap: B20_MAX_SUPPLY_CAP,
            currency: "USD".into(),
        })
        .unwrap();
        coin.set_balance(holder, U256::from(1_000u64)).unwrap();
        coin.set_total_supply(U256::from(1_000u64)).unwrap();
    });
    PreState { storage: storage.into_storage().collect(), coded_accounts: vec![token] }
}

/// Activates the policy registry, as committed pre-state.
fn activated_policy_registry(upgrade: BaseUpgrade) -> PreState {
    let mut storage = HashMapStorageProvider::new_with_storage_features(
        CHAIN_ID,
        UpgradeGatedStorageFeatures::from_upgrade(upgrade),
    );
    storage.set_caller(ADMIN);
    StorageCtx::enter(&mut storage, |ctx| {
        ActivationRegistryStorage::new(ctx)
            .activate(
                ActivationFeature::PolicyRegistry.id(),
                ActivationAdminConfig::static_fallback(Some(ADMIN)),
            )
            .unwrap();
    });
    PreState { storage: storage.into_storage().collect(), coded_accounts: Vec::new() }
}

fn run_revm(pre: &PreState, call: &Call) -> Outcome {
    let mut db = RevmInMemoryDB::default();
    for address in &pre.coded_accounts {
        let code = RevmBytecode::new_legacy(Bytes::from_static(&TOKEN_MARKER_CODE));
        db.insert_account_info(
            *address,
            RevmAccountInfo { code_hash: code.hash_slow(), code: Some(code), ..Default::default() },
        );
    }
    for (address, key, value) in &pre.storage {
        db.insert_account_storage(*address, *key, *value).unwrap();
    }
    let mut ctx = EthEvmContext::new(db, RevmSpecId::OSAKA);
    ctx.tx.caller = ORIGIN;

    let precompiles = BasePrecompiles::<BaseUpgrade>::new_with_spec(call.upgrade).install();
    let precompile = precompiles.get(&call.target).expect("native precompile installed");
    let output = precompile
        .call(PrecompileInput {
            data: &call.calldata,
            gas: GAS_LIMIT,
            reservoir: 0,
            caller: call.caller,
            value: U256::ZERO,
            target_address: call.context(),
            is_static: false,
            bytecode_address: call.target,
            internals: EvmInternals::from_context(&mut ctx),
        })
        .expect("native precompile must not fail fatally");
    let frame = precompile_output_to_interpreter_result(output, GAS_LIMIT);

    let status = if frame.result.is_ok() {
        Status::Success
    } else if frame.result.is_revert() {
        Status::Revert
    } else {
        Status::Halt
    };
    let mut storage: Vec<_> = ctx
        .journaled_state
        .inner
        .state
        .iter()
        .flat_map(|(address, account)| {
            account.storage.iter().map(|(key, slot)| (*address, *key, slot.present_value))
        })
        .collect();
    storage.sort();
    Outcome {
        status,
        output: frame.output,
        gas_spent: frame.gas.total_gas_spent(),
        gas_refunded: frame.gas.refunded(),
        state_gas_spent: frame.gas.state_gas_spent(),
        logs: ctx.journaled_state.inner.logs.clone(),
        storage,
    }
}

/// Runs `call` on evm2, reporting the values of the slots revm touched.
fn run_evm2(pre: &PreState, call: &Call, slots: &[(Address, U256)]) -> Outcome {
    let mut db = Evm2InMemoryDB::default();
    for address in &pre.coded_accounts {
        let code = evm2::bytecode::Bytecode::new_legacy(Bytes::from_static(&TOKEN_MARKER_CODE));
        db.insert_account_info(address, Evm2AccountInfo::default().with_code(code));
    }
    for (address, key, value) in &pre.storage {
        // Unlike revm's, evm2's in-memory database does not create the account that owns
        // inserted storage, and storage of an absent account reads as zero.
        if !pre.coded_accounts.contains(address) {
            db.insert_account_info(address, Evm2AccountInfo::default());
        }
        db.insert_account_storage(address, key, value);
    }
    let mut evm = Evm::<TestTypes>::new_with_ext(
        Evm2SpecId::OSAKA,
        BlockEnvExt::default(),
        TxRegistry::new(),
        db,
        NoPrecompiles::default(),
        TxOrigin(ORIGIN),
    );
    let message = MessageExt {
        kind: MessageKind::Call,
        gas_limit: GAS_LIMIT,
        destination: call.context(),
        call_target: call.target,
        caller: call.caller,
        code_address: call.target,
        input: call.calldata.clone(),
        ..MessageExt::default()
    };

    let mut precompiles =
        BaseEvm2Precompiles::new(Precompiles::base(Evm2SpecId::OSAKA), call.upgrade);
    let mut gas = GasTracker::new(GAS_LIMIT);
    let result =
        precompiles.execute(&mut evm, &message, &mut gas).expect("native precompile installed");

    let (status, output) = match result {
        Ok(output) => (Status::Success, output.into_bytes()),
        Err(PrecompileError::Revert(output)) => (Status::Revert, output),
        Err(PrecompileError::Halt(_)) => (Status::Halt, Bytes::new()),
        Err(error) => panic!("native precompile must not fail fatally: {error:?}"),
    };
    let storage = slots
        .iter()
        .map(|(address, key)| {
            (*address, *key, evm.state().get_storage(address, key).unwrap_or_default())
        })
        .collect();
    Outcome {
        status,
        output,
        gas_spent: gas.spent(),
        gas_refunded: gas.refunded(),
        state_gas_spent: gas.state_gas_spent(),
        logs: evm.logs().to_vec(),
        storage,
    }
}

/// Runs `call` on both engines, asserts identical outcomes, and returns the shared outcome.
fn assert_engine_parity(pre: &PreState, call: &Call) -> Outcome {
    let revm = run_revm(pre, call);
    let slots: Vec<_> = revm.storage.iter().map(|(address, key, _)| (*address, *key)).collect();
    let evm2 = run_evm2(pre, call, &slots);
    assert_eq!(revm, evm2, "revm and evm2 native precompile dispatch diverged");
    revm
}

#[rstest]
#[case::beryl(BaseUpgrade::Beryl)]
#[case::cobalt(BaseUpgrade::Cobalt)]
fn b20_transfer_matches(#[case] upgrade: BaseUpgrade) {
    let token = stablecoin_address();
    let pre = funded_stablecoin(upgrade, token, ALICE);
    let call = Call {
        upgrade,
        caller: ALICE,
        target: token,
        calldata: IB20::transferCall { to: BOB, amount: U256::from(250u64) }.abi_encode().into(),
        delegated: false,
    };

    let outcome = assert_engine_parity(&pre, &call);

    assert_eq!(outcome.status, Status::Success);
    assert!(IB20::transferCall::abi_decode_returns(&outcome.output).unwrap());
    assert_eq!(outcome.logs.len(), 1, "a transfer emits one Transfer event");
}

#[rstest]
#[case::beryl(BaseUpgrade::Beryl)]
#[case::cobalt(BaseUpgrade::Cobalt)]
fn b20_overdraft_reverts_alike(#[case] upgrade: BaseUpgrade) {
    let token = stablecoin_address();
    let pre = funded_stablecoin(upgrade, token, ALICE);
    let call = Call {
        upgrade,
        caller: ALICE,
        target: token,
        calldata: IB20::transferCall { to: BOB, amount: U256::from(1_001u64) }.abi_encode().into(),
        delegated: false,
    };

    let outcome = assert_engine_parity(&pre, &call);

    assert_eq!(outcome.status, Status::Revert, "an overdraft reverts");
}

#[rstest]
#[case::beryl(BaseUpgrade::Beryl)]
#[case::cobalt(BaseUpgrade::Cobalt)]
fn policy_creation_matches(#[case] upgrade: BaseUpgrade) {
    let call = Call {
        upgrade,
        caller: ALICE,
        target: PolicyRegistryStorage::ADDRESS,
        calldata: IPolicyRegistry::createPolicyCall {
            admin: ADMIN,
            policyType: IPolicyRegistry::PolicyType::ALLOWLIST,
        }
        .abi_encode()
        .into(),
        delegated: false,
    };

    let outcome = assert_engine_parity(&activated_policy_registry(upgrade), &call);

    assert_eq!(outcome.status, Status::Success);
    assert!(!outcome.logs.is_empty(), "policy creation emits an event");
}

#[rstest]
#[case::beryl(BaseUpgrade::Beryl)]
#[case::cobalt(BaseUpgrade::Cobalt)]
fn delegated_call_is_rejected_alike(#[case] upgrade: BaseUpgrade) {
    let token = stablecoin_address();
    let pre = funded_stablecoin(upgrade, token, ALICE);
    let call = Call {
        upgrade,
        caller: ALICE,
        target: token,
        calldata: IB20::transferCall { to: BOB, amount: U256::from(250u64) }.abi_encode().into(),
        delegated: true,
    };

    let outcome = assert_engine_parity(&pre, &call);

    assert_eq!(outcome.status, Status::Revert);
    assert_eq!(outcome.gas_spent, 0);
}
