#![cfg(feature = "t4b-shadow")]
#![doc = "In-node selected-route authority, synthetic reachability, and realistic-cost tests."]

use std::{cell::Cell, convert::Infallible, error::Error, fmt, fs, path::PathBuf, rc::Rc};

use alloy_primitives::{Address, B256, TxKind, U256, U512, aliases::I512, hex};
use base_common_consensus::Predeploys;
use base_common_evm::L1BlockInfo;
use base_common_evm::{
    BaseContext, BaseSpecId, BaseTransaction, BaseUpgrade, Builder, DefaultBase,
};
use base_execution_cli::{
    AuditPhase, AuditedDatabase, AuditedDatabaseError, CandidateAccessAllowlistV1,
    CandidateAccessedStateV1, CandidateExecutionCardinalityV1,
};
use base_mev_trader::{
    AdmissionStageV2, AdmissionTerminalV2, AttemptedAuthorityUnavailableReasonV2,
    AttemptedAuthorityUnavailableV2, AuthorityUnavailableV2, CanonicalL1FeeEvidenceV2,
    PriorityEconomicsCountersV2, PriorityEconomicsV2, SelectedRouteEvidenceV2,
};
use revm::{
    Database, DatabaseCommit, ExecuteEvm,
    context::{BlockEnv, CfgEnv, TxEnv},
    database::InMemoryDB,
    database_interface::DBErrorMarker,
    primitives::{AddressMap, StorageKey, StorageValue},
    state::{Account, AccountId, AccountInfo, TransactionId},
};
use revm_bytecode::Bytecode;
use serde_json::{Value, json};

const CLAIM: &str = "pool and fee environment are synthetic/reachability-only; this evidence does not represent live economics";
const R61_MANIFEST: &str = include_str!("../../mev-trader-submit/tests/fixtures/r61-manifest.json");
const MEV_TRADER_SOURCE: &str = include_str!("../src/mev_trader.rs");

