//! Fixtures and helpers shared across the B-20 precompile golden test suites.

use alloy_primitives::{Address, B256, Bytes, LogData, U256, address, hex, keccak256};
use alloy_sol_types::SolValue;
use base_common_genesis::BaseUpgrade;
use base_common_precompiles::{
    FakePolicyAccounting, IB20, PermitArgs, TokenAccounting, UpgradeGatedStorageFeatures,
};
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

/// A second policy id distinct from [`POLICY_ID`], used where a test needs both a
/// seize-holder and a seize-receiver scope configured simultaneously.
pub const POLICY_ID_2: u64 = (1u64 << 56) | 8;

// Anvil/Hardhat account 0 — well-known test key, never used in production.
pub const PRIVATE_KEY: [u8; 32] =
    hex!("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

// --- small value helpers ----------------------------------------------------

/// `U256` from a small literal.
pub fn u(n: u64) -> U256 {
    U256::from(n)
}

/// The ABI encoding for a boolean-returning op (`transfer`/`approve`).
pub fn ok_true() -> Bytes {
    Bytes::from(true.abi_encode())
}

// --- token harness ----------------------------------------------------------

/// Empty provider on [`CHAIN_ID`] with the storage features active at `upgrade`.
pub fn storage_at(upgrade: BaseUpgrade) -> HashMapStorageProvider {
    HashMapStorageProvider::new_with_storage_features(
        CHAIN_ID,
        UpgradeGatedStorageFeatures::from_upgrade(upgrade),
    )
}

/// Topic-0 (signature hash) of the last event emitted by the token.
pub fn last_topic0(storage: &HashMapStorageProvider) -> B256 {
    storage.get_events(TOKEN).last().expect("an emitted event").topics()[0]
}

/// Asserts the token's storage hash, or prints it under `BLESS_GOLDEN` for (re)pinning.
#[track_caller]
pub fn assert_token_root(label: &str, storage: HashMapStorageProvider, expected: B256) {
    bless_or_assert_root(label, hash_token_state(storage, TOKEN), expected);
}

/// Grants `role` to `who` and bumps the role member count (setup only).
pub fn give_role<T: TokenAccounting>(token: &mut T, role: B256, who: Address) {
    token.set_role(role, who, true).unwrap();
    let next = token.role_member_count(role).unwrap() + U256::ONE;
    token.set_role_member_count(role, next).unwrap();
}

/// Credits `who` with `amount` and grows total supply to match (setup only).
pub fn fund<T: TokenAccounting>(token: &mut T, who: Address, amount: U256) {
    let balance = token.balance_of(who).unwrap();
    token.set_balance(who, balance + amount).unwrap();
    let supply = token.total_supply().unwrap();
    token.set_total_supply(supply + amount).unwrap();
}

/// A `FakePolicyAccounting` authorizing `who` under the default (0) scope.
pub fn allow0(who: Address) -> FakePolicyAccounting {
    let mut p = FakePolicyAccounting::new();
    p.allow(0, who);
    p
}

/// A `FakePolicyAccounting` that authorizes `who` under [`POLICY_ID_2`] (the seize-receiver
/// scope configured by each suite's `make_seizable`), leaving [`POLICY_ID`] (seize-holder) with
/// no members.
pub fn seize_receiver_policy(who: Address) -> FakePolicyAccounting {
    let mut policy = FakePolicyAccounting::new();
    policy.allow(POLICY_ID_2, who);
    policy
}

/// Generates the storage-typed helpers of one asset/stablecoin golden suite: `token`, `seed`,
/// `read`, `op`, `op_privileged` and `gas`.
///
/// The invoking file must define `fresh()`, because each token type has its own init struct.
/// `upgrade` is the fork the suite resolves through `versions` (the version resolver);
/// `version` and `policy` are the logic and policy versions that fork activates.
#[macro_export]
macro_rules! token_golden_harness {
    (
        storage: $storage:ident,
        token: $token:ident,
        versions: $versions:ident,
        version: $version:expr,
        upgrade: $upgrade:expr,
        policy: $policy_version:expr $(,)?
    ) => {
        /// Builds the token under test over `ctx`, backed by `policy`.
        fn token<'a>(
            ctx: ::base_precompile_storage::StorageCtx<'a>,
            policy: ::base_common_precompiles::FakePolicyAccounting,
        ) -> $token<$storage<'a>, ::base_common_precompiles::FakePolicyAccounting> {
            $token::with_storage_and_policy(
                $storage::from_address($crate::common::TOKEN, ctx),
                policy,
                $policy_version,
            )
        }

        /// Mutates raw token storage through the accounting port (test setup only).
        fn seed(
            storage: &mut ::base_precompile_storage::HashMapStorageProvider,
            f: impl FnOnce(&mut $storage<'_>),
        ) {
            ::base_precompile_storage::StorageCtx::enter(storage, |ctx| {
                f(&mut $storage::from_address($crate::common::TOKEN, ctx));
            });
        }

        /// Reads token state through the accounting port.
        fn read<R>(
            storage: &mut ::base_precompile_storage::HashMapStorageProvider,
            f: impl FnOnce(&$storage<'_>) -> R,
        ) -> R {
            ::base_precompile_storage::StorageCtx::enter(storage, |ctx| {
                f(&$storage::from_address($crate::common::TOKEN, ctx))
            })
        }

        /// Drives one op through the resolver-gated unprivileged path.
        fn op(
            storage: &mut ::base_precompile_storage::HashMapStorageProvider,
            caller: ::alloy_primitives::Address,
            policy: ::base_common_precompiles::FakePolicyAccounting,
            calldata: Vec<u8>,
        ) -> Result<::alloy_primitives::Bytes, ::base_precompile_storage::BasePrecompileError> {
            storage.set_caller(caller);
            ::base_precompile_storage::StorageCtx::enter(storage, |ctx| {
                let version = $versions::from_base_upgrade($upgrade)
                    .expect("the suite's upgrade activates a version");
                token(ctx, policy).route(
                    ctx,
                    &calldata,
                    version,
                    false,
                    ::base_common_precompiles::NoopPrecompileCallObserver,
                )
            })
        }

        /// Drives one op with factory-init privilege (guards skipped).
        fn op_privileged(
            storage: &mut ::base_precompile_storage::HashMapStorageProvider,
            caller: ::alloy_primitives::Address,
            policy: ::base_common_precompiles::FakePolicyAccounting,
            calldata: Vec<u8>,
        ) -> Result<::alloy_primitives::Bytes, ::base_precompile_storage::BasePrecompileError> {
            storage.set_caller(caller);
            ::base_precompile_storage::StorageCtx::enter(storage, |ctx| {
                token(ctx, policy).route(
                    ctx,
                    &calldata,
                    $version,
                    true,
                    ::base_common_precompiles::NoopPrecompileCallObserver,
                )
            })
        }

        /// Runs `calldata` privileged after `setup`, returning `(sload, sstore, keccak256)` counts.
        fn gas(
            setup: impl FnOnce(&mut $storage<'_>),
            caller: ::alloy_primitives::Address,
            policy: ::base_common_precompiles::FakePolicyAccounting,
            calldata: Vec<u8>,
        ) -> (u64, u64, u64) {
            let mut s = fresh();
            seed(&mut s, setup);
            // Pinned so multiplier-scheduling calldata (`effectiveAt` etc.) is never accidentally
            // "in the past" relative to the provider's real wall-clock default timestamp.
            s.set_timestamp(::alloy_primitives::U256::ZERO);
            s.reset_counters();
            op_privileged(&mut s, caller, policy, calldata).expect("gas-footprint op must succeed");
            (s.counter_sload(), s.counter_sstore(), s.counter_keccak256())
        }
    };
}

/// Generates `gas_unprivileged_revert`, the unprivileged reject-path footprint helper, for suites
/// that already expand [`token_golden_harness`] (it relies on that macro's `fresh`, `seed` and
/// `op`).
#[macro_export]
macro_rules! unprivileged_revert_gas_harness {
    ($storage:ident) => {
        /// Unprivileged reject-path footprint: `(sload, sstore, keccak256)` after `calldata`
        /// reverts.
        ///
        /// Locks that a zero-address `transfer` rejects before the transfer-policy-id SLOAD
        /// (Cantina #13 / BOP-600). Privileged success-path `gas()` cannot catch that.
        fn gas_unprivileged_revert(
            setup: impl FnOnce(&mut $storage<'_>),
            caller: ::alloy_primitives::Address,
            policy: ::base_common_precompiles::FakePolicyAccounting,
            calldata: Vec<u8>,
        ) -> (u64, u64, u64) {
            let mut s = fresh();
            seed(&mut s, setup);
            s.set_timestamp(::alloy_primitives::U256::ZERO);
            s.reset_counters();
            let err = op(&mut s, caller, policy, calldata)
                .expect_err("reject-path gas golden must revert");
            assert_eq!(
                err,
                ::base_precompile_storage::BasePrecompileError::revert(
                    ::base_common_precompiles::IB20::InvalidReceiver {
                        receiver: ::alloy_primitives::Address::ZERO
                    }
                )
            );
            (s.counter_sload(), s.counter_sstore(), s.counter_keccak256())
        }
    };
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
