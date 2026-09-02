//! Golden tests pinning Factory **V1** behavior of the B-20 precompile.
//!
//! These are authored and pinned against the shipped **v1.1.1** (pre-versioned) factory
//! implementation; the conversion to the versioned precompile structure is behavior-preserving
//! and continues to satisfy every pin below unchanged.
//!
//! Every op (token creation flows, address derivation, prefix/initialized reads) is driven
//! through the real `B20FactoryStorage` entry (version-resolver-gated `dispatch`) over
//! `HashMapStorageProvider`. Each case asserts:
//!   1. exact returned ABI bytes (or the typed revert),
//!   2. resulting state — the *created token's* code/storage plus the factory's,
//!   3. emitted events (`B20Created` at the factory + role/init events at the token), and
//!   4. a per-case keccak storage **hash** snapshot (the frozen-manifest baseline).
//!
//! Creation requires the variant's activation feature to be enabled; `fresh()` activates both.
//!
//! ## Blessing storage hashes
//! State-hash constants below are pinned. To (re)generate them after an intentional change, run:
//! `BLESS_GOLDEN=1 cargo test -p base-common-precompiles --features test-utils \
//!    --test b20_factory_v1_golden -- --nocapture` and copy the printed `GOLDEN_ROOT` values.

use alloy_primitives::{Address, B256, Bytes, LogData, U256, b256, keccak256};
use alloy_sol_types::{SolCall, SolError, SolEvent, SolValue};
use base_common_genesis::BaseUpgrade;
use base_common_precompiles::{
    ActivationAdminConfig, ActivationFeature, ActivationRegistryStorage, AssetAccounting,
    B20AssetStorage, B20FactoryStorage, B20PolicyType, B20StablecoinStorage, B20TokenRole,
    B20Variant, FactoryVersion, FactoryVersions, IActivationRegistry, IB20, IB20Factory,
    IPolicyRegistry, PolicyRegistryStorage, StablecoinAccounting, TokenAccounting,
};
use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

mod common;
use common::{
    ACTIVATION_ADMIN, ADMIN, ALICE, CHAIN_ID, bless_or_assert_gas, bless_or_assert_root, u,
};

// --- fixtures ---------------------------------------------------------------

const CREATOR: Address = Address::repeat_byte(0xC0);
const SALT: B256 = B256::repeat_byte(0x51);
const NAME: &str = "Base Asset";
const SYMBOL: &str = "bASSET";
const SC_NAME: &str = "USD Coin";
const SC_SYMBOL: &str = "USDC";
const CURRENCY: &str = "USD";
const ASSET_DECIMALS: u8 = 6;

// --- pinned storage hashes (bless with BLESS_GOLDEN=1; see module docs) --------

const ROOT_CREATE_ASSET: B256 =
    b256!("f7f92ca9c8974431d62db57bf3bf8b02e25cc31b37e88521ae1a409f3b522e8b");
const ROOT_CREATE_STABLECOIN: B256 =
    b256!("719bcff400266cb793065adaf6636584a8f81d274255852a4433a6d2ca7f2b93");
const ROOT_CREATE_WITH_INIT_CALLS: B256 =
    b256!("314d42c1d68896acd51829b7b66a9884f8dbeca50838d4a88e8b4c98bfdcdd0f");
const ROOT_CREATE_ZERO_ADMIN: B256 =
    b256!("483e93df226ac4cf92969a48bfd2d6b67f49e2ab63322c58c3abd95d98b58289");
const ROOT_CREATE_SC_WITH_INIT_CALLS: B256 =
    b256!("f757f7ed634e58fac4ff7c8fbb82fad003287185337ccb067d5564c8776b2cf2");

// --- harness ----------------------------------------------------------------

/// The factory precompile's singleton address.
const fn factory() -> Address {
    B20FactoryStorage::ADDRESS
}

/// Activates both B-20 variants so `createB20` is permitted.
fn activate(storage: &mut HashMapStorageProvider) {
    storage.set_caller(ACTIVATION_ADMIN);
    for key in [ActivationFeature::B20Stablecoin.id(), ActivationFeature::B20Asset.id()] {
        StorageCtx::enter(storage, |ctx| {
            ActivationRegistryStorage::new(ctx)
                .activate(key, ActivationAdminConfig::static_fallback(Some(ACTIVATION_ADMIN)))
                .unwrap();
        });
    }
}

/// A fresh provider with both B-20 variants activated.
fn fresh() -> HashMapStorageProvider {
    let mut storage = HashMapStorageProvider::new(CHAIN_ID);
    activate(&mut storage);
    storage
}