fn fixture(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../mev-trader/tests/fixtures/priority_economics")
        .join(format!("beryl_two_hop_synthetic_reachability.{name}.json"));
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn decimal(value: &Value) -> U256 {
    U256::from_str_radix(value.as_str().unwrap(), 10).unwrap()
}

fn amount_out(amount_in: U256, reserve_in: U256, reserve_out: U256, fee_bps: u64) -> U256 {
    let scale = U256::from(10_000u64);
    let effective = amount_in * U256::from(10_000 - fee_bps);
    effective * reserve_out / (reserve_in * scale + effective)
}
fn kickback(gross: U256) -> U256 {
    (gross * U256::from(7_500u64) + U256::from(9_999u64)) / U256::from(10_000u64)
}
fn signed(value: U256) -> I512 {
    I512::from_raw(U512::from(value))
}

fn route_evidence(input: &Value, amount_in: U256) -> SelectedRouteEvidenceV2 {
    let pools = [
        input["route"]["pools"][0].as_str().unwrap().parse().unwrap(),
        input["route"]["pools"][1].as_str().unwrap().parse().unwrap(),
    ];
    let tokens = [
        input["route"]["tokens"][0].as_str().unwrap().parse().unwrap(),
        input["route"]["tokens"][1].as_str().unwrap().parse().unwrap(),
        input["route"]["tokens"][2].as_str().unwrap().parse().unwrap(),
    ];
    SelectedRouteEvidenceV2::new(
        input["victim"]["expectedHash"].as_str().unwrap().parse().unwrap(),
        B256::repeat_byte(1),
        pools,
        tokens,
        [B256::repeat_byte(2), B256::repeat_byte(3)],
        [
            input["route"]["feeBps"][0].as_u64().unwrap() as u32,
            input["route"]["feeBps"][1].as_u64().unwrap() as u32,
        ],
        [
            input["route"]["zeroForOne"][0].as_bool().unwrap(),
            input["route"]["zeroForOne"][1].as_bool().unwrap(),
        ],
        amount_in,
        B256::repeat_byte(4),
        B256::repeat_byte(5),
        B256::repeat_byte(6),
        B256::repeat_byte(7),
        B256::repeat_byte(8),
        input["canonicalShapeL1Fee"]["digest"].as_str().unwrap().parse().unwrap(),
    )
    .unwrap()
}

fn derive_golden() -> Value {
    let input = fixture("input");
    let setup = fixture("setup");
    let start = decimal(&input["route"]["startAmountWei"]);
    let victim_in = start / U256::from(input["victim"]["amountInDivisor"].as_u64().unwrap());
    let first = &setup["pools"][0];
    let second = &setup["pools"][1];
    let first_r0 = decimal(&first["reserve0"]);
    let first_r1 = decimal(&first["reserve1"]);
    let second_r0 = decimal(&second["reserve0"]);
    let second_r1 = decimal(&second["reserve1"]);
    let victim_out = amount_out(victim_in, first_r0, first_r1, first["feeBps"].as_u64().unwrap());
    let after_r0 = first_r0 + victim_in;
    let after_r1 = first_r1 - victim_out;
    let first_with = amount_out(start, after_r0, after_r1, first["feeBps"].as_u64().unwrap());
    let out_with = amount_out(first_with, second_r1, second_r0, second["feeBps"].as_u64().unwrap());
    let first_without = amount_out(start, first_r0, first_r1, first["feeBps"].as_u64().unwrap());
    let out_without =
        amount_out(first_without, second_r1, second_r0, second["feeBps"].as_u64().unwrap());
    let positive_gross = out_with - start;
    let positive_kickback = kickback(positive_gross);
    let positive_retained = positive_gross - positive_kickback;
    let positive_l2 = U256::from(50_000u64);
    let positive_l1 = decimal(&input["canonicalShapeL1Fee"]["feeWei"]);
    let positive_total = positive_l2 + positive_l1;
    let positive_ev = positive_retained - positive_total;
    let realistic_in =
        start / U256::from(input["realisticVariant"]["amountInDivisor"].as_u64().unwrap());
    let realistic_first =
        amount_out(realistic_in, after_r0, after_r1, first["feeBps"].as_u64().unwrap());
    let realistic_out =
        amount_out(realistic_first, second_r1, second_r0, second["feeBps"].as_u64().unwrap());
    let realistic_gross = realistic_out - realistic_in;
    let realistic_kickback = kickback(realistic_gross);
    let realistic_retained = realistic_gross - realistic_kickback;
    let gas = input["realisticVariant"]["actualGasUsed"].as_u64().unwrap();
    let l2 = U256::from(gas) * decimal(&input["header"]["realisticBaseFeePerGas"]);
    let l1 = decimal(&input["realisticVariant"]["l1FeeWei"]);
    let total = l2 + l1;
    let shortfall = total - realistic_retained + U256::from(1u64);

    json!({
        "schemaVersion": "base-mev/synthetic-reachability-golden/v1",
        "identity": "synthetic_reachability_golden",
        "claim": CLAIM,
        "fingerprint": input["fingerprint"],
        "victim": {
            "amountInWei": victim_in.to_string(),
            "amountOutRaw": victim_out.to_string(),
            "poolReserve0AfterWei": after_r0.to_string(),
            "poolReserve1AfterRaw": after_r1.to_string(),
            "mutatedPoolCount": 1
        },
        "candidateWithVictim": {
            "firstHopAmountOutRaw": first_with.to_string(),
            "amountOutWei": out_with.to_string(),
            "grossWei": positive_gross.to_string(),
            "kickbackWei": positive_kickback.to_string(),
            "retainedWei": positive_retained.to_string(),
            "actualGasUsed": 50000,
            "l2FeeWei": positive_l2.to_string(),
            "l1FeeWei": positive_l1.to_string(),
            "totalCostWei": positive_total.to_string(),
            "evWei": positive_ev.to_string(),
            "shortfallWei": "0",
            "terminal": "SelectedRouteEvPositive",
            "grossOptimismUnverified": true,
            "syntheticReachabilityOnly": true,
            "netRanked": false
        },
        "candidateWithoutVictim": {
            "firstHopAmountOutRaw": first_without.to_string(),
            "amountOutWei": out_without.to_string(),
            "grossWei": (out_without - start).to_string()
        },
        "realisticVariant": {
            "amountInWei": realistic_in.to_string(),
            "firstHopAmountOutRaw": realistic_first.to_string(),
            "amountOutWei": realistic_out.to_string(),
            "grossWei": realistic_gross.to_string(),
            "kickbackWei": realistic_kickback.to_string(),
            "retainedWei": realistic_retained.to_string(),
            "actualGasUsed": gas,
            "l2FeeWei": l2.to_string(),
            "l1FeeWei": l1.to_string(),
            "totalCostWei": total.to_string(),
            "evWei": format!("-{}", (total - realistic_retained)),
            "shortfallWei": shortfall.to_string(),
            "terminal": "SelectedRouteNoEdge",
            "grossOptimismUnverified": Value::Null,
            "syntheticReachabilityOnly": Value::Null,
            "netRanked": false
        },
        "intendedLive": {
            "terminal": "AuthorityUnavailable",
            "reason": "RequiredFeeUnavailable",
            "attempted": 1,
            "succeeded": 0,
            "failed": 1
        },
        "rawTransactionBytesPersisted": false,
        "signaturePersisted": false,
        "privateKeyPersisted": false
    })
}

fn manifest_creation(manifest: &Value, section: &str, name: &str) -> Vec<u8> {
    let entry =
        manifest[section].as_array().unwrap().iter().find(|entry| entry["name"] == name).unwrap();
    hex::decode(entry["creation_bytecode_hex"].as_str().unwrap()).unwrap()
}

fn push_address_word(encoded: &mut Vec<u8>, address: Address) {
    encoded.extend_from_slice(&[0u8; 12]);
    encoded.extend_from_slice(address.as_slice());
}

fn push_u256_word(encoded: &mut Vec<u8>, value: U256) {
    encoded.extend_from_slice(&value.to_be_bytes::<32>());
}

fn call_data(signature: &str, words: &[U256]) -> Vec<u8> {
    let mut encoded = alloy_primitives::keccak256(signature).as_slice()[..4].to_vec();
    for word in words {
        push_u256_word(&mut encoded, *word);
    }
    encoded
}

fn address_word(address: Address) -> U256 {
    U256::from_be_slice(address.as_slice())
}

fn execute_and_commit(
    db: &mut InMemoryDB,
    caller: Address,
    kind: TxKind,
    data: Vec<u8>,
) -> (Option<Address>, Vec<u8>) {
    let nonce = db.basic(caller).unwrap().map_or(0, |account| account.nonce);
    let mut cfg = CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Beryl));
    cfg.chain_id = 8453;
    let mut block = BlockEnv::default();
    block.gas_limit = 30_000_000;
    block.basefee = 0;
    let tx = BaseTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(caller)
                .chain_id(Some(8453))
                .kind(kind)
                .nonce(nonce)
                .data(data.into())
                .gas_price(0)
                .gas_priority_fee(None)
                .max_fee_per_gas(0)
                .gas_limit(12_000_000),
        )
        .build_fill();
    let mut evm =
        BaseContext::base().with_db(&mut *db).with_block(block).with_cfg(cfg).build_base();
    let output = evm.transact(tx).unwrap();
    drop(evm);
    assert!(output.result.is_success(), "sealed synthetic EVM transaction reverted");
    let created = output.result.created_address();
    let returned = output.result.output().unwrap().to_vec();
    DatabaseCommit::commit(db, output.state);
    (created, returned)
}

