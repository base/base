//! ABI calls to Base's native precompiles: the B-20 token factory and tokens, the policy and
//! activation registries, and the transaction-context and nonce-manager precompiles.

use std::sync::OnceLock;

use alloy_primitives::{Address, B256, Bytes, U256, b256, keccak256};
use alloy_signer::SignerSync;
use alloy_sol_types::{SolCall, SolValue};
use base_common_chains::{ChainConfig, MAINNET_BERYL_ACTIVATION_ADMIN_ADDRESS};
use base_common_genesis::{BaseFeature, BaseUpgrade};
use base_common_precompiles::{
    ActivationFeature, ActivationRegistryStorage, B20_MAX_SUPPLY_CAP, B20FactoryStorage,
    B20PolicyType, B20TokenRole, B20Variant, IActivationRegistry, IB20, IB20Asset, IB20Factory,
    IB20Stablecoin, INonceManager, IPolicyRegistry, ITransactionContext, NonceManagerStorage,
    PermitArgs, PolicyRegistryStorage, TxContextStorage,
};

use crate::r#gen::{
    bootstrap::Bootstrap,
    keys::Keys,
    registry::{Policy, Registry, Token},
    rng::Rng,
};

/// Who should send a native call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// Any generated key.
    Anyone,
    /// A specific address: a generated key, or an address only a deposit can act as.
    Account(Address),
}

/// A native-precompile call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeCall {
    /// Callee.
    pub to: Address,
    /// ABI calldata.
    pub input: Bytes,
    /// Attached value.
    pub value: U256,
    /// Intended sender.
    pub caller: Caller,
    /// Function label, for reach statistics.
    pub label: &'static str,
}

/// Native-precompile call generator.
#[derive(Debug)]
pub struct Native;

const CURRENCIES: [&str; 6] = ["USD", "EUR", "JPY", "GBP", "usd", "XXX"];

impl Native {
    /// Environment variable giving native-precompile calls a different weight in the random
    /// traffic mix (default [`Self::DEFAULT_WEIGHT`], out of about 140), such as `120` for a
    /// precompile-focused campaign.
    pub const WEIGHT_ENV: &str = "BASE_DIFF_NATIVE_WEIGHT";

    /// Weight of native-precompile calls in the random traffic mix without [`Self::WEIGHT_ENV`].
    pub const DEFAULT_WEIGHT: u64 = 14;

    /// Weight of native-precompile calls in the random traffic mix (see [`Self::WEIGHT_ENV`]).
    ///
    /// # Panics
    ///
    /// When the variable is not a number.
    pub fn weight() -> u64 {
        static WEIGHT: OnceLock<u64> = OnceLock::new();
        *WEIGHT.get_or_init(|| {
            std::env::var(Self::WEIGHT_ENV).map_or(Self::DEFAULT_WEIGHT, |value| {
                value
                    .trim()
                    .parse()
                    .unwrap_or_else(|_| panic!("{} is not a number: {value}", Self::WEIGHT_ENV))
            })
        })
    }

    /// Whether native precompiles exist under `upgrade`.
    pub const fn active(upgrade: BaseUpgrade) -> bool {
        upgrade.is_feature_enabled(BaseFeature::TokenFactoryPrecompile)
    }

    /// The activation admin: the operator key once Cobalt moved it to storage, the chain-config
    /// address (reachable only through deposits) before.
    pub fn activation_admin(upgrade: BaseUpgrade) -> Address {
        if upgrade.is_feature_enabled(BaseFeature::StatefulActivationAdmin) {
            Keys::get().operator()
        } else {
            MAINNET_BERYL_ACTIVATION_ADMIN_ADDRESS
        }
    }

    fn call(to: Address, input: Vec<u8>, caller: Caller, label: &'static str) -> NativeCall {
        NativeCall { to, input: input.into(), value: U256::ZERO, caller, label }
    }

    fn user(rng: &mut Rng) -> Address {
        Keys::get().address(rng.range(1, Keys::COUNT as u64 - 1) as usize)
    }

    fn any_account(rng: &mut Rng, registry: &Registry) -> Address {
        match rng.below(8) {
            0 => Address::ZERO,
            1 if !registry.programs.is_empty() => {
                registry.programs[rng.index(registry.programs.len())].address
            }
            2 => rng.address(),
            3 => Keys::get().operator(),
            _ => Self::user(rng),
        }
    }

