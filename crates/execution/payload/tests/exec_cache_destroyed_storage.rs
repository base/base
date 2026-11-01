//! Execution-cache (reth `ExecutionCache`) consistency for EIP-8130 accounts.
//!
//! An EIP-8130 transaction with a non-zero nonce key leaves the protocol nonce at
//! zero, so an account can write storage while delegated, clear the delegation,
//! and then be drained until it is empty. EIP-161 destroys it and wipes that
//! storage. Each block here runs on a fresh `State` over reth's real
//! `CachedStateProvider`, and the shared cache is updated with the real
//! `insert_state`, the same way the engine carries its saved cache across blocks.
//! The cache-backed run must match a run without a cache.

use alloy_eips::eip2718::Encodable2718;
use alloy_evm::{Evm, FromTxWithEncoded};
use alloy_primitives::{Address, B256, Bytes, U256, address, bytes, keccak256, map::HashMap};
use base_common_consensus::{
    AccountChange, BaseTxEnvelope, Call, Delegation, Eip8130Constants, Eip8130Signed, TxEip8130,
};
use base_common_evm::{BaseSpecId, BaseTransaction, BaseUpgrade, Builder, DefaultBase};
use k256::ecdsa::SigningKey;
use reth_execution_cache::{CachedStateProvider, ExecutionCache};
use reth_primitives_traits::Account;
use reth_revm::{database::StateProviderDatabase, test_utils::StateProviderTest};
use reth_storage_api::EvmStateProviderAdapter;
use revm::{
    DatabaseCommit,
    context::{BlockEnv, CfgEnv, Context},
    database::{BundleState, State, states::bundle_state::BundleRetention},
    inspector::NoOpInspector,
};

const CHAIN_ID: u64 = 8453;
const BASE_FEE: u64 = 1_000_000_000;
const BENEFICIARY: Address = address!("0x00000000000000000000000000000000000000bb");
const RECIPIENT: Address = address!("0x00000000000000000000000000000000000000cc");
/// `SSTORE(1, 7)`.
const WRITER: Address = address!("0x00000000000000000000000000000000000000d1");
/// `SSTORE(2, SLOAD(1))`.
const PROBE: Address = address!("0x00000000000000000000000000000000000000d2");

fn key(byte: u8) -> SigningKey {
    SigningKey::from_slice(&[byte; 32]).unwrap()
}

fn addr(key: &SigningKey) -> Address {
    let point = key.verifying_key().to_encoded_point(false);
    Address::from_slice(&keccak256(&point.as_bytes()[1..])[12..])
}

fn sig(key: &SigningKey, hash: B256) -> Bytes {
    let (signature, recid) = key.sign_prehash_recoverable(hash.as_slice()).unwrap();
    let mut out = vec![0u8; 65];
    out[..64].copy_from_slice(&signature.to_bytes());
    out[64] = recid.to_byte() + 27;
    Bytes::from(out)
}

fn tx(nonce_key: u64, nonce_sequence: u64) -> TxEip8130 {
    TxEip8130 {
        chain_id: CHAIN_ID,
        sender: None,
        nonce_key: U256::from(nonce_key),
        nonce_sequence,
        valid_after: 0,
        valid_before: 0,
        max_priority_fee_per_gas: 0,
        max_fee_per_gas: 2_000_000_000,
        gas_limit: 500_000,
        account_changes: Vec::new(),
        calls: Vec::new(),
        metadata: Bytes::new(),
        payer: None,
    }
}

fn self_pay(tx: TxEip8130, sender: &SigningKey) -> Eip8130Signed {
    let auth = sig(sender, tx.sender_signature_hash());
    Eip8130Signed::new(tx, auth, Bytes::new())
}

fn open_pay(mut tx: TxEip8130, sender: &SigningKey, payer: &SigningKey) -> Eip8130Signed {
    tx.payer = Some(Eip8130Constants::OPEN_PAYER);
    let sender_auth = sig(sender, tx.sender_signature_hash());
    let payer_auth = sig(payer, tx.payer_signature_hash(addr(sender)));
    Eip8130Signed::new(tx, sender_auth, payer_auth)
}