fn deploy_manifest_contract(
    db: &mut InMemoryDB,
    caller: Address,
    mut creation: Vec<u8>,
    constructor: &[u8],
) -> Address {
    creation.extend_from_slice(constructor);
    execute_and_commit(db, caller, TxKind::Create, creation).0.unwrap()
}

fn pool_quote_data(pool_token0: Address, amount_in: U256) -> Vec<u8> {
    let encoded =
        call_data("getAmountOut(uint256,address)", &[amount_in, address_word(pool_token0)]);
    assert_eq!(&encoded[..4], &[0xf1, 0x40, 0xa3, 0x5a]);
    encoded
}

fn pool_swap_data(amount_out: U256, recipient: Address) -> Vec<u8> {
    let encoded = call_data(
        "swap(uint256,uint256,address,bytes)",
        &[U256::ZERO, amount_out, address_word(recipient), U256::from(128u64), U256::ZERO],
    );
    assert_eq!(&encoded[..4], &[0x02, 0x2c, 0x0d, 0x9f]);
    encoded
}

fn synthetic_parent_from_r61(
    setup: &Value,
    victim_amount: U256,
) -> (InMemoryDB, Address, Address, Address) {
    let manifest: Value = serde_json::from_str(R61_MANIFEST).unwrap();
    assert_eq!(manifest["generated_from_commit"], "28889b15c25e2a04e29f187866901efb4c3f2b3a");
    assert_eq!(
        manifest["artifact_sha256"],
        "d4facc1b10da19cb1f820b15087dafff5f6e2abe2425738d5d0b05df2bf9a7c9"
    );

    let owner = Address::with_last_byte(0xa1);
    let mut db = InMemoryDB::default();
    db.insert_account_info(owner, AccountInfo::from_balance(U256::MAX));

    let token_creation = manifest_creation(&manifest, "dependency_fixtures", "MockERC20");
    let token0 = deploy_manifest_contract(&mut db, owner, token_creation.clone(), &[]);
    let token1 = deploy_manifest_contract(&mut db, owner, token_creation, &[]);
    let mut pool_constructor = Vec::with_capacity(96);
    push_address_word(&mut pool_constructor, token0);
    push_address_word(&mut pool_constructor, token1);
    push_u256_word(&mut pool_constructor, U256::ZERO);
    let pool = deploy_manifest_contract(
        &mut db,
        owner,
        manifest_creation(&manifest, "dependency_fixtures", "MockAerodromePool"),
        &pool_constructor,
    );

    let reserve0 = decimal(&setup["pools"][0]["reserve0"]);
    let reserve1 = decimal(&setup["pools"][0]["reserve1"]);
    execute_and_commit(
        &mut db,
        owner,
        TxKind::Call(pool),
        call_data("setReserves(uint256,uint256)", &[reserve0, reserve1]),
    );
    execute_and_commit(
        &mut db,
        owner,
        TxKind::Call(token0),
        call_data("mint(address,uint256)", &[address_word(pool), victim_amount]),
    );
    (db, owner, token0, pool)
}