    /// A token-creation call through the factory.
    pub fn create_token(
        rng: &mut Rng,
        creator: Address,
        salt: B256,
        variant: B20Variant,
    ) -> NativeCall {
        let admin = if rng.chance(85) { creator } else { Self::user(rng) };
        let name = if rng.chance(95) { format!("Fuzz {}", rng.below(1000)) } else { String::new() };
        let symbol = if rng.chance(95) { format!("F{}", rng.below(100)) } else { String::new() };
        let params = match variant {
            B20Variant::Asset => IB20Factory::B20AssetCreateParams {
                version: if rng.chance(95) { 1 } else { rng.below(4) as u8 },
                name,
                symbol,
                initialAdmin: admin,
                decimals: *rng.pick(&[0u8, 6, 8, 18, 18, 18, 19, 36, 255]),
            }
            .abi_encode(),
            B20Variant::Stablecoin => IB20Factory::B20StablecoinCreateParams {
                version: if rng.chance(95) { 1 } else { rng.below(4) as u8 },
                name,
                symbol,
                initialAdmin: admin,
                currency: (*rng.pick(&CURRENCIES)).to_owned(),
            }
            .abi_encode(),
        };
        let mut init_calls = Vec::new();
        for _ in 0..rng.below(4) {
            let call: Vec<u8> = match rng.below(4) {
                0 => IB20::grantRoleCall { role: B20TokenRole::Mint.id(), account: admin }
                    .abi_encode(),
                1 => IB20::mintCall { to: Self::user(rng), amount: Registry::amount(rng) }
                    .abi_encode(),
                2 => IB20::updateSupplyCapCall { newSupplyCap: Registry::amount(rng) }.abi_encode(),
                _ => {
                    IB20::pauseCall { features: vec![IB20::PausableFeature::TRANSFER] }.abi_encode()
                }
            };
            init_calls.push(Bytes::from(call));
        }
        let input = IB20Factory::createB20Call {
            variant: variant.abi(),
            salt,
            params: params.into(),
            initCalls: init_calls,
        }
        .abi_encode();
        let mut call = Self::call(
            B20FactoryStorage::ADDRESS,
            input,
            Caller::Account(creator),
            "factory.createB20",
        );
        if rng.chance(3) {
            call.value = U256::from(1);
        }
        call
    }

    /// The asset-only operator role (`keccak256("OPERATOR_ROLE")`) that guards `announce` and
    /// the multiplier updates. The precompile crate keeps it crate-private.
    pub const ASSET_OPERATOR_ROLE: B256 =
        b256!("0x97667070c54ef182b0f5858b034beac1b6f3089aa2d3188bb1e8929f4fa9b929");