/// ABI-encoded asset creation params.
fn asset_params(admin: Address, decimals: u8) -> Bytes {
    IB20Factory::B20AssetCreateParams {
        version: 1,
        name: NAME.into(),
        symbol: SYMBOL.into(),
        initialAdmin: admin,
        decimals,
    }
    .abi_encode()
    .into()
}

/// ABI-encoded stablecoin creation params.
fn stablecoin_params(admin: Address, currency: &str) -> Bytes {
    IB20Factory::B20StablecoinCreateParams {
        version: 1,
        name: SC_NAME.into(),
        symbol: SC_SYMBOL.into(),
        initialAdmin: admin,
        currency: currency.into(),
    }
    .abi_encode()
    .into()
}

/// A `createB20` call for the given variant/params.
fn create_call(
    variant: IB20Factory::B20Variant,
    salt: B256,
    params: Bytes,
    init_calls: Vec<Bytes>,
) -> Vec<u8> {
    IB20Factory::createB20Call { variant, salt, params, initCalls: init_calls }.abi_encode()
}

/// Drives one factory call through the full `dispatch` entry, returning `(is_revert, bytes)`.
fn call_factory(
    storage: &mut HashMapStorageProvider,
    caller: Address,
    calldata: Vec<u8>,
) -> (bool, Bytes) {
    storage.set_caller(caller);
    StorageCtx::enter(storage, |ctx| {
        B20FactoryStorage::new(ctx).dispatch(ctx, &calldata, BaseUpgrade::Beryl)
    })
    .map(|out| (out.is_revert(), out.bytes))
    .expect("dispatch must not fatally error")
}

/// The deterministic asset-token address for `(creator, salt)`.
fn asset_addr(creator: Address, salt: B256) -> Address {
    B20Variant::Asset.compute_address(creator, salt).0
}

/// The deterministic stablecoin-token address for `(creator, salt)`.
fn stablecoin_addr(creator: Address, salt: B256) -> Address {
    B20Variant::Stablecoin.compute_address(creator, salt).0
}