fn execute_candidate_branch(
    parent: &InMemoryDB,
    owner: Address,
    token0: Address,
    pool: Address,
    victim_amount_out: Option<U256>,
    candidate_amount: U256,
) -> (U256, usize, usize) {
    let mut branch = parent.clone();
    let mut victim_transacts_and_commits = 0;
    if let Some(amount_out) = victim_amount_out {
        let (_, victim_output) = execute_and_commit(
            &mut branch,
            owner,
            TxKind::Call(pool),
            pool_swap_data(amount_out, owner),
        );
        assert!(victim_output.is_empty());
        victim_transacts_and_commits += 1;
    }
    let (_, candidate_output) = execute_and_commit(
        &mut branch,
        owner,
        TxKind::Call(pool),
        pool_quote_data(token0, candidate_amount),
    );
    let candidate_transacts_and_commits = 1;
    assert_eq!(candidate_output.len(), 32);
    (
        U256::from_be_slice(&candidate_output),
        victim_transacts_and_commits,
        candidate_transacts_and_commits,
    )
}

#[derive(Clone, Debug)]
struct SpyDb {
    reads: Rc<Cell<u64>>,
    commits: Rc<Cell<u64>>,
    code_len: usize,
}

impl Default for SpyDb {
    fn default() -> Self {
        Self { reads: Rc::new(Cell::new(0)), commits: Rc::new(Cell::new(0)), code_len: 0 }
    }
}

impl Database for SpyDb {
    type Error = Infallible;

    fn basic(&mut self, _address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.reads.set(self.reads.get() + 1);
        Ok(Some(AccountInfo::default()))
    }

    fn code_by_hash(&mut self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.reads.set(self.reads.get() + 1);
        Ok(Bytecode::new_raw(vec![0x5b; self.code_len].into()))
    }

    fn storage(
        &mut self,
        _address: Address,
        _index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        self.reads.set(self.reads.get() + 1);
        Ok(U256::ZERO)
    }

    fn storage_by_account_id(
        &mut self,
        _address: Address,
        _account_id: AccountId,
        _storage_key: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        self.reads.set(self.reads.get() + 1);
        Ok(U256::ZERO)
    }

    fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
        self.reads.set(self.reads.get() + 1);
        Ok(B256::ZERO)
    }
}

impl DatabaseCommit for SpyDb {
    fn commit(&mut self, _changes: AddressMap<Account>) {
        self.commits.set(self.commits.get() + 1);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TestDbError {
    fatal: bool,
}

impl fmt::Display for TestDbError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("raw inner detail must not enter evidence")
    }
}

impl Error for TestDbError {}

impl DBErrorMarker for TestDbError {
    fn is_fatal(&self) -> bool {
        self.fatal
    }
}

#[derive(Debug)]
struct FailingDb {
    error: TestDbError,
}

impl Database for FailingDb {
    type Error = TestDbError;

    fn basic(&mut self, _address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        Err(self.error)
    }

    fn code_by_hash(&mut self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
        Err(self.error)
    }

    fn storage(
        &mut self,
        _address: Address,
        _index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        Err(self.error)
    }

    fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
        Err(self.error)
    }
}

fn address_from_index(index: u64) -> Address {
    let mut bytes = [0u8; 20];
    bytes[12..].copy_from_slice(&index.to_be_bytes());
    Address::from(bytes)
}

fn production_cardinality_is_exact(source: &str) -> bool {
    [
        "cardinality.record_adapter_entry()?;",
        "cardinality.record_victim_commit()?;",
        "cardinality.record_evm_transact()?;",
        "cardinality.record_candidate_commit()?;",
        "let _cardinality = cardinality.checked()?;",
    ]
    .into_iter()
    .all(|marker| source.matches(marker).count() == 1)
}

fn complete_l1_fetch(db: &mut AuditedDatabase<SpyDb>) {
    db.basic(Predeploys::L1_BLOCK_INFO).unwrap();
    db.storage(Predeploys::L1_BLOCK_INFO, L1BlockInfo::L1_BASE_FEE_SLOT).unwrap();
    db.storage(Predeploys::L1_BLOCK_INFO, L1BlockInfo::ECOTONE_L1_BLOB_BASE_FEE_SLOT).unwrap();
    db.storage(Predeploys::L1_BLOCK_INFO, L1BlockInfo::ECOTONE_L1_FEE_SCALARS_SLOT).unwrap();
    db.storage(Predeploys::L1_BLOCK_INFO, L1BlockInfo::L1_OVERHEAD_SLOT).unwrap();
    db.storage(Predeploys::L1_BLOCK_INFO, L1BlockInfo::OPERATOR_FEE_SCALARS_SLOT).unwrap();
    db.storage(Predeploys::L1_BLOCK_INFO, L1BlockInfo::DA_FOOTPRINT_GAS_SCALAR_SLOT).unwrap();
}