/// One account of the canonical post-state.
#[derive(Default, Clone)]
struct CanonicalAccount {
    account: Account,
    code: Option<Bytes>,
    storage: HashMap<B256, U256>,
}

/// Canonical post-state, rebuilt into a fresh provider for every block.
#[derive(Default, Clone)]
struct Canonical(HashMap<Address, CanonicalAccount>);

impl Canonical {
    fn provider(&self) -> StateProviderTest {
        let mut p = StateProviderTest::default();
        for (a, acc) in &self.0 {
            p.insert_account(*a, acc.account, acc.code.clone(), acc.storage.clone());
        }
        p
    }

    fn apply(&mut self, bundle: &BundleState) {
        for (a, acc) in &bundle.state {
            if acc.was_destroyed() {
                self.0.remove(a);
            }
            let Some(info) = &acc.info else { continue };
            let entry = self.0.entry(*a).or_default();
            entry.account = Account {
                nonce: info.nonce,
                balance: info.balance,
                bytecode_hash: (!info.is_empty_code_hash()).then_some(info.code_hash),
            };
            entry.code = if info.is_empty_code_hash() {
                None
            } else {
                Some(
                    info.code
                        .clone()
                        .or_else(|| bundle.contracts.get(&info.code_hash).cloned())
                        .expect("code")
                        .original_bytes(),
                )
            };
            for (k, slot) in &acc.storage {
                let k = B256::from(*k);
                if slot.present_value.is_zero() {
                    entry.storage.remove(&k);
                } else {
                    entry.storage.insert(k, slot.present_value);
                }
            }
        }
    }

    fn storage(&self, a: Address, k: u64) -> U256 {
        self.0
            .get(&a)
            .and_then(|e| e.storage.get(&B256::from(U256::from(k))).copied())
            .unwrap_or_default()
    }

    fn balance(&self, a: Address) -> U256 {
        self.0.get(&a).map(|e| e.account.balance).unwrap_or_default()
    }
}

/// Executes `txs` as one block against `canonical`, optionally reading through
/// `cache`, then updates the cache with the real `insert_state` and returns the
/// block's bundle.
fn run_block(
    number: u64,
    canonical: &Canonical,
    cache: Option<&ExecutionCache>,
    txs: &[Eip8130Signed],
) -> BundleState {
    let provider = canonical.provider();
    let Some(cache) = cache else {
        return execute(
            number,
            StateProviderDatabase::new(EvmStateProviderAdapter(&provider)),
            txs,
        );
    };
    let cached = CachedStateProvider::new(EvmStateProviderAdapter(&provider), cache.clone(), None);
    let bundle = execute(number, StateProviderDatabase::new(cached), txs);
    cache.insert_state(&bundle).expect("insert_state");
    bundle
}

fn execute<DB: revm::Database + std::fmt::Debug>(
    number: u64,
    db: DB,
    txs: &[Eip8130Signed],
) -> BundleState
where
    DB::Error: Send + Sync + 'static,
{
    let mut state = State::builder().with_database(db).with_bundle_update().build();
    {
        let mut evm = Context::base()
            .with_db(&mut state)
            .with_cfg(
                CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Everest))
                    .with_chain_id(CHAIN_ID),
            )
            .with_block(BlockEnv {
                number: U256::from(number),
                timestamp: U256::from(1_000 + number),
                basefee: BASE_FEE,
                beneficiary: BENEFICIARY,
                ..Default::default()
            })
            .build_with_inspector(NoOpInspector);
        for signed in txs {
            let envelope = BaseTxEnvelope::Eip8130(signed.clone());
            let encoded: Bytes = envelope.encoded_2718().into();
            let base_tx = BaseTransaction::from_encoded_tx(&envelope, Address::ZERO, encoded);
            let out = evm.transact_raw(base_tx).expect("tx must be valid");
            assert!(out.result.is_success(), "block {number}: {:?}", out.result);
            evm.db_mut().commit(out.state);
        }
    }
    state.merge_transitions(BundleRetention::Reverts);
    state.take_bundle()
}