/// Reads created asset-token state through the accounting port.
fn read_asset<R>(
    storage: &mut HashMapStorageProvider,
    token: Address,
    f: impl FnOnce(&B20AssetStorage<'_>) -> R,
) -> R {
    StorageCtx::enter(storage, |ctx| f(&B20AssetStorage::from_address(token, ctx)))
}

/// Reads created stablecoin-token state through the accounting port.
fn read_stablecoin<R>(
    storage: &mut HashMapStorageProvider,
    token: Address,
    f: impl FnOnce(&B20StablecoinStorage<'_>) -> R,
) -> R {
    StorageCtx::enter(storage, |ctx| f(&B20StablecoinStorage::from_address(token, ctx)))
}

/// Deterministic keccak hash of the per-case snapshot, scoped to `addrs` (the factory and the
/// created token): emitted events at each address (topics + data), followed by the sorted
/// `(address, slot, value)` storage triples belonging to those addresses.
///
/// Scoping to the factory + created token deliberately excludes activation-registry scaffolding,
/// so the pin captures only the factory's own effect and is stable across unrelated changes to
/// the activation-setup API.
fn hash_state(storage: HashMapStorageProvider, addrs: &[Address]) -> B256 {
    let mut buf = Vec::new();
    for addr in addrs {
        let events: Vec<LogData> = storage.get_events(*addr).clone();
        for log in &events {
            for topic in log.topics() {
                buf.extend_from_slice(topic.as_slice());
            }
            buf.extend_from_slice(&log.data);
        }
    }
    let mut triples: Vec<(Address, U256, U256)> =
        storage.into_storage().filter(|(a, _, _)| addrs.contains(a)).collect();
    triples.sort();
    for (addr, slot, value) in triples {
        buf.extend_from_slice(addr.as_slice());
        buf.extend_from_slice(&slot.to_be_bytes::<32>());
        buf.extend_from_slice(&value.to_be_bytes::<32>());
    }
    keccak256(&buf)
}

/// Asserts the storage hash, or prints it under `BLESS_GOLDEN` for (re)pinning.
#[track_caller]
fn assert_root(
    label: &str,
    storage: HashMapStorageProvider,
    event_addrs: &[Address],
    expected: B256,
) {
    bless_or_assert_root(label, hash_state(storage, event_addrs), expected);
}

// ============================================================================
// Version resolver
// ============================================================================

#[test]
fn resolver_maps_forks_to_versions() {
    assert_eq!(FactoryVersions::from_base_upgrade(BaseUpgrade::Azul), None);
    assert_eq!(FactoryVersions::from_base_upgrade(BaseUpgrade::Beryl), Some(FactoryVersion::V1));
    assert_eq!(FactoryVersions::from_base_upgrade(BaseUpgrade::Cobalt), Some(FactoryVersion::V1));
}

#[test]
fn dispatch_reverts_before_beryl() {
    let mut s = fresh();
    let calldata = IB20Factory::isB20Call { token: asset_addr(CREATOR, SALT) }.abi_encode();
    s.set_caller(ALICE);
    let (rev, bytes) = StorageCtx::enter(&mut s, |ctx| {
        B20FactoryStorage::new(ctx).dispatch(ctx, &calldata, BaseUpgrade::Azul)
    })
    .map(|out| (out.is_revert(), out.bytes))
    .expect("dispatch must not fatally error");
    assert!(rev);
    assert!(bytes.is_empty());
}

// ============================================================================
// getB20Address (deterministic address derivation)
// ============================================================================

#[test]
fn golden_get_b20_address() {
    let mut s = fresh();
    let (rev, bytes) = call_factory(
        &mut s,
        ALICE,
        IB20Factory::getB20AddressCall {
            variant: IB20Factory::B20Variant::ASSET,
            sender: CREATOR,
            salt: SALT,
        }
        .abi_encode(),
    );
    assert!(!rev);
    assert_eq!(
        bytes,
        Bytes::from(IB20Factory::getB20AddressCall::abi_encode_returns(&asset_addr(CREATOR, SALT)))
    );

    let (rev, bytes) = call_factory(
        &mut s,
        ALICE,
        IB20Factory::getB20AddressCall {
            variant: IB20Factory::B20Variant::STABLECOIN,
            sender: CREATOR,
            salt: SALT,
        }
        .abi_encode(),
    );
    assert!(!rev);
    assert_eq!(
        bytes,
        Bytes::from(IB20Factory::getB20AddressCall::abi_encode_returns(&stablecoin_addr(
            CREATOR, SALT
        )))
    );
}

// ============================================================================
// isB20 / isB20Initialized
// ============================================================================

#[test]
fn golden_is_b20() {
    let mut s = fresh();
    // A derived B-20 address has the structural prefix.
    let (rev, bytes) = call_factory(
        &mut s,
        ALICE,
        IB20Factory::isB20Call { token: asset_addr(CREATOR, SALT) }.abi_encode(),
    );
    assert!(!rev);
    assert_eq!(bytes, Bytes::from(IB20Factory::isB20Call::abi_encode_returns(&true)));

    // A random address does not.
    let (rev, bytes) = call_factory(
        &mut s,
        ALICE,
        IB20Factory::isB20Call { token: Address::repeat_byte(0x99) }.abi_encode(),
    );
    assert!(!rev);
    assert_eq!(bytes, Bytes::from(IB20Factory::isB20Call::abi_encode_returns(&false)));
}

#[test]
fn golden_is_b20_initialized() {
    let mut s = fresh();
    let token = asset_addr(CREATOR, SALT);
    // Prefix address but not yet created => not initialized.
    let (rev, bytes) =
        call_factory(&mut s, ALICE, IB20Factory::isB20InitializedCall { token }.abi_encode());
    assert!(!rev);
    assert_eq!(bytes, Bytes::from(IB20Factory::isB20InitializedCall::abi_encode_returns(&false)));

    // After creation => initialized.
    let (rev, _) = call_factory(
        &mut s,
        CREATOR,
        create_call(
            IB20Factory::B20Variant::ASSET,
            SALT,
            asset_params(ADMIN, ASSET_DECIMALS),
            vec![],
        ),
    );
    assert!(!rev);
    let (rev, bytes) =
        call_factory(&mut s, ALICE, IB20Factory::isB20InitializedCall { token }.abi_encode());
    assert!(!rev);
    assert_eq!(bytes, Bytes::from(IB20Factory::isB20InitializedCall::abi_encode_returns(&true)));
}

// ============================================================================
// createB20 — asset
// ============================================================================

#[test]
fn golden_create_asset() {
    let mut s = fresh();
    let token = asset_addr(CREATOR, SALT);
    let (rev, bytes) = call_factory(
        &mut s,
        CREATOR,
        create_call(
            IB20Factory::B20Variant::ASSET,
            SALT,
            asset_params(ADMIN, ASSET_DECIMALS),
            vec![],
        ),
    );
    assert!(!rev);
    assert_eq!(bytes, Bytes::from(IB20Factory::createB20Call::abi_encode_returns(&token)));

    read_asset(&mut s, token, |t| {
        assert_eq!(t.name().unwrap(), NAME);
        assert_eq!(t.symbol().unwrap(), SYMBOL);
        assert_eq!(AssetAccounting::decimals(t).unwrap(), ASSET_DECIMALS);
        assert!(t.has_role(B20TokenRole::DefaultAdmin.id(), ADMIN).unwrap());
    });

    // B20Created emitted at the factory address.
    let created = s.get_events(factory());
    assert_eq!(created.last().unwrap().topics()[0], IB20Factory::B20Created::SIGNATURE_HASH);

    assert_root("create_asset", s, &[factory(), token], ROOT_CREATE_ASSET);
}

#[test]
fn golden_create_stablecoin() {
    let mut s = fresh();
    let token = stablecoin_addr(CREATOR, SALT);
    let (rev, bytes) = call_factory(
        &mut s,
        CREATOR,
        create_call(
            IB20Factory::B20Variant::STABLECOIN,
            SALT,
            stablecoin_params(ADMIN, CURRENCY),
            vec![],
        ),
    );
    assert!(!rev);
    assert_eq!(bytes, Bytes::from(IB20Factory::createB20Call::abi_encode_returns(&token)));

    read_stablecoin(&mut s, token, |t| {
        assert_eq!(t.name().unwrap(), SC_NAME);
        assert_eq!(t.symbol().unwrap(), SC_SYMBOL);
        assert_eq!(t.currency().unwrap(), CURRENCY);
        assert!(t.has_role(B20TokenRole::DefaultAdmin.id(), ADMIN).unwrap());
    });
    let created = s.get_events(factory());
    assert_eq!(created.last().unwrap().topics()[0], IB20Factory::B20Created::SIGNATURE_HASH);

    assert_root("create_stablecoin", s, &[factory(), token], ROOT_CREATE_STABLECOIN);
}

#[test]
fn golden_create_stablecoin_with_init_calls() {
    let mut s = fresh();
    let token = stablecoin_addr(CREATOR, SALT);
    let mint = IB20::mintCall { to: ALICE, amount: u(500) }.abi_encode();
    let (rev, _) = call_factory(
        &mut s,
        CREATOR,
        create_call(
            IB20Factory::B20Variant::STABLECOIN,
            SALT,
            stablecoin_params(ADMIN, CURRENCY),
            vec![Bytes::from(mint)],
        ),
    );
    assert!(!rev);
    read_stablecoin(&mut s, token, |t| {
        assert_eq!(t.balance_of(ALICE).unwrap(), u(500));
        assert_eq!(t.total_supply().unwrap(), u(500));
    });
    assert_root(
        "create_sc_with_init_calls",
        s,
        &[factory(), token],
        ROOT_CREATE_SC_WITH_INIT_CALLS,
    );
}

#[test]
fn golden_create_with_init_calls() {
    let mut s = fresh();
    let token = asset_addr(CREATOR, SALT);
    // An init call minting initial supply, executed atomically during creation.
    let mint = IB20::mintCall { to: ALICE, amount: u(1_000) }.abi_encode();
    let (rev, _) = call_factory(
        &mut s,
        CREATOR,
        create_call(
            IB20Factory::B20Variant::ASSET,
            SALT,
            asset_params(ADMIN, ASSET_DECIMALS),
            vec![Bytes::from(mint)],
        ),
    );
    assert!(!rev);
    read_asset(&mut s, token, |t| {
        assert_eq!(t.balance_of(ALICE).unwrap(), u(1_000));
        assert_eq!(t.total_supply().unwrap(), u(1_000));
    });
    assert_root("create_with_init_calls", s, &[factory(), token], ROOT_CREATE_WITH_INIT_CALLS);
}

#[test]
fn golden_create_zero_admin_skips_grant() {
    let mut s = fresh();
    let token = asset_addr(CREATOR, SALT);
    let (rev, _) = call_factory(
        &mut s,
        CREATOR,
        create_call(
            IB20Factory::B20Variant::ASSET,
            SALT,
            asset_params(Address::ZERO, ASSET_DECIMALS),
            vec![],
        ),
    );
    assert!(!rev);
    read_asset(&mut s, token, |t| {
        assert!(!t.has_role(B20TokenRole::DefaultAdmin.id(), Address::ZERO).unwrap());
    });
    assert_root("create_zero_admin", s, &[factory(), token], ROOT_CREATE_ZERO_ADMIN);
}

// ============================================================================
// createB20 — reverts
// ============================================================================

#[test]
fn golden_create_reverts_token_already_exists() {
    let mut s = fresh();
    let token = asset_addr(CREATOR, SALT);
    let (rev, _) = call_factory(
        &mut s,
        CREATOR,
        create_call(
            IB20Factory::B20Variant::ASSET,
            SALT,
            asset_params(ADMIN, ASSET_DECIMALS),
            vec![],
        ),
    );
    assert!(!rev);
    // Same (creator, salt) => same address => already deployed.
    let (rev, bytes) = call_factory(
        &mut s,
        CREATOR,
        create_call(
            IB20Factory::B20Variant::ASSET,
            SALT,
            asset_params(ADMIN, ASSET_DECIMALS),
            vec![],
        ),
    );
    assert!(rev);
    assert_eq!(bytes, Bytes::from(IB20Factory::TokenAlreadyExists { token }.abi_encode()));
}

#[test]
fn golden_create_reverts_unsupported_version() {
    let mut s = fresh();
    let params: Bytes = IB20Factory::B20AssetCreateParams {
        version: 2,
        name: NAME.into(),
        symbol: SYMBOL.into(),
        initialAdmin: ADMIN,
        decimals: ASSET_DECIMALS,
    }
    .abi_encode()
    .into();
    let (rev, bytes) = call_factory(
        &mut s,
        CREATOR,
        create_call(IB20Factory::B20Variant::ASSET, SALT, params, vec![]),
    );
    assert!(rev);
    assert_eq!(
        bytes,
        Bytes::from(
            IB20Factory::UnsupportedVersion { version: 2, variant: IB20Factory::B20Variant::ASSET }
                .abi_encode()
        )
    );
}

#[test]
fn golden_create_reverts_invalid_decimals() {
    let mut s = fresh();
    let (rev, bytes) = call_factory(
        &mut s,
        CREATOR,
        create_call(IB20Factory::B20Variant::ASSET, SALT, asset_params(ADMIN, 5), vec![]),
    );
    assert!(rev);
    assert_eq!(bytes, Bytes::from(IB20Factory::InvalidDecimals { decimals: 5 }.abi_encode()));
}

#[test]
fn golden_create_reverts_nonzero_value() {
    let mut s = fresh();
    let calldata = create_call(
        IB20Factory::B20Variant::ASSET,
        SALT,
        asset_params(ADMIN, ASSET_DECIMALS),
        vec![],
    );
    s.set_caller(CREATOR);
    s.set_call_value(U256::ONE);
    let (rev, bytes) = StorageCtx::enter(&mut s, |ctx| {
        B20FactoryStorage::new(ctx).dispatch(ctx, &calldata, BaseUpgrade::Beryl)
    })
    .map(|out| (out.is_revert(), out.bytes))
    .expect("dispatch must not fatally error");
    assert!(rev);
    assert_eq!(bytes, Bytes::from(IB20Factory::NonPayable {}.abi_encode()));
}

#[test]
fn golden_create_reverts_init_call_failed() {
    let mut s = fresh();
    // An init call with an unknown selector fails the atomic post-creation init.
    let bad = Bytes::from(vec![0xde_u8, 0xad, 0xbe, 0xef]);
    let (rev, bytes) = call_factory(
        &mut s,
        CREATOR,
        create_call(
            IB20Factory::B20Variant::ASSET,
            SALT,
            asset_params(ADMIN, ASSET_DECIMALS),
            vec![bad],
        ),
    );
    assert!(rev);
    assert_eq!(bytes, Bytes::from(IB20Factory::InitCallFailed { index: U256::ZERO }.abi_encode()));
}

#[test]
fn golden_create_reverts_missing_currency() {
    let mut s = fresh();
    let (rev, bytes) = call_factory(
        &mut s,
        CREATOR,
        create_call(
            IB20Factory::B20Variant::STABLECOIN,
            SALT,
            stablecoin_params(ADMIN, ""),
            vec![],
        ),
    );
    assert!(rev);
    assert_eq!(
        bytes,
        Bytes::from(IB20Factory::MissingRequiredField { field: "currency".into() }.abi_encode())
    );
}

#[test]
fn golden_create_reverts_invalid_currency() {
    let mut s = fresh();
    let (rev, bytes) = call_factory(
        &mut s,
        CREATOR,
        create_call(
            IB20Factory::B20Variant::STABLECOIN,
            SALT,
            stablecoin_params(ADMIN, "usd"),
            vec![],
        ),
    );
    assert!(rev);
    assert_eq!(
        bytes,
        Bytes::from(IB20Factory::InvalidCurrency { code: "usd".into() }.abi_encode())
    );
}

#[test]
fn golden_create_propagates_typed_init_call_revert() {
    // A typed (non-empty) revert from an init call propagates as-is, NOT wrapped in InitCallFailed.
    let mut s = fresh();
    let mint_zero = IB20::mintCall { to: Address::ZERO, amount: u(1) }.abi_encode();
    let (rev, bytes) = call_factory(
        &mut s,
        CREATOR,
        create_call(
            IB20Factory::B20Variant::ASSET,
            SALT,
            asset_params(ADMIN, ASSET_DECIMALS),
            vec![Bytes::from(mint_zero)],
        ),
    );
    assert!(rev);
    assert_eq!(bytes, Bytes::from(IB20::InvalidReceiver { receiver: Address::ZERO }.abi_encode()));
}

#[test]
fn golden_create_reverts_malformed_params() {
    let mut s = fresh();
    let (rev, bytes) = call_factory(
        &mut s,
        CREATOR,
        create_call(
            IB20Factory::B20Variant::ASSET,
            SALT,
            Bytes::from(vec![0xaa_u8, 0xbb, 0xcc]),
            vec![],
        ),
    );
    assert!(rev);
    // Malformed params surface as an AbiDecodeFailed carrying the createB20 selector.
    assert_eq!(&bytes[..4], IB20Factory::createB20Call::SELECTOR.as_slice());
}

#[test]
fn golden_create_reverts_when_not_activated() {
    // A provider with NO activation of the asset feature must reject creation.
    let mut s = HashMapStorageProvider::new(CHAIN_ID);
    let (rev, bytes) = call_factory(
        &mut s,
        CREATOR,
        create_call(
            IB20Factory::B20Variant::ASSET,
            SALT,
            asset_params(ADMIN, ASSET_DECIMALS),
            vec![],
        ),
    );
    assert!(rev);
    assert_eq!(
        bytes,
        Bytes::from(
            IActivationRegistry::FeatureNotActivated { feature: ActivationFeature::B20Asset.id() }
                .abi_encode()
        )
    );
}

// ============================================================================
// createB20 — test correct base upgrade logic is used when creating
// ============================================================================

/// Drives one factory call through `dispatch` at `upgrade`, returning `(is_revert, bytes)`.
fn call_factory_at(
    storage: &mut HashMapStorageProvider,
    caller: Address,
    calldata: Vec<u8>,
    upgrade: BaseUpgrade,
) -> (bool, Bytes) {
    storage.set_caller(caller);
    StorageCtx::enter(storage, |ctx| B20FactoryStorage::new(ctx).dispatch(ctx, &calldata, upgrade))
        .map(|out| (out.is_revert(), out.bytes))
        .expect("dispatch must not fatally error")
}

/// Creates a simple policy as `ADMIN` on the live registry (dispatched at Cobalt), returning its id.
fn create_policy(s: &mut HashMapStorageProvider, policy_type: IPolicyRegistry::PolicyType) -> u64 {
    s.set_caller(ADMIN);
    let out = StorageCtx::enter(s, |ctx| {
        PolicyRegistryStorage::new(ctx).dispatch(
            ctx,
            &IPolicyRegistry::createPolicyCall { admin: ADMIN, policyType: policy_type }
                .abi_encode(),
            BaseUpgrade::Cobalt,
        )
    })
    .expect("dispatch must not fatally error");
    assert!(!out.is_revert(), "createPolicy reverted");
    IPolicyRegistry::createPolicyCall::abi_decode_returns(&out.bytes).unwrap()
}

/// Seeds a UNION composite over two simple ALLOWLIST children on the live registry, returning its id.
fn seed_union_composite(s: &mut HashMapStorageProvider) -> u64 {
    s.set_caller(ACTIVATION_ADMIN);
    StorageCtx::enter(s, |ctx| {
        ActivationRegistryStorage::new(ctx)
            .activate(
                ActivationFeature::PolicyRegistry.id(),
                ActivationAdminConfig::static_fallback(Some(ACTIVATION_ADMIN)),
            )
            .unwrap();
    });
    let child_a = create_policy(s, IPolicyRegistry::PolicyType::ALLOWLIST);
    let child_b = create_policy(s, IPolicyRegistry::PolicyType::ALLOWLIST);
    s.set_caller(ADMIN);
    let out = StorageCtx::enter(s, |ctx| {
        PolicyRegistryStorage::new(ctx).dispatch(
            ctx,
            &IPolicyRegistry::createCompositePolicyCall {
                admin: ADMIN,
                policyType: IPolicyRegistry::PolicyType::UNION,
                childPolicyIds: vec![child_a, child_b],
            }
            .abi_encode(),
            BaseUpgrade::Cobalt,
        )
    })
    .expect("dispatch must not fatally error");
    assert!(!out.is_revert(), "createCompositePolicy reverted");
    IPolicyRegistry::createCompositePolicyCall::abi_decode_returns(&out.bytes).unwrap()
}

#[test]
fn golden_cobalt_bootstrap_routes_asset_seize_exempt_policy_init_call() {
    let mut s = fresh();
    let composite = seed_union_composite(&mut s);
    let token = asset_addr(CREATOR, SALT);

    let update = IB20::updatePolicyCall {
        policyScope: B20PolicyType::SeizeExempt.id(),
        newPolicyId: composite,
    }
    .abi_encode();
    let (rev, bytes) = call_factory_at(
        &mut s,
        CREATOR,
        create_call(
            IB20Factory::B20Variant::ASSET,
            SALT,
            asset_params(Address::ZERO, ASSET_DECIMALS),
            vec![Bytes::from(update)],
        ),
        BaseUpgrade::Cobalt,
    );

    assert!(!rev, "Cobalt bootstrap must not revert; got {bytes:?}");
    assert_eq!(bytes, Bytes::from(IB20Factory::createB20Call::abi_encode_returns(&token)));
    read_asset(&mut s, token, |t| {
        assert_eq!(t.policy_id(B20PolicyType::SeizeExempt.id()).unwrap(), composite);
    });
}

#[test]
fn golden_cobalt_bootstrap_routes_stablecoin_seize_exempt_policy_init_call() {
    let mut s = fresh();
    let composite = seed_union_composite(&mut s);
    let token = stablecoin_addr(CREATOR, SALT);

    let update = IB20::updatePolicyCall {
        policyScope: B20PolicyType::SeizeExempt.id(),
        newPolicyId: composite,
    }
    .abi_encode();
    let (rev, bytes) = call_factory_at(
        &mut s,
        CREATOR,
        create_call(
            IB20Factory::B20Variant::STABLECOIN,
            SALT,
            stablecoin_params(Address::ZERO, CURRENCY),
            vec![Bytes::from(update)],
        ),
        BaseUpgrade::Cobalt,
    );

    assert!(!rev, "Cobalt bootstrap must not revert; got {bytes:?}");
    assert_eq!(bytes, Bytes::from(IB20Factory::createB20Call::abi_encode_returns(&token)));
    read_stablecoin(&mut s, token, |t| {
        assert_eq!(t.policy_id(B20PolicyType::SeizeExempt.id()).unwrap(), composite);
    });
}

#[test]
fn golden_beryl_bootstrap_rejects_asset_seize_exempt_policy_init_call() {
    let mut s = fresh();
    let composite = seed_union_composite(&mut s);
    let update = IB20::updatePolicyCall {
        policyScope: B20PolicyType::SeizeExempt.id(),
        newPolicyId: composite,
    }
    .abi_encode();
    let (rev, bytes) = call_factory_at(
        &mut s,
        CREATOR,
        create_call(
            IB20Factory::B20Variant::ASSET,
            SALT,
            asset_params(Address::ZERO, ASSET_DECIMALS),
            vec![Bytes::from(update)],
        ),
        BaseUpgrade::Beryl,
    );

    assert!(rev);
    assert_eq!(
        bytes,
        Bytes::from(
            IB20::UnsupportedPolicyType { policyScope: B20PolicyType::SeizeExempt.id() }
                .abi_encode()
        )
    );
}

#[test]
fn golden_beryl_bootstrap_rejects_stablecoin_seize_exempt_policy_init_call() {
    let mut s = fresh();
    let composite = seed_union_composite(&mut s);
    let update = IB20::updatePolicyCall {
        policyScope: B20PolicyType::SeizeExempt.id(),
        newPolicyId: composite,
    }
    .abi_encode();
    let (rev, bytes) = call_factory_at(
        &mut s,
        CREATOR,
        create_call(
            IB20Factory::B20Variant::STABLECOIN,
            SALT,
            stablecoin_params(Address::ZERO, CURRENCY),
            vec![Bytes::from(update)],
        ),
        BaseUpgrade::Beryl,
    );

    assert!(rev);
    assert_eq!(
        bytes,
        Bytes::from(
            IB20::UnsupportedPolicyType { policyScope: B20PolicyType::SeizeExempt.id() }
                .abi_encode()
        )
    );
}

// ============================================================================
// gas: storage-access footprint per op
// ============================================================================

/// Runs `calldata` as `caller` on a fresh activated factory, returning `(sload, sstore, keccak256)`.
fn gas(caller: Address, calldata: Vec<u8>) -> (u64, u64, u64) {
    let mut s = fresh();
    s.set_caller(caller);
    s.reset_counters();
    let out = StorageCtx::enter(&mut s, |ctx| {
        B20FactoryStorage::new(ctx).dispatch(ctx, &calldata, BaseUpgrade::Beryl)
    })
    .expect("gas-footprint op must not fatally error");
    assert!(!out.is_revert(), "gas-footprint op must succeed (not revert)");
    (s.counter_sload(), s.counter_sstore(), s.counter_keccak256())
}

#[test]
fn golden_gas_footprints() {
    let actual: Vec<(&str, (u64, u64, u64))> = vec![
        (
            "create_asset",
            gas(
                CREATOR,
                create_call(
                    IB20Factory::B20Variant::ASSET,
                    SALT,
                    asset_params(ADMIN, ASSET_DECIMALS),
                    vec![],
                ),
            ),
        ),
        (
            "create_stablecoin",
            gas(
                CREATOR,
                create_call(
                    IB20Factory::B20Variant::STABLECOIN,
                    SALT,
                    stablecoin_params(ADMIN, CURRENCY),
                    vec![],
                ),
            ),
        ),
        (
            "get_b20_address",
            gas(
                ALICE,
                IB20Factory::getB20AddressCall {
                    variant: IB20Factory::B20Variant::ASSET,
                    sender: CREATOR,
                    salt: SALT,
                }
                .abi_encode(),
            ),
        ),
        (
            "is_b20",
            gas(ALICE, IB20Factory::isB20Call { token: asset_addr(CREATOR, SALT) }.abi_encode()),
        ),
        (
            "is_b20_initialized",
            gas(
                ALICE,
                IB20Factory::isB20InitializedCall { token: asset_addr(CREATOR, SALT) }.abi_encode(),
            ),
        ),
    ];

    let expected: &[(&str, (u64, u64, u64))] = &[
        ("create_asset", (3, 7, 1)),
        ("create_stablecoin", (3, 6, 1)),
        ("get_b20_address", (0, 0, 1)),
        ("is_b20", (0, 0, 0)),
        ("is_b20_initialized", (0, 0, 0)),
    ];

    bless_or_assert_gas(&actual, expected);
}

// ============================================================================
// meta: op coverage checklist
// ============================================================================

/// Compile-time coverage checklist — never called; its exhaustive `match` (no `_` arm) names the
/// golden `#[test]` fn(s) pinning each op. Adding an ABI op fails the build until a golden is added.
#[allow(dead_code)]
fn v1_op_coverage_checklist(call: IB20Factory::IB20FactoryCalls) {
    use IB20Factory::IB20FactoryCalls as C;

    fn covered(_goldens: &[fn()]) {}

    match call {
        C::createB20(_) => covered(&[
            golden_create_asset,
            golden_create_stablecoin,
            golden_create_stablecoin_with_init_calls,
            golden_create_with_init_calls,
            golden_create_zero_admin_skips_grant,
            golden_create_reverts_token_already_exists,
            golden_create_reverts_unsupported_version,
            golden_create_reverts_invalid_decimals,
            golden_create_reverts_nonzero_value,
            golden_create_reverts_init_call_failed,
            golden_create_reverts_missing_currency,
            golden_create_reverts_invalid_currency,
            golden_create_reverts_when_not_activated,
            golden_create_propagates_typed_init_call_revert,
            golden_create_reverts_malformed_params,
            golden_cobalt_bootstrap_routes_asset_seize_exempt_policy_init_call,
            golden_cobalt_bootstrap_routes_stablecoin_seize_exempt_policy_init_call,
            golden_beryl_bootstrap_rejects_asset_seize_exempt_policy_init_call,
            golden_beryl_bootstrap_rejects_stablecoin_seize_exempt_policy_init_call,
        ]),
        C::getB20Address(_) => covered(&[golden_get_b20_address]),
        C::isB20(_) => covered(&[golden_is_b20]),
        C::isB20Initialized(_) => covered(&[golden_is_b20_initialized]),
    }
}