#[test]
fn audit_phase_dfa_accepts_only_the_exact_path() {
    let weth = Address::with_last_byte(0xee);
    let slot = U256::from(9);
    let mut db = AuditedDatabase::new(SpyDb::default());
    assert_eq!(db.phase(), AuditPhase::PreWeth);
    db.storage(weth, slot).unwrap();
    db.transition::<Infallible>(AuditPhase::Candidate).unwrap();
    db.basic(Address::with_last_byte(1)).unwrap();
    db.transition::<Infallible>(AuditPhase::PostWeth).unwrap();
    db.storage(weth, slot).unwrap();
    db.transition::<Infallible>(AuditPhase::L1Fetch).unwrap();
    complete_l1_fetch(&mut db);
    db.transition::<Infallible>(AuditPhase::Sealed).unwrap();
    assert!(matches!(
        db.basic(Address::ZERO),
        Err(AuditedDatabaseError::PhaseIncomplete { phase: AuditPhase::Sealed, .. })
    ));
    assert!(db.into_sealed_parts::<Infallible>().is_ok());

    let mut duplicate_weth = AuditedDatabase::new(SpyDb::default());
    duplicate_weth.storage(weth, slot).unwrap();
    duplicate_weth.storage(weth, slot).unwrap();
    assert!(matches!(
        duplicate_weth.transition::<Infallible>(AuditPhase::Candidate),
        Err(AuditedDatabaseError::PhaseIncomplete { .. })
    ));
}

#[test]
fn audit_phase_dfa_rejects_skips_and_reentry() {
    let mut db = AuditedDatabase::new(SpyDb::default());
    assert!(matches!(
        db.transition::<Infallible>(AuditPhase::Candidate),
        Err(AuditedDatabaseError::PhaseIncomplete { phase: AuditPhase::PreWeth, .. })
    ));
    assert!(matches!(
        db.transition::<Infallible>(AuditPhase::PostWeth),
        Err(AuditedDatabaseError::InvalidPhaseTransition { .. })
    ));
    db.storage(Address::ZERO, U256::ZERO).unwrap();
    db.transition::<Infallible>(AuditPhase::Candidate).unwrap();
    assert!(matches!(
        db.transition::<Infallible>(AuditPhase::Candidate),
        Err(AuditedDatabaseError::InvalidPhaseTransition { .. })
    ));
    db.basic(Address::ZERO).unwrap();
    db.transition::<Infallible>(AuditPhase::PostWeth).unwrap();
    db.storage(Address::ZERO, U256::ZERO).unwrap();
    db.transition::<Infallible>(AuditPhase::L1Fetch).unwrap();
    assert!(matches!(
        db.transition::<Infallible>(AuditPhase::Sealed),
        Err(AuditedDatabaseError::PhaseIncomplete { phase: AuditPhase::L1Fetch, .. })
    ));

    complete_l1_fetch(&mut db);
    db.storage(Predeploys::L1_BLOCK_INFO, L1BlockInfo::L1_BASE_FEE_SLOT).unwrap();
    assert!(matches!(
        db.transition::<Infallible>(AuditPhase::Sealed),
        Err(AuditedDatabaseError::PhaseIncomplete { phase: AuditPhase::L1Fetch, .. })
    ));
}

#[test]
fn audited_database_records_all_five_locked_read_methods() {
    let mut db = AuditedDatabase::new(SpyDb::default());
    db.basic(Address::ZERO).unwrap();
    db.code_by_hash(B256::ZERO).unwrap();
    db.storage(Address::ZERO, U256::ZERO).unwrap();
    db.storage_by_account_id(Address::ZERO, AccountId::new(0).unwrap(), U256::ZERO).unwrap();
    db.block_hash(0).unwrap();
    assert_eq!(db.accesses().len(), 5);
    assert_eq!(db.accesses()[0].ordinal(), 0);
    assert_eq!(db.accesses()[4].ordinal(), 4);

    assert!(MEV_TRADER_SOURCE.contains("const PRODUCTION_TOTAL_ACCESSES: usize = 16_384;"));
    assert!(MEV_TRADER_SOURCE.contains("const PRODUCTION_UNIQUE_ACCOUNTS: usize = 256;"));
    assert!(MEV_TRADER_SOURCE.contains("const PRODUCTION_UNIQUE_STORAGE_KEYS: usize = 8_192;"));
    assert!(
        MEV_TRADER_SOURCE
            .contains("const PRODUCTION_AGGREGATE_CODE_BYTES: usize = 4 * 1024 * 1024;")
    );
    assert!(MEV_TRADER_SOURCE.contains("const PRODUCTION_BLOCK_HASH_KEYS: usize = 256;"));

    let mut total = AuditedDatabase::new(SpyDb::default());
    for _ in 0..16_384 {
        total.basic(Address::ZERO).unwrap();
    }
    assert!(matches!(total.basic(Address::ZERO), Err(AuditedDatabaseError::ResourceLimit { .. })));

    let mut accounts = AuditedDatabase::new(SpyDb::default());
    for index in 0..256 {
        accounts.basic(address_from_index(index)).unwrap();
    }
    assert!(matches!(
        accounts.basic(address_from_index(256)),
        Err(AuditedDatabaseError::ResourceLimit { .. })
    ));

    let mut storage = AuditedDatabase::new(SpyDb::default());
    for slot in 0u64..8_192 {
        storage.storage(Address::ZERO, U256::from(slot)).unwrap();
    }
    assert!(matches!(
        storage.storage(Address::ZERO, U256::from(8_192)),
        Err(AuditedDatabaseError::ResourceLimit { .. })
    ));

    let mut blocks = AuditedDatabase::new(SpyDb::default());
    for number in 0..256 {
        blocks.block_hash(number).unwrap();
    }
    assert!(matches!(blocks.block_hash(256), Err(AuditedDatabaseError::ResourceLimit { .. })));

    let code_db = SpyDb { code_len: 2_097_153, ..SpyDb::default() };
    let mut code = AuditedDatabase::new(code_db);
    code.code_by_hash(B256::ZERO).unwrap();
    assert!(matches!(
        code.code_by_hash(B256::ZERO),
        Err(AuditedDatabaseError::ResourceLimit { .. })
    ));

    let ordinal_update = "ordinal.checked_add(1).ok_or(AuditedDatabaseError::OrdinalOverflow)?";
    assert_eq!(MEV_TRADER_SOURCE.matches(ordinal_update).count(), 1);
}