    /// Name of tokens created by [`Self::seeded_token_input`].
    pub const fn seeded_token_name(variant: B20Variant) -> &'static str {
        match variant {
            B20Variant::Asset => "Fuzz Asset",
            B20Variant::Stablecoin => "Fuzz USD",
        }
    }

    /// Calldata creating a well-formed token administered by the operator, which also holds every
    /// other role (including the asset operator role for assets), with `holders` each minted 10^24 units, followed by the `extra` init calls.
    pub fn seeded_token_input(
        salt: B256,
        variant: B20Variant,
        holders: &[Address],
        extra: &[Bytes],
    ) -> Bytes {
        let operator = Keys::get().operator();
        let params = match variant {
            B20Variant::Asset => IB20Factory::B20AssetCreateParams {
                version: 1,
                name: Self::seeded_token_name(variant).into(),
                symbol: "FA".into(),
                initialAdmin: operator,
                decimals: 18,
            }
            .abi_encode(),
            B20Variant::Stablecoin => IB20Factory::B20StablecoinCreateParams {
                version: 1,
                name: Self::seeded_token_name(variant).into(),
                symbol: "FUSD".into(),
                initialAdmin: operator,
                currency: "USD".into(),
            }
            .abi_encode(),
        };
        let mut init_calls: Vec<Bytes> = [
            B20TokenRole::Mint,
            B20TokenRole::Burn,
            B20TokenRole::BurnBlocked,
            B20TokenRole::Seize,
            B20TokenRole::Pause,
            B20TokenRole::Unpause,
            B20TokenRole::Metadata,
        ]
        .into_iter()
        .map(|role| IB20::grantRoleCall { role: role.id(), account: operator }.abi_encode().into())
        .collect();
        if variant == B20Variant::Asset {
            init_calls.push(
                IB20::grantRoleCall { role: Self::ASSET_OPERATOR_ROLE, account: operator }
                    .abi_encode()
                    .into(),
            );
        }
        for holder in holders {
            init_calls.push(
                IB20::mintCall { to: *holder, amount: U256::from(10).pow(U256::from(24)) }
                    .abi_encode()
                    .into(),
            );
        }
        init_calls.extend_from_slice(extra);
        IB20Factory::createB20Call {
            variant: variant.abi(),
            salt,
            params: params.into(),
            initCalls: init_calls,
        }
        .abi_encode()
        .into()
    }

    /// A random native call that uses what the registry knows.
    pub fn random(rng: &mut Rng, upgrade: BaseUpgrade, registry: &Registry) -> NativeCall {
        let cobalt = upgrade.is_feature_enabled(BaseFeature::TransactionContextPrecompile);
        let mut weights = vec![10u64, 35, 15, 8, 4];
        if !cobalt {
            weights[4] = 0;
        }
        match rng.weighted(&weights) {
            0 => Self::factory(rng, registry),
            1 => match registry.tokens.len() {
                0 => Self::factory(rng, registry),
                n => {
                    let token = &registry.tokens[rng.index(n)];
                    Self::token(rng, upgrade, registry, token)
                }
            },
            2 => Self::policy(rng, upgrade, registry),
            3 => Self::activation(rng, upgrade),
            _ => Self::context(rng),
        }
    }

    fn factory(rng: &mut Rng, registry: &Registry) -> NativeCall {
        match rng.below(6) {
            0..=2 => {
                let creator = Self::user(rng);
                let salt = if rng.chance(20) { B256::ZERO } else { rng.b256() };
                let variant =
                    if rng.chance(50) { B20Variant::Asset } else { B20Variant::Stablecoin };
                Self::create_token(rng, creator, salt, variant)
            }
            3 => {
                let variant = if rng.chance(50) {
                    IB20Factory::B20Variant::ASSET
                } else {
                    IB20Factory::B20Variant::STABLECOIN
                };
                let input = IB20Factory::getB20AddressCall {
                    variant,
                    sender: Self::user(rng),
                    salt: rng.b256(),
                }
                .abi_encode();
                Self::call(
                    B20FactoryStorage::ADDRESS,
                    input,
                    Caller::Anyone,
                    "factory.getB20Address",
                )
            }
            4 => {
                let token = registry.tokens.first().map_or_else(|| rng.address(), |t| t.address);
                let input = IB20Factory::isB20Call { token }.abi_encode();
                Self::call(B20FactoryStorage::ADDRESS, input, Caller::Anyone, "factory.isB20")
            }
            _ => {
                let token = registry.tokens.last().map_or_else(|| rng.address(), |t| t.address);
                let input = IB20Factory::isB20InitializedCall { token }.abi_encode();
                Self::call(
                    B20FactoryStorage::ADDRESS,
                    input,
                    Caller::Anyone,
                    "factory.isB20Initialized",
                )
            }
        }
    }

    /// Picks a sender holding `role` on `token` most of the time, someone else otherwise.
    fn role_caller(rng: &mut Rng, token: &Token, role: B256) -> Caller {
        let holders = token.holders_of(role);
        if !holders.is_empty() && rng.chance(80) {
            Caller::Account(holders[rng.index(holders.len())])
        } else {
            let admins = token.holders_of(B20TokenRole::DefaultAdmin.id());
            if !admins.is_empty() && rng.chance(50) {
                Caller::Account(admins[rng.index(admins.len())])
            } else {
                Caller::Anyone
            }
        }
    }

    fn holder(rng: &mut Rng, token: &Token) -> Caller {
        let holders: Vec<Address> = token.holders.iter().copied().collect();
        if !holders.is_empty() && rng.chance(85) {
            Caller::Account(holders[rng.index(holders.len())])
        } else {
            Caller::Anyone
        }
    }

    /// An EIP-2612 permit, signed by a generated owner over the token's EIP-712 domain most of
    /// the time, with a stale or random nonce, an expired deadline, or a junk signature otherwise.
    /// Fresh owners have permit nonce 0; repeated permits from one owner exercise the nonce check.
    fn permit(rng: &mut Rng, token: &Token, value: U256) -> IB20::permitCall {
        let keys = Keys::get();
        let owner_key = rng.index(Keys::COUNT);
        let owner = keys.address(owner_key);
        let spender = Self::user(rng);
        let deadline = match rng.below(10) {
            0 => U256::from(rng.below(1_000_000)),
            1 => U256::from(1_700_000_000 + rng.below(200_000_000)),
            _ => U256::MAX,
        };
        let nonce = U256::from(if rng.chance(85) { 0 } else { rng.range(1, 3) });
        let args =
            PermitArgs { owner, spender, value, deadline, v: 0, r: B256::ZERO, s: B256::ZERO };
        let domain = keccak256(
            (
                keccak256(
                    "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
                ),
                keccak256(token.name.as_bytes()),
                keccak256("1"),
                U256::from(ChainConfig::mainnet().chain_id),
                token.address,
            )
                .abi_encode(),
        );
        let signature = keys.signers[owner_key]
            .sign_hash_sync(&args.signing_hash(domain, nonce))
            .expect("signing never fails");
        let mut call = IB20::permitCall {
            owner,
            spender,
            value,
            deadline,
            v: 27 + u8::from(signature.v()),
            r: signature.r().into(),
            s: signature.s().into(),
        };
        if rng.chance(10) {
            call.r = rng.b256();
        } else if rng.chance(5) {
            call.v = rng.below(256) as u8;
        }
        call
    }

    fn token(
        rng: &mut Rng,
        upgrade: BaseUpgrade,
        registry: &Registry,
        token: &Token,
    ) -> NativeCall {
        let to = token.address;
        let amount = Registry::amount(rng);
        let recipient = Self::any_account(rng, registry);
        let memo = rng.b256();
        let roles = [
            B20TokenRole::DefaultAdmin,
            B20TokenRole::Mint,
            B20TokenRole::Burn,
            B20TokenRole::BurnBlocked,
            B20TokenRole::Seize,
            B20TokenRole::Pause,
            B20TokenRole::Unpause,
            B20TokenRole::Metadata,
        ];
        let role = rng.pick(&roles).id();
        let features = {
            let all = [
                IB20::PausableFeature::TRANSFER,
                IB20::PausableFeature::MINT,
                IB20::PausableFeature::BURN,
                IB20::PausableFeature::SEIZE,
            ];
            let n = rng.range(0, 3) as usize;
            (0..n).map(|_| *rng.pick(&all)).collect::<Vec<_>>()
        };
        let scope = rng
            .pick(&[
                B20PolicyType::TransferSender,
                B20PolicyType::TransferReceiver,
                B20PolicyType::TransferExecutor,
                B20PolicyType::MintReceiver,
                B20PolicyType::SeizeExempt,
                B20PolicyType::SeizeReceiver,
            ])
            .id();
        let policy_id = if !registry.policies.is_empty() && rng.chance(70) {
            registry.policies[rng.index(registry.policies.len())].id
        } else {
            {
                let small = rng.below(10);
                *rng.pick(&[0u64, 1, (1 << 56) | 1, u64::MAX, small])
            }
        };
        // A scheduled UI multiplier update can only be cancelled before it matures, so try
        // soon after one is seen.
        let arm = if token.ui_update_pending && rng.chance(20) { 25 } else { rng.below(30) };
        let mut call = match arm {
            0..=4 => {
                let input = IB20::transferCall { to: recipient, amount }.abi_encode();
                Self::call(to, input, Self::holder(rng, token), "b20.transfer")
            }
            5 => {
                let input = IB20::transferWithMemoCall { to: recipient, amount, memo }.abi_encode();
                Self::call(to, input, Self::holder(rng, token), "b20.transferWithMemo")
            }
            6..=7 => {
                let approvals: Vec<(Address, Address)> = token.approvals.iter().copied().collect();
                let (from, spender) = if !approvals.is_empty() && rng.chance(70) {
                    approvals[rng.index(approvals.len())]
                } else {
                    (Self::any_account(rng, registry), Self::user(rng))
                };
                // Mostly within typical approvals.
                let amount =
                    if rng.chance(70) { U256::from(rng.range(1, 1_000_000)) } else { amount };
                let input = if rng.chance(50) {
                    IB20::transferFromCall { from, to: recipient, amount }.abi_encode()
                } else {
                    IB20::transferFromWithMemoCall { from, to: recipient, amount, memo }
                        .abi_encode()
                };
                Self::call(to, input, Caller::Account(spender), "b20.transferFrom")
            }
            8..=9 => {
                let spender = Self::any_account(rng, registry);
                let input = IB20::approveCall { spender, amount }.abi_encode();
                Self::call(to, input, Self::holder(rng, token), "b20.approve")
            }
            10..=12 => {
                let input = if rng.chance(70) {
                    IB20::mintCall { to: recipient, amount }.abi_encode()
                } else {
                    IB20::mintWithMemoCall { to: recipient, amount, memo }.abi_encode()
                };
                Self::call(
                    to,
                    input,
                    Self::role_caller(rng, token, B20TokenRole::Mint.id()),
                    "b20.mint",
                )
            }
            13 => {
                let input = IB20::burnCall { amount }.abi_encode();
                Self::call(
                    to,
                    input,
                    Self::role_caller(rng, token, B20TokenRole::Burn.id()),
                    "b20.burn",
                )
            }
            14 => {
                let from = if rng.chance(70) {
                    Keys::get().address(Bootstrap::SEEDED_BLOCKED)
                } else {
                    recipient
                };
                let input = IB20::burnBlockedCall { from, amount }.abi_encode();
                Self::call(
                    to,
                    input,
                    Self::role_caller(rng, token, B20TokenRole::BurnBlocked.id()),
                    "b20.burnBlocked",
                )
            }
            15 => {
                let from = if rng.chance(60) && !token.holders.is_empty() {
                    *token.holders.iter().nth(rng.index(token.holders.len())).expect("in range")
                } else {
                    Self::any_account(rng, registry)
                };
                let input =
                    IB20::seizeWithMemoCall { from, to: recipient, amount, memo }.abi_encode();
                Self::call(
                    to,
                    input,
                    Self::role_caller(rng, token, B20TokenRole::Seize.id()),
                    "b20.seizeWithMemo",
                )
            }
            16..=17 => {
                let account = Self::any_account(rng, registry);
                let input = IB20::grantRoleCall { role, account }.abi_encode();
                Self::call(
                    to,
                    input,
                    Self::role_caller(rng, token, B20TokenRole::DefaultAdmin.id()),
                    "b20.grantRole",
                )
            }
            18 => {
                let holders = token.holders_of(role);
                let account = holders.first().copied().unwrap_or_else(|| Self::user(rng));
                let input = if rng.chance(50) {
                    IB20::revokeRoleCall { role, account }.abi_encode()
                } else {
                    IB20::renounceRoleCall { role, callerConfirmation: account }.abi_encode()
                };
                Self::call(
                    to,
                    input,
                    Self::role_caller(rng, token, B20TokenRole::DefaultAdmin.id()),
                    "b20.revokeRole",
                )
            }
            19 => {
                let input = IB20::pauseCall { features }.abi_encode();
                Self::call(
                    to,
                    input,
                    Self::role_caller(rng, token, B20TokenRole::Pause.id()),
                    "b20.pause",
                )
            }
            20 => {
                let input = IB20::unpauseCall { features }.abi_encode();
                Self::call(
                    to,
                    input,
                    Self::role_caller(rng, token, B20TokenRole::Unpause.id()),
                    "b20.unpause",
                )
            }
            21 => {
                let input = IB20::updatePolicyCall { policyScope: scope, newPolicyId: policy_id }
                    .abi_encode();
                Self::call(
                    to,
                    input,
                    Self::role_caller(rng, token, B20TokenRole::DefaultAdmin.id()),
                    "b20.updatePolicy",
                )
            }
            22 => {
                // Around the seeded supply (12 holders x 10^24) and the hard cap.
                let cap = *rng.pick(&[
                    B20_MAX_SUPPLY_CAP,
                    B20_MAX_SUPPLY_CAP + U256::from(1),
                    U256::from(10).pow(U256::from(30)),
                    U256::from(10).pow(U256::from(25)),
                    amount,
                ]);
                let input = IB20::updateSupplyCapCall { newSupplyCap: cap }.abi_encode();
                Self::call(
                    to,
                    input,
                    Self::role_caller(rng, token, B20TokenRole::DefaultAdmin.id()),
                    "b20.updateSupplyCap",
                )
            }
            23 => {
                let input = match rng.below(3) {
                    0 => IB20::updateNameCall { newName: format!("Renamed {}", rng.below(100)) }
                        .abi_encode(),
                    1 => {
                        IB20::updateSymbolCall { newSymbol: "R".repeat(rng.range(0, 40) as usize) }
                            .abi_encode()
                    }
                    _ => IB20::updateContractURICall { newURI: "ipfs://fuzz".to_owned() }
                        .abi_encode(),
                };
                Self::call(
                    to,
                    input,
                    Self::role_caller(rng, token, B20TokenRole::Metadata.id()),
                    "b20.updateMetadata",
                )
            }
            24 => {
                let input = Self::permit(rng, token, amount).abi_encode();
                Self::call(to, input, Caller::Anyone, "b20.permit")
            }
            25 if token.variant == B20Variant::Asset => Self::asset(rng, upgrade, token, registry),
            26 if token.variant == B20Variant::Stablecoin => Self::call(
                to,
                IB20Stablecoin::currencyCall {}.abi_encode(),
                Caller::Anyone,
                "stablecoin.currency",
            ),
            27 => {
                let input = IB20::renounceLastAdminCall {}.abi_encode();
                Self::call(
                    to,
                    input,
                    Self::role_caller(rng, token, B20TokenRole::DefaultAdmin.id()),
                    "b20.renounceLastAdmin",
                )
            }
            _ => {
                let account = Self::any_account(rng, registry);
                let (input, label) = match rng.below(10) {
                    0 => (IB20::balanceOfCall { account }.abi_encode(), "b20.balanceOf"),
                    1 => (
                        IB20::allowanceCall { owner: account, spender: Self::user(rng) }
                            .abi_encode(),
                        "b20.allowance",
                    ),
                    2 => (IB20::totalSupplyCall {}.abi_encode(), "b20.totalSupply"),
                    3 => (IB20::hasRoleCall { role, account }.abi_encode(), "b20.hasRole"),
                    4 => (IB20::pausedFeaturesCall {}.abi_encode(), "b20.pausedFeatures"),
                    5 => (IB20::policyIdCall { policyScope: scope }.abi_encode(), "b20.policyId"),
                    6 => (IB20::DOMAIN_SEPARATORCall {}.abi_encode(), "b20.DOMAIN_SEPARATOR"),
                    7 => (IB20::eip712DomainCall {}.abi_encode(), "b20.eip712Domain"),
                    8 => (IB20::nameCall {}.abi_encode(), "b20.name"),
                    _ => (IB20::supplyCapCall {}.abi_encode(), "b20.supplyCap"),
                };
                Self::call(to, input, Caller::Anyone, label)
            }
        };
        if rng.chance(3) {
            call.value = U256::from(rng.range(1, 1000));
        }
        call
    }

    fn asset(
        rng: &mut Rng,
        upgrade: BaseUpgrade,
        token: &Token,
        registry: &Registry,
    ) -> NativeCall {
        let to = token.address;
        let admin = Self::role_caller(rng, token, Self::ASSET_OPERATOR_ROLE);
        let wad = U256::from(10).pow(U256::from(18));
        let multiplier = *rng.pick(&[
            U256::ZERO,
            wad,
            wad * U256::from(2),
            U256::from(1),
            U256::MAX,
            wad / U256::from(3),
        ]);
        let arm = if token.ui_update_pending && rng.chance(50) { 2 } else { rng.below(8) };
        match arm {
            0 => Self::call(
                to,
                IB20Asset::updateMultiplierCall { newMultiplier: multiplier }.abi_encode(),
                admin,
                "asset.updateMultiplier",
            ),
            1 => {
                let effective = U256::from(rng.below(2_000_000_000) + 1_700_000_000);
                let input = IB20Asset::updateUIMultiplierCall {
                    newMultiplier: multiplier,
                    effectiveAt: effective,
                }
                .abi_encode();
                Self::call(to, input, admin, "asset.updateUIMultiplier")
            }
            2 => Self::call(
                to,
                IB20Asset::cancelUIMultiplierUpdateCall {}.abi_encode(),
                admin,
                "asset.cancelUIMultiplierUpdate",
            ),
            3 => {
                let n = rng.range(0, 4) as usize;
                let recipients = (0..n).map(|_| Self::any_account(rng, registry)).collect();
                let m = if rng.chance(85) { n } else { n + 1 };
                let amounts = (0..m).map(|_| Registry::amount(rng)).collect();
                let input = IB20Asset::batchMintCall { recipients, amounts }.abi_encode();
                Self::call(
                    to,
                    input,
                    Self::role_caller(rng, token, B20TokenRole::Mint.id()),
                    "asset.batchMint",
                )
            }
            4 => {
                let internal = vec![Bytes::from(
                    IB20::mintCall { to: Self::user(rng), amount: U256::from(1) }.abi_encode(),
                )];
                let input = IB20Asset::announceCall {
                    internalCalls: if rng.chance(50) { internal } else { Vec::new() },
                    id: format!("a{}", rng.below(5)),
                    description: "fuzz".to_owned(),
                    uri: String::new(),
                }
                .abi_encode();
                Self::call(to, input, admin, "asset.announce")
            }
            5 => {
                let input = IB20Asset::updateExtraMetadataCall {
                    key: format!("k{}", rng.below(4)),
                    value: if rng.chance(30) {
                        String::new()
                    } else {
                        "v".repeat(rng.range(1, 80) as usize)
                    },
                }
                .abi_encode();
                Self::call(
                    to,
                    input,
                    Self::role_caller(rng, token, B20TokenRole::Metadata.id()),
                    "asset.updateExtraMetadata",
                )
            }
            6 if upgrade.is_feature_enabled(BaseFeature::AssetPrecompileV2) => {
                let id =
                    *rng.pick(&[[0x01, 0xff, 0xc9, 0xa7], [0x57, 0x85, 0x4f, 0xc3], [0, 0, 0, 0]]);
                Self::call(
                    to,
                    IB20Asset::supportsInterfaceCall { interfaceId: id.into() }.abi_encode(),
                    Caller::Anyone,
                    "asset.supportsInterface",
                )
            }
            _ => {
                let account = Self::any_account(rng, registry);
                let (input, label) = match rng.below(5) {
                    0 => (IB20Asset::multiplierCall {}.abi_encode(), "asset.multiplier"),
                    1 => (IB20Asset::balanceOfUICall { account }.abi_encode(), "asset.balanceOfUI"),
                    2 => (
                        IB20Asset::toUIAmountCall { rawAmount: Registry::amount(rng) }.abi_encode(),
                        "asset.toUIAmount",
                    ),
                    3 => (
                        IB20Asset::fromUIAmountCall { uiAmount: Registry::amount(rng) }
                            .abi_encode(),
                        "asset.fromUIAmount",
                    ),
                    _ => (IB20Asset::totalSupplyUICall {}.abi_encode(), "asset.totalSupplyUI"),
                };
                Self::call(to, input, Caller::Anyone, label)
            }
        }
    }

    fn policy(rng: &mut Rng, upgrade: BaseUpgrade, registry: &Registry) -> NativeCall {
        let to = PolicyRegistryStorage::ADDRESS;
        let composite = upgrade.is_feature_enabled(BaseFeature::PolicyPrecompileV2);
        let kinds = if composite { 4 } else { 2 };
        let kind = IPolicyRegistry::PolicyType::try_from(rng.below(kinds) as u8)
            .unwrap_or(IPolicyRegistry::PolicyType::ALLOWLIST);
        let simple = IPolicyRegistry::PolicyType::try_from(rng.below(2) as u8)
            .unwrap_or(IPolicyRegistry::PolicyType::BLOCKLIST);
        let accounts: Vec<Address> =
            (0..rng.range(0, 4)).map(|_| Self::any_account(rng, registry)).collect();
        let existing = (!registry.policies.is_empty())
            .then(|| registry.policies[rng.index(registry.policies.len())].clone());
        let policy_id = existing.as_ref().map_or_else(|| rng.below(5), |p| p.id);
        let admin_caller = existing.as_ref().map_or(Caller::Anyone, |p| {
            if rng.chance(80) { Caller::Account(p.admin) } else { Caller::Anyone }
        });
        match rng.below(12) {
            0..=1 => {
                let admin = if rng.chance(80) {
                    Keys::get().operator()
                } else {
                    Self::any_account(rng, registry)
                };
                Self::call(
                    to,
                    IPolicyRegistry::createPolicyCall { admin, policyType: simple }.abi_encode(),
                    Caller::Account(Keys::get().operator()),
                    "policy.createPolicy",
                )
            }
            2 => {
                let admin = Keys::get().operator();
                let input = IPolicyRegistry::createPolicyWithAccountsCall {
                    admin,
                    policyType: simple,
                    accounts,
                }
                .abi_encode();
                Self::call(to, input, Caller::Anyone, "policy.createPolicyWithAccounts")
            }
            3 if composite => {
                let children: Vec<u64> = (0..rng.range(0, 4))
                    .map(|_| {
                        registry
                            .policies
                            .get(rng.index(registry.policies.len().max(1)))
                            .map_or(1, |p| p.id)
                    })
                    .collect();
                let input = IPolicyRegistry::createCompositePolicyCall {
                    admin: Keys::get().operator(),
                    policyType: kind,
                    childPolicyIds: children,
                }
                .abi_encode();
                Self::call(to, input, Caller::Anyone, "policy.createCompositePolicy")
            }
            4 => {
                let input = IPolicyRegistry::updateAllowlistCall {
                    policyId: policy_id,
                    allowed: rng.chance(70),
                    accounts,
                }
                .abi_encode();
                Self::call(to, input, admin_caller, "policy.updateAllowlist")
            }
            5 => {
                let input = IPolicyRegistry::updateBlocklistCall {
                    policyId: policy_id,
                    blocked: rng.chance(70),
                    accounts,
                }
                .abi_encode();
                Self::call(to, input, admin_caller, "policy.updateBlocklist")
            }
            6 => {
                let input = IPolicyRegistry::stageUpdateAdminCall {
                    policyId: policy_id,
                    newAdmin: Self::user(rng),
                }
                .abi_encode();
                Self::call(to, input, admin_caller, "policy.stageUpdateAdmin")
            }
            7 => {
                // Finalizing only succeeds on a staged handoff, so prefer one when there is one.
                let staged: Vec<&Policy> =
                    registry.policies.iter().filter(|p| p.pending_admin.is_some()).collect();
                let target = if !staged.is_empty() && rng.chance(80) {
                    Some(staged[rng.index(staged.len())])
                } else {
                    existing.as_ref()
                };
                let caller =
                    target.and_then(|p| p.pending_admin).map_or(Caller::Anyone, Caller::Account);
                let policy_id = target.map_or(policy_id, |p| p.id);
                Self::call(
                    to,
                    IPolicyRegistry::finalizeUpdateAdminCall { policyId: policy_id }.abi_encode(),
                    caller,
                    "policy.finalizeUpdateAdmin",
                )
            }
            8 if rng.chance(30) => Self::call(
                to,
                IPolicyRegistry::renounceAdminCall { policyId: policy_id }.abi_encode(),
                admin_caller,
                "policy.renounceAdmin",
            ),
            9 if composite => {
                // Only a composite policy's admin can update it, and only to 2 to 4 simple
                // children, so aim mostly at a known composite with simple children.
                let composites: Vec<&Policy> =
                    registry.policies.iter().filter(|p| p.kind >= 2).collect();
                let (policy_id, caller) = if !composites.is_empty() && rng.chance(80) {
                    let p = composites[rng.index(composites.len())];
                    (p.id, if rng.chance(90) { Caller::Account(p.admin) } else { Caller::Anyone })
                } else {
                    (policy_id, admin_caller)
                };
                let simple: Vec<u64> =
                    registry.policies.iter().filter(|p| p.kind < 2).map(|p| p.id).collect();
                let children: Vec<u64> = if !simple.is_empty() && rng.chance(80) {
                    (0..rng.range(1, 6)).map(|_| simple[rng.index(simple.len())]).collect()
                } else {
                    registry.policies.iter().take(rng.range(0, 3) as usize).map(|p| p.id).collect()
                };
                let input = IPolicyRegistry::updateCompositeCall {
                    policyId: policy_id,
                    childPolicyIds: children,
                }
                .abi_encode();
                Self::call(to, input, caller, "policy.updateComposite")
            }
            _ => {
                let account = Self::any_account(rng, registry);
                let (input, label) = match rng.below(5) {
                    0 => (
                        IPolicyRegistry::isAuthorizedCall { policyId: policy_id, account }
                            .abi_encode(),
                        "policy.isAuthorized",
                    ),
                    1 => (
                        IPolicyRegistry::policyExistsCall { policyId: policy_id }.abi_encode(),
                        "policy.policyExists",
                    ),
                    2 => (
                        IPolicyRegistry::policyAdminCall { policyId: policy_id }.abi_encode(),
                        "policy.policyAdmin",
                    ),
                    3 => (
                        IPolicyRegistry::pendingPolicyAdminCall { policyId: policy_id }
                            .abi_encode(),
                        "policy.pendingPolicyAdmin",
                    ),
                    _ => (
                        IPolicyRegistry::invertedPolicyIdCall { policyId: policy_id }.abi_encode(),
                        "policy.invertedPolicyId",
                    ),
                };
                Self::call(to, input, Caller::Anyone, label)
            }
        }
    }

    /// Activation registry calls, privileged ones from the right admin most of the time.
    pub fn activation(rng: &mut Rng, upgrade: BaseUpgrade) -> NativeCall {
        let to = ActivationRegistryStorage::ADDRESS;
        let feature = match rng.below(4) {
            0 => ActivationFeature::PolicyRegistry.id(),
            1 => ActivationFeature::B20Stablecoin.id(),
            2 => ActivationFeature::B20Asset.id(),
            _ => rng.b256(),
        };
        let admin = Self::activation_admin(upgrade);
        let privileged = if rng.chance(75) { Caller::Account(admin) } else { Caller::Anyone };
        let mut call = match rng.below(8) {
            0 => Self::call(
                to,
                IActivationRegistry::activateCall { feature }.abi_encode(),
                privileged,
                "activation.activate",
            ),
            1 => Self::call(
                to,
                IActivationRegistry::deactivateCall { feature }.abi_encode(),
                privileged,
                "activation.deactivate",
            ),
            2 => {
                let new_admin =
                    if rng.chance(70) { Keys::get().operator() } else { Self::user(rng) };
                Self::call(
                    to,
                    IActivationRegistry::setAdminCall { newAdmin: new_admin }.abi_encode(),
                    privileged,
                    "activation.setAdmin",
                )
            }
            3 => Self::call(
                to,
                IActivationRegistry::adminCall {}.abi_encode(),
                Caller::Anyone,
                "activation.admin",
            ),
            4 => Self::call(
                to,
                IActivationRegistry::checkActivatedCall { feature }.abi_encode(),
                Caller::Anyone,
                "activation.checkActivated",
            ),
            _ => Self::call(
                to,
                IActivationRegistry::isActivatedCall { feature }.abi_encode(),
                Caller::Anyone,
                "activation.isActivated",
            ),
        };
        if rng.chance(5) {
            call.value = U256::from(1);
        }
        call
    }

    fn context(rng: &mut Rng) -> NativeCall {
        match rng.below(4) {
            0 => Self::call(
                TxContextStorage::ADDRESS,
                ITransactionContext::getTransactionSenderCall {}.abi_encode(),
                Caller::Anyone,
                "txContext.getTransactionSender",
            ),
            1 => Self::call(
                TxContextStorage::ADDRESS,
                ITransactionContext::getTransactionPayerCall {}.abi_encode(),
                Caller::Anyone,
                "txContext.getTransactionPayer",
            ),
            2 => Self::call(
                TxContextStorage::ADDRESS,
                ITransactionContext::getTransactionSenderActorIdCall {}.abi_encode(),
                Caller::Anyone,
                "txContext.getTransactionSenderActorId",
            ),
            _ => {
                let input = INonceManager::getNonceCall {
                    account: Self::user(rng),
                    nonceKey: rng.edge_u256(),
                }
                .abi_encode();
                Self::call(
                    NonceManagerStorage::ADDRESS,
                    input,
                    Caller::Anyone,
                    "nonceManager.getNonce",
                )
            }
        }
    }
}
