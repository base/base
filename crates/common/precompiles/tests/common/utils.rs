//! Fixtures and helpers shared across the B-20 precompile golden test suites.

use alloy_primitives::{Address, B256, Bytes, LogData, U256, address, hex, keccak256};
use alloy_sol_types::SolValue;
use base_common_genesis::BaseUpgrade;
use base_common_precompiles::{IB20, PermitArgs, UpgradeGatedStorageFeatures};
use base_precompile_storage::HashMapStorageProvider;
use k256::ecdsa::SigningKey;

// --- shared fixtures --------------------------------------------------------

pub const CHAIN_ID: u64 = 8453;
pub const ADMIN: Address = Address::repeat_byte(0xAD);
pub const ALICE: Address = Address::repeat_byte(0xA1);
pub const BOB: Address = Address::repeat_byte(0xB0);
pub const CAROL: Address = Address::repeat_byte(0xCA);
/// The token precompile address shared by the asset/stablecoin per-op suites.
pub const TOKEN: Address = Address::repeat_byte(0x22);
pub const MEMO: B256 = B256::repeat_byte(0x77);
/// The activation admin used by the factory/policy suites.
pub const ACTIVATION_ADMIN: Address = address!("0xcb00000000000000000000000000000000000000");

/// A concrete (non-sentinel) ALLOWLIST policy id (type byte = 1, counter = 7).
///
/// Unconfigured scopes default to the `ALWAYS_ALLOW_ID` (0) EVM zero-slot, so
/// blocking/executor guards are exercised against an explicit id like this one.
/// Under V1, ALLOWLIST (type = 1) authorizes exactly its members: `.allow(POLICY_ID, acct)`
/// grants, and unconfigured accounts are blocked.
pub const POLICY_ID: u64 = (1u64 << 56) | 7;

// Anvil/Hardhat account 0 — well-known test key, never used in production.
pub const PRIVATE_KEY: [u8; 32] =
    hex!("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

// --- provider construction --------------------------------------------------

/// A fresh, empty provider whose persistent-storage features match `upgrade` in production, so a
/// golden dispatching at `upgrade` exercises the same `SLOAD`/dynamic-tail-cleanup behavior as
/// on-chain rather than the `HashMapStorageProvider::new` `Legacy` default. Reuses the production
/// mapping [`UpgradeGatedStorageFeatures::from_upgrade`] as the single source of truth.
pub fn provider_for(upgrade: BaseUpgrade) -> HashMapStorageProvider {
    let mut storage = HashMapStorageProvider::new(CHAIN_ID);
    storage.set_storage_features(UpgradeGatedStorageFeatures::from_upgrade(upgrade));
    storage
}

// --- small value helpers ----------------------------------------------------

/// `U256` from a small literal.
pub fn u(n: u64) -> U256 {
    U256::from(n)
}

/// The ABI encoding for a boolean-returning op (`transfer`/`approve`).
pub fn ok_true() -> Bytes {
    Bytes::from(true.abi_encode())
}

// --- permit signing ---------------------------------------------------------

/// Recovers the anvil account-0 address from [`PRIVATE_KEY`].
pub fn anvil_owner() -> Address {
    let key = SigningKey::from_slice(&PRIVATE_KEY).unwrap();
    let point = key.verifying_key().to_encoded_point(false);
    Address::from_slice(&keccak256(&point.as_bytes()[1..])[12..])
}

/// Builds a validly-signed `permit` call for `owner`'s current nonce.
pub fn signed_permit(
    domain_sep: B256,
    nonce: U256,
    owner: Address,
    spender: Address,
    value: U256,
    deadline: U256,
) -> IB20::permitCall {
    let mut args =
        PermitArgs { owner, spender, value, deadline, v: 0, r: B256::ZERO, s: B256::ZERO };
    let signing_hash = args.signing_hash(domain_sep, nonce);
    let key = SigningKey::from_slice(&PRIVATE_KEY).unwrap();
    let (sig, recid) = key.sign_prehash_recoverable(signing_hash.as_slice()).unwrap();
    let bytes = sig.to_bytes();
    args.r = B256::from_slice(&bytes[..32]);
    args.s = B256::from_slice(&bytes[32..]);
    args.v = if recid.is_y_odd() { 28 } else { 27 };
    IB20::permitCall {
        owner: args.owner,
        spender: args.spender,
        value: args.value,
        deadline: args.deadline,
        v: args.v,
        r: args.r,
        s: args.s,
    }
}

// --- golden hashing / blessing ----------------------------------------------

/// Deterministic keccak hash of a single token's per-case snapshot: the token's emitted
/// events (topics + data) followed by every sorted `(address, slot, value)` storage triple.
///
/// A plain content hash (not an MPT state root). Events are included so a regression in
/// an event's payload — indexed args or data not otherwise reflected in storage, e.g. a
/// `Memo`'s bytes — is pinned here even though logs are not storage. Used by the asset and
/// stablecoin per-op suites, whose only stateful address is the token itself.
pub fn hash_token_state(storage: HashMapStorageProvider, token: Address) -> B256 {
    let events: Vec<LogData> = storage.get_events(token).clone();
    let mut triples: Vec<(Address, U256, U256)> = storage.into_storage().collect();
    triples.sort();
    let mut buf = Vec::with_capacity(triples.len() * 84 + events.len() * 64);
    for log in &events {
        for topic in log.topics() {
            buf.extend_from_slice(topic.as_slice());
        }
        buf.extend_from_slice(&log.data);
    }
    for (addr, slot, value) in triples {
        buf.extend_from_slice(addr.as_slice());
        buf.extend_from_slice(&slot.to_be_bytes::<32>());
        buf.extend_from_slice(&value.to_be_bytes::<32>());
    }
    keccak256(&buf)
}

/// Asserts a storage hash equals its pin, or prints it under `BLESS_GOLDEN=1` for (re)pinning.
#[track_caller]
pub fn bless_or_assert_root(label: &str, got: B256, expected: B256) {
    if std::env::var("BLESS_GOLDEN").ok().as_deref() == Some("1") {
        println!("GOLDEN_ROOT {label} = {got:#x}");
        return;
    }
    assert_eq!(got, expected, "V1 storage hash drift for `{label}`");
}

/// Asserts per-op gas footprints match their pins, or prints them under `BLESS_GOLDEN=1`.
#[track_caller]
pub fn bless_or_assert_gas(
    actual: &[(&str, (u64, u64, u64))],
    expected: &[(&str, (u64, u64, u64))],
) {
    if std::env::var("BLESS_GOLDEN").ok().as_deref() == Some("1") {
        for (label, counts) in actual {
            println!("GAS {label} = {counts:?}");
        }
        return;
    }
    assert_eq!(actual, expected, "storage-access footprint (sload, sstore, keccak256) drift");
}