#[test]
fn candidate_blockhash_fails_before_delegate_or_log() {
    let inner = SpyDb::default();
    let reads = Rc::clone(&inner.reads);
    let mut db = AuditedDatabase::new(inner);
    db.storage(Address::ZERO, U256::ZERO).unwrap();
    db.transition::<Infallible>(AuditPhase::Candidate).unwrap();
    let reads_before = reads.get();
    assert!(matches!(
        db.block_hash(7),
        Err(AuditedDatabaseError::CandidateBlockHashForbidden { number: 7 })
    ));
    assert_eq!(reads.get(), reads_before);
    assert_eq!(db.accesses().len(), 1);

    let mut failing = AuditedDatabase::new(FailingDb { error: TestDbError { fatal: false } });
    assert!(matches!(
        failing.basic(Address::with_last_byte(7)),
        Err(AuditedDatabaseError::Inner(TestDbError { fatal: false }))
    ));
    assert_eq!(failing.failure_count(), 1);
    assert_eq!(failing.failure_phase(0), Some(AuditPhase::PreWeth));
    assert_eq!(failing.failure_ordinal(0), Some(0));
    assert_eq!(failing.failure_operation_tag(0), Some("basic"));
    assert_eq!(failing.failure_is_fatal(0), Some(false));
    assert!(matches!(
        failing.code_by_hash(B256::with_last_byte(8)),
        Err(AuditedDatabaseError::Inner(TestDbError { fatal: false }))
    ));
    assert_eq!(failing.failure_ordinal(1), Some(1));
    assert_eq!(failing.failure_operation_tag(1), Some("code_by_hash"));

    let mut fatal = AuditedDatabase::new(FailingDb { error: TestDbError { fatal: true } });
    assert!(fatal.storage(Address::ZERO, U256::ZERO).is_err());
    assert_eq!(fatal.failure_is_fatal(0), Some(true));
    assert!(failing.accesses().is_empty());
}

#[test]
fn audited_database_delegates_candidate_commit_exactly_once() {
    let inner = SpyDb::default();
    let commits = Rc::clone(&inner.commits);
    let mut db = AuditedDatabase::new(inner);
    db.commit(AddressMap::default());
    assert_eq!(commits.get(), 1);
}

#[test]
fn allowlist_distinguishes_account_balance_nonce_and_storage_authority() {
    let account = Address::with_last_byte(1);
    let storage_owner = Address::with_last_byte(2);
    let slot = U256::from(3);
    let allowlist = CandidateAccessAllowlistV1::new(
        [account, storage_owner],
        [account],
        [storage_owner],
        [storage_owner],
    );
    assert!(allowlist.allows_account(account));
    assert!(allowlist.allows_balance(account));
    assert!(!allowlist.allows_nonce(account));
    assert!(allowlist.allows_storage(storage_owner, slot));
    assert!(!allowlist.allows_storage(account, slot));
}

#[test]
fn hydration_db_zero_one_and_access_union_are_preserved() {
    let address = Address::with_last_byte(1);
    let slot = U256::from(2);
    let code_hash = B256::with_last_byte(3);
    let state = CandidateAccessedStateV1::new([address], [(address, slot)], [code_hash]);
    assert_eq!(state.accounts().len(), 1);
    assert_eq!(state.storage().len(), 1);
    assert_eq!(state.code_hashes().len(), 1);
    let inner = SpyDb::default();
    let reads = Rc::clone(&inner.reads);
    let mut audited = AuditedDatabase::new(inner);
    assert_eq!(reads.get(), 0, "empty-code hydration must not query code DB");
    audited.code_by_hash(code_hash).unwrap();
    assert_eq!(reads.get(), 1, "non-empty hydration must query code DB exactly once");
}

#[test]
fn victim_overlay_preserves_absent_and_existing_empty_inequality() {
    let absent = Account::new_not_existing(TransactionId::ZERO);
    let existing = Account::from(AccountInfo::default());
    assert_ne!(absent.status, existing.status);
}