fn scenario(cache: Option<&ExecutionCache>) -> (Canonical, Vec<BundleState>) {
    let x_key = key(0x51);
    let payer_key = key(0x52);
    let x = addr(&x_key);
    let payer = addr(&payer_key);
    let eth = U256::from(10u64).pow(U256::from(18u64));

    let funded = |balance| CanonicalAccount {
        account: Account { balance, ..Default::default() },
        ..Default::default()
    };
    let mut canonical = Canonical::default();
    canonical.0.insert(x, funded(eth));
    canonical.0.insert(payer, funded(eth * U256::from(10)));
    for (a, code) in [(WRITER, bytes!("600760015500")), (PROBE, bytes!("60015460025500"))] {
        let account = Account { bytecode_hash: Some(keccak256(&code)), ..Default::default() };
        canonical.0.insert(a, CanonicalAccount { account, code: Some(code), ..Default::default() });
    }

    let mut bundles = Vec::new();
    let mut step = |n: u64, canonical: &mut Canonical, txs: Vec<Eip8130Signed>| {
        let b = run_block(n, canonical, cache, &txs);
        canonical.apply(&b);
        bundles.push(b);
    };

    // Block 1: X (nonce key 1) delegates to WRITER and calls itself: slot 1 = 7.
    let mut t = tx(1, 0);
    t.account_changes = vec![AccountChange::Delegation(Delegation { target: WRITER })];
    t.calls = vec![vec![Call { to: x, value: U256::ZERO, data: Bytes::new() }]];
    step(1, &mut canonical, vec![self_pay(t, &x_key)]);

    // Block 2: X (nonce key 2) clears its delegation; open payer pays gas.
    let mut t = tx(2, 0);
    t.account_changes = vec![AccountChange::Delegation(Delegation { target: Address::ZERO })];
    step(2, &mut canonical, vec![open_pay(t, &x_key, &payer_key)]);

    // Block 3: X (nonce key 3) sends its whole balance away; open payer pays gas.
    let mut t = tx(3, 0);
    t.calls = vec![vec![Call { to: RECIPIENT, value: canonical.balance(x), data: Bytes::new() }]];
    step(3, &mut canonical, vec![open_pay(t, &x_key, &payer_key)]);

    // Block 4: payer re-funds X.
    let payer_nonce = canonical.0[&payer].account.nonce;
    let mut t = tx(0, payer_nonce);
    t.calls = vec![vec![Call { to: x, value: eth, data: Bytes::new() }]];
    step(4, &mut canonical, vec![self_pay(t, &payer_key)]);

    // Block 5: X (nonce key 4) delegates to PROBE and calls itself: slot 2 = SLOAD(1).
    let mut t = tx(4, 0);
    t.account_changes = vec![AccountChange::Delegation(Delegation { target: PROBE })];
    t.calls = vec![vec![Call { to: x, value: U256::ZERO, data: Bytes::new() }]];
    step(5, &mut canonical, vec![self_pay(t, &x_key)]);

    (canonical, bundles)
}

#[test]
fn destroyed_codeless_account_storage_is_not_served_from_cache() {
    let x = addr(&key(0x51));

    let (control, control_bundles) = scenario(None);
    let written = &control_bundles[0].state[&x];
    assert_eq!(written.info.as_ref().map(|i| i.nonce), Some(0), "nonce-key txs keep nonce 0");
    assert_eq!(written.storage[&U256::from(1)].present_value, U256::from(7));
    let destroyed = &control_bundles[2].state[&x];
    assert!(destroyed.was_destroyed() && destroyed.info.is_none(), "EIP-161 destroys X");
    assert!(
        destroyed.original_info.as_ref().is_some_and(|i| i.is_empty_code_hash()),
        "X is codeless at the start of the destroying block"
    );
    assert_eq!(control.storage(x, 2), U256::ZERO, "uncached probe reads the wiped slot");

    let cache = ExecutionCache::new(1_000_000);
    let (cached, _) = scenario(Some(&cache));
    assert_eq!(
        cached.storage(x, 2),
        control.storage(x, 2),
        "cache-backed execution diverged from canonical execution"
    );
}