#[test]
fn synthetic_reachability_matches_golden_and_recomputes_route_and_conservation() {
    let input = fixture("input");
    let setup = fixture("setup");
    let sources = fixture("sources");
    let golden = fixture("golden");
    assert_eq!(derive_golden(), golden);
    assert_eq!(input["identity"], "synthetic_reachability_golden");
    assert_eq!(input["claim"], CLAIM);
    assert_eq!(golden["claim"], CLAIM);
    assert_eq!(setup["poolUniverse"].as_array().unwrap().len(), 2);
    assert_eq!(golden["victim"]["mutatedPoolCount"], 1);
    assert_ne!(
        golden["candidateWithVictim"]["amountOutWei"],
        golden["candidateWithoutVictim"]["amountOutWei"]
    );
    let victim_amount = decimal(&golden["victim"]["amountInWei"]);
    let victim_amount_out = decimal(&golden["victim"]["amountOutRaw"]);
    let (parent, owner, token0, first_pool) = synthetic_parent_from_r61(&setup, victim_amount);
    let candidate_amount = decimal(&input["route"]["startAmountWei"]);
    let (amount_out_a, victim_a, candidate_a) = execute_candidate_branch(
        &parent,
        owner,
        token0,
        first_pool,
        Some(victim_amount_out),
        candidate_amount,
    );
    let (amount_out_b, victim_b, candidate_b) =
        execute_candidate_branch(&parent, owner, token0, first_pool, None, candidate_amount);
    assert_eq!((victim_a, candidate_a), (1, 1));
    assert_eq!((victim_b, candidate_b), (0, 1));
    assert_eq!(amount_out_a.to_string(), golden["candidateWithVictim"]["firstHopAmountOutRaw"]);
    assert_eq!(amount_out_b.to_string(), golden["candidateWithoutVictim"]["firstHopAmountOutRaw"]);
    assert_ne!(amount_out_a, amount_out_b);
    assert_eq!(input["claim"], CLAIM);
    assert_eq!(golden["candidateWithVictim"]["grossOptimismUnverified"], true);
    assert_eq!(golden["candidateWithVictim"]["syntheticReachabilityOnly"], true);

    for source in sources["sources"].as_array().unwrap() {
        assert_eq!(source["commit"].as_str().unwrap().len(), 40);
        assert_eq!(source["blob"].as_str().unwrap().len(), 40);
        assert!(source["repo"].as_str().unwrap().starts_with("https://github.com/"));
        assert!(!source["path"].as_str().unwrap().is_empty());
    }
    let upstream_l1 = sources["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["id"] == "upstream-l1-authority")
        .unwrap();
    assert_eq!(upstream_l1["repo"], "https://github.com/simjaemun2/base");
    assert_eq!(upstream_l1["commit"], "15ebc01d64d25b37a5c83226e0ca47a3267ef6d8");
    assert_eq!(upstream_l1["blob"], "eda082f8278b64052e41d346f54309c072c0b166");
    assert_eq!(upstream_l1["path"], "crates/common/evm/src/l1block.rs");
    assert_eq!(upstream_l1["lines"], "75-86,139-191,298,328-403,657-690");
    assert_eq!(
        upstream_l1["functions"],
        json!([
            "L1BlockInfo::try_fetch",
            "L1BlockInfo::clear_tx_l1_cost",
            "L1BlockInfo::calculate_tx_l1_cost"
        ])
    );
    let serialized = format!("{input}{setup}{golden}");
    for forbidden in
        ["\"rawTx\":", "\"privateKey\":", "\"signatureBytes\":", "\"rpcUrl\":", "\"credential\":"]
    {
        assert!(!serialized.contains(forbidden));
    }

    let amount_in = decimal(&input["route"]["startAmountWei"]);
    let amount_out = decimal(&golden["candidateWithVictim"]["amountOutWei"]);
    let gross = amount_out - amount_in;
    let kickback = decimal(&golden["candidateWithVictim"]["kickbackWei"]);
    let retained = decimal(&golden["candidateWithVictim"]["retainedWei"]);
    assert_eq!(kickback + retained, gross);
    let l1 = CanonicalL1FeeEvidenceV2::new(
        input["canonicalShapeL1Fee"]["length"].as_u64().unwrap(),
        input["canonicalShapeL1Fee"]["zeroBytes"].as_u64().unwrap(),
        input["canonicalShapeL1Fee"]["nonZeroBytes"].as_u64().unwrap(),
        input["canonicalShapeL1Fee"]["fastLzSize"].as_u64().unwrap(),
        input["canonicalShapeL1Fee"]["digest"].as_str().unwrap().parse().unwrap(),
        decimal(&input["canonicalShapeL1Fee"]["feeWei"]),
    )
    .unwrap();
    let l2 = decimal(&golden["candidateWithVictim"]["l2FeeWei"]);
    let total = l2 + l1.fee();
    let ev = signed(retained).checked_sub(signed(total)).unwrap();
    let record = PriorityEconomicsV2::evaluated(
        route_evidence(&input, amount_in),
        amount_out,
        signed(gross),
        kickback,
        retained,
        golden["candidateWithVictim"]["actualGasUsed"].as_u64().unwrap(),
        l2,
        l1,
        total,
        ev,
        U256::ZERO,
        Some(true),
        PriorityEconomicsCountersV2::new(1, 1, 1, 1, 1, 1, 1, 0).unwrap(),
    )
    .unwrap();
    assert_eq!(record.terminal(), AdmissionTerminalV2::SelectedRouteEvPositive);
    assert_eq!(record.synthetic_reachability_only(), Some(true));
    assert!(!record.net_ranked());
    assert_eq!(serde_json::to_value(record).unwrap()["grossOptimismUnverified"], true);
}

#[test]
fn realistic_fee_no_edge_and_intended_live_unavailable_match_contract() {
    let input = fixture("input");
    let golden = fixture("golden");
    let realistic = &golden["realisticVariant"];
    let amount_in = decimal(&realistic["amountInWei"]);
    let amount_out = decimal(&realistic["amountOutWei"]);
    let gross = decimal(&realistic["grossWei"]);
    let kickback = decimal(&realistic["kickbackWei"]);
    let retained = decimal(&realistic["retainedWei"]);
    let l2 = decimal(&realistic["l2FeeWei"]);
    let l1 = CanonicalL1FeeEvidenceV2::new(
        input["canonicalShapeL1Fee"]["length"].as_u64().unwrap(),
        input["canonicalShapeL1Fee"]["zeroBytes"].as_u64().unwrap(),
        input["canonicalShapeL1Fee"]["nonZeroBytes"].as_u64().unwrap(),
        input["canonicalShapeL1Fee"]["fastLzSize"].as_u64().unwrap(),
        input["canonicalShapeL1Fee"]["digest"].as_str().unwrap().parse().unwrap(),
        decimal(&realistic["l1FeeWei"]),
    )
    .unwrap();
    let total = l2 + l1.fee();
    let ev = signed(retained).checked_sub(signed(total)).unwrap();
    let record = PriorityEconomicsV2::evaluated(
        route_evidence(&input, amount_in),
        amount_out,
        signed(gross),
        kickback,
        retained,
        realistic["actualGasUsed"].as_u64().unwrap(),
        l2,
        l1,
        total,
        ev,
        decimal(&realistic["shortfallWei"]),
        None,
        PriorityEconomicsCountersV2::new(1, 1, 1, 1, 1, 1, 1, 0).unwrap(),
    )
    .unwrap();
    assert_eq!(record.terminal(), AdmissionTerminalV2::SelectedRouteNoEdge);
    assert_eq!(record.ev_wei(), Some(ev));
    assert_eq!(record.synthetic_reachability_only(), None);
    assert!(!record.net_ranked());

    let failed = AttemptedAuthorityUnavailableV2::new(
        AttemptedAuthorityUnavailableReasonV2::RequiredFeeUnavailable,
        B256::repeat_byte(9),
        None,
        None,
        None,
        None,
    )
    .unwrap();
    let unavailable = PriorityEconomicsV2::authority_unavailable(
        AdmissionStageV2::AuthorityAttempted,
        AuthorityUnavailableV2::Attempted(failed),
        Some(route_evidence(&input, amount_in)),
        Some(signed(gross)),
        PriorityEconomicsCountersV2::new(1, 1, 1, 1, 1, 1, 0, 1).unwrap(),
    )
    .unwrap();
    assert_eq!(unavailable.terminal(), AdmissionTerminalV2::AuthorityUnavailable);
    assert_eq!(golden["intendedLive"]["terminal"], "AuthorityUnavailable");

    let _: Option<CandidateExecutionCardinalityV1> = None;
    let execute = MEV_TRADER_SOURCE
        .split_once("fn execute_candidate(")
        .unwrap()
        .1
        .split_once("struct CliSnapshotFreshness")
        .unwrap()
        .0;
    assert!(
        execute
            .contains("let mut cardinality = CandidateExecutionCardinalityTrackerV1::default();")
    );
    assert!(production_cardinality_is_exact(execute));
    for marker in [
        "cardinality.record_adapter_entry()?;",
        "cardinality.record_victim_commit()?;",
        "cardinality.record_evm_transact()?;",
        "cardinality.record_candidate_commit()?;",
    ] {
        assert!(!production_cardinality_is_exact(&execute.replacen(marker, "", 1)));
        assert!(!production_cardinality_is_exact(&execute.replacen(
            marker,
            &format!("{marker}\n{marker}"),
            1
        )));
    }
}

#[test]
#[ignore = "reviewer-local golden updater; excluded from normal CI and aggregate"]
fn update_synthetic_reachability_golden() {
    assert_eq!(
        std::env::var("UPDATE_PRIORITY_ECONOMICS_GOLDEN").as_deref(),
        Ok("1"),
        "the updater is disabled without the explicit reviewer-local gate"
    );
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../mev-trader/tests/fixtures/priority_economics")
        .join("beryl_two_hop_synthetic_reachability.golden.json");
    let bytes = serde_json::to_vec_pretty(&derive_golden()).unwrap();
    fs::write(path, [bytes.as_slice(), b"\n"].concat()).unwrap();
}
