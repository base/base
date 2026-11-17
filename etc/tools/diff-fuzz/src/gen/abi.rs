//! Selector-level calls to Base's native precompiles: every function of every interface version
//! of each precompile, with arguments generated from the function signature and biased towards
//! values the precompiles act on (known tokens, policies, roles, scopes, and features), called
//! from right and wrong callers and with well-formed or mutated ABI encodings.

use std::sync::OnceLock;

use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_sol_types::{SolCall, SolValue};
use base_common_chains::MAINNET_BERYL_ACTIVATION_ADMIN_ADDRESS;
use base_common_genesis::{BaseFeature, BaseUpgrade};
use base_common_precompiles::{
    ActivationFeature, ActivationRegistryStorage, B20_MAX_SUPPLY_CAP, B20FactoryStorage,
    B20PolicyType, B20TokenRole, B20Variant, ERC165_INTERFACE_ID, ERC8056_INTERFACE_IDS,
    IActivationRegistry, IB20, IB20AssetV1, IB20AssetV2, IB20Factory, IB20Stablecoin, IB20V1,
    IB20V2, INonceManager, IPolicyRegistryV1, IPolicyRegistryV2, IPolicyRegistryV3,
    ITransactionContext, NonceManagerStorage, PolicyRegistryStorage, TxContextStorage,
};

use crate::r#gen::{keys::Keys, native::Native, registry::Registry, rng::Rng};

/// A parsed ABI type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbiType {
    /// `address`.
    Address,
    /// `bool`.
    Bool,
    /// `uintN`.
    Uint(usize),
    /// `intN`.
    Int(usize),
    /// `bytesN`.
    FixedBytes(usize),
    /// `bytes`.
    Bytes,
    /// `string`.
    String,
    /// `T[]`.
    Array(Box<Self>),
    /// `T[N]`.
    FixedArray(Box<Self>, usize),
    /// `(T, ...)`.
    Tuple(Vec<Self>),
}

impl AbiType {
    /// Parses the parameter list of a function signature such as `f(uint8,(address,bytes)[])`.
    pub fn params(signature: &str) -> Vec<Self> {
        let open = signature.find('(').expect("signature has parameters");
        match Self::parse(&signature[open..]) {
            Self::Tuple(types) => types,
            other => vec![other],
        }
    }

    /// Parses one type.
    pub fn parse(text: &str) -> Self {
        let text = text.trim();
        if let Some(inner) = text.strip_suffix(']') {
            let open = inner.rfind('[').expect("array brackets match");
            let element = Box::new(Self::parse(&inner[..open]));
            let len = &inner[open + 1..];
            return if len.is_empty() {
                Self::Array(element)
            } else {
                Self::FixedArray(element, len.parse().expect("array length"))
            };
        }
        if let Some(body) = text.strip_prefix('(') {
            let body = body.strip_suffix(')').expect("tuple parentheses match");
            let mut parts = Vec::new();
            let (mut depth, mut start) = (0usize, 0usize);
            for (i, c) in body.char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    ',' if depth == 0 => {
                        parts.push(Self::parse(&body[start..i]));
                        start = i + 1;
                    }
                    _ => {}
                }
            }
            if !body[start..].trim().is_empty() {
                parts.push(Self::parse(&body[start..]));
            }
            return Self::Tuple(parts);
        }
        match text {
            "address" => Self::Address,
            "bool" => Self::Bool,
            "bytes" => Self::Bytes,
            "string" => Self::String,
            _ if text.starts_with("uint") => Self::Uint(text[4..].parse().unwrap_or(256)),
            _ if text.starts_with("int") => Self::Int(text[3..].parse().unwrap_or(256)),
            _ if text.starts_with("bytes") => Self::FixedBytes(text[5..].parse().expect("bytesN")),
            _ => panic!("unsupported ABI type {text}"),
        }
    }

    /// Whether the encoding is dynamic (referenced through an offset).
    pub fn is_dynamic(&self) -> bool {
        match self {
            Self::Bytes | Self::String | Self::Array(_) => true,
            Self::FixedArray(element, _) => element.is_dynamic(),
            Self::Tuple(types) => types.iter().any(Self::is_dynamic),
            _ => false,
        }
    }
}

/// A native precompile family a selector call targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeTarget {
    /// A B-20 asset token.
    Asset,
    /// A B-20 stablecoin token.
    Stablecoin,
    /// The token factory.
    Factory,
    /// The policy registry.
    Policy,
    /// The activation registry.
    Activation,
    /// The nonce manager.
    NonceManager,
    /// The transaction-context precompile.
    TxContext,
    /// An address in the asset token range that was never created.
    UninitializedToken,
}

impl NativeTarget {
    /// Every target, indexed by descriptor fields.
    pub const ALL: [Self; 8] = [
        Self::Asset,
        Self::Stablecoin,
        Self::Factory,
        Self::Policy,
        Self::Activation,
        Self::NonceManager,
        Self::TxContext,
        Self::UninitializedToken,
    ];

    /// Every function signature of every interface version of the target, deduplicated.
    pub fn signatures(self) -> &'static [&'static str] {
        fn collect(lists: &[&[&'static str]]) -> Vec<&'static str> {
            let mut all: Vec<&'static str> = lists.iter().flat_map(|l| l.iter().copied()).collect();
            all.sort_unstable();
            all.dedup();
            all
        }
        static TABLES: OnceLock<Vec<Vec<&'static str>>> = OnceLock::new();
        let tables = TABLES.get_or_init(|| {
            let token = [IB20V1::IB20Calls::SIGNATURES, IB20V2::IB20Calls::SIGNATURES];
            vec![
                collect(&[
                    token[0],
                    token[1],
                    IB20AssetV1::IB20AssetCalls::SIGNATURES,
                    IB20AssetV2::IB20AssetCalls::SIGNATURES,
                ]),
                collect(&[token[0], token[1], IB20Stablecoin::IB20StablecoinCalls::SIGNATURES]),
                collect(&[IB20Factory::IB20FactoryCalls::SIGNATURES]),
                collect(&[
                    IPolicyRegistryV1::IPolicyRegistryCalls::SIGNATURES,
                    IPolicyRegistryV2::IPolicyRegistryCalls::SIGNATURES,
                    IPolicyRegistryV3::IPolicyRegistryCalls::SIGNATURES,
                ]),
                collect(&[IActivationRegistry::IActivationRegistryCalls::SIGNATURES]),
                collect(&[INonceManager::INonceManagerCalls::SIGNATURES]),
                collect(&[ITransactionContext::ITransactionContextCalls::SIGNATURES]),
            ]
        });
        let index = match self {
            Self::Asset | Self::UninitializedToken => 0,
            Self::Stablecoin => 1,
            Self::Factory => 2,
            Self::Policy => 3,
            Self::Activation => 4,
            Self::NonceManager => 5,
            Self::TxContext => 6,
        };
        &tables[index]
    }

    /// Whether the target exists under `upgrade`.
    pub const fn active(self, upgrade: BaseUpgrade) -> bool {
        match self {
            Self::TxContext => {
                upgrade.is_feature_enabled(BaseFeature::TransactionContextPrecompile)
            }
            Self::NonceManager => upgrade.is_feature_enabled(BaseFeature::NonceManagerPrecompile),
            _ => Native::active(upgrade),
        }
    }
}

/// How a selector call's ABI encoding is mutated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbiMutation {
    /// Well formed.
    None,
    /// Truncated to a random length.
    Truncate,
    /// Random bytes appended.
    Append,
    /// One argument word overwritten with a boundary value.
    Word,
    /// The bare selector.
    SelectorOnly,
}

/// Generates selector-level native precompile calls.
#[derive(Debug)]
pub struct NativeAbi<'a> {
    /// Random source for argument values.
    pub rng: &'a mut Rng,
    /// What the chain knows.
    pub registry: &'a Registry,
    /// The callee.
    pub to: Address,
}

impl<'a> NativeAbi<'a> {
    /// The callee address for `target`, using a token the registry knows when there is one.
    pub fn address(rng: &mut Rng, registry: &Registry, target: NativeTarget) -> Address {
        let token = |variant: B20Variant, rng: &mut Rng| {
            let tokens: Vec<Address> = registry
                .tokens
                .iter()
                .filter(|t| t.variant == variant)
                .map(|t| t.address)
                .collect();
            if tokens.is_empty() {
                variant.compute_address(Keys::get().operator(), rng.b256()).0
            } else {
                tokens[rng.index(tokens.len())]
            }
        };
        match target {
            NativeTarget::Asset => token(B20Variant::Asset, rng),
            NativeTarget::Stablecoin => token(B20Variant::Stablecoin, rng),
            NativeTarget::Factory => B20FactoryStorage::ADDRESS,
            NativeTarget::Policy => PolicyRegistryStorage::ADDRESS,
            NativeTarget::Activation => ActivationRegistryStorage::ADDRESS,
            NativeTarget::NonceManager => NonceManagerStorage::ADDRESS,
            NativeTarget::TxContext => TxContextStorage::ADDRESS,
            NativeTarget::UninitializedToken => {
                B20Variant::Asset.compute_address(Keys::get().address(5), rng.b256()).0
            }
        }
    }

    /// Calldata for `signature` with generated arguments, then `mutation` applied.
    pub fn calldata(&mut self, signature: &str, mutation: AbiMutation) -> Bytes {
        let selector = alloy_primitives::keccak256(signature.as_bytes());
        let mut out = selector[..4].to_vec();
        out.extend(self.encode_tuple(&AbiType::params(signature)));
        match mutation {
            AbiMutation::None => {}
            AbiMutation::Truncate => {
                let len = self.rng.index(out.len() + 1);
                out.truncate(len);
            }
            AbiMutation::Append => out.extend(self.rng.bytes_between(1, 64)),
            AbiMutation::Word if out.len() >= 36 => {
                let words = (out.len() - 4) / 32;
                let at = 4 + 32 * self.rng.index(words);
                out[at..at + 32].copy_from_slice(&self.rng.edge_u256().to_be_bytes::<32>());
            }
            AbiMutation::Word => out.extend(self.rng.edge_u256().to_be_bytes::<32>()),
            AbiMutation::SelectorOnly => out.truncate(4),
        }
        out.into()
    }

    fn encode_tuple(&mut self, types: &[AbiType]) -> Vec<u8> {
        let parts: Vec<(Vec<u8>, bool)> =
            types.iter().map(|ty| (self.encode_value(ty), ty.is_dynamic())).collect();
        let head_len: usize =
            parts.iter().map(|(bytes, dynamic)| if *dynamic { 32 } else { bytes.len() }).sum();
        let (mut head, mut tail) = (Vec::new(), Vec::new());
        for (bytes, dynamic) in parts {
            if dynamic {
                head.extend(U256::from(head_len + tail.len()).to_be_bytes::<32>());
                tail.extend(bytes);
            } else {
                head.extend(bytes);
            }
        }
        head.extend(tail);
        head
    }

    fn encode_bytes(data: &[u8]) -> Vec<u8> {
        let mut out = U256::from(data.len()).to_be_bytes::<32>().to_vec();
        out.extend_from_slice(data);
        out.resize(32 + data.len().div_ceil(32) * 32, 0);
        out
    }

    fn encode_value(&mut self, ty: &AbiType) -> Vec<u8> {
        match ty {
            AbiType::Bytes => {
                let data = self.bytes_value();
                Self::encode_bytes(&data)
            }
            AbiType::String => {
                let text = self.string_value();
                Self::encode_bytes(text.as_bytes())
            }
            AbiType::Array(element) => {
                let len = match self.rng.below(10) {
                    0 => 0,
                    1 => self.rng.range(5, 40) as usize,
                    _ => self.rng.range(1, 4) as usize,
                };
                let mut out = U256::from(len).to_be_bytes::<32>().to_vec();
                out.extend(self.encode_tuple(&vec![(**element).clone(); len]));
                out
            }
            AbiType::FixedArray(element, len) => {
                self.encode_tuple(&vec![(**element).clone(); *len])
            }
            AbiType::Tuple(types) => self.encode_tuple(types),
            word => self.word_value(word).to_be_bytes::<32>().to_vec(),
        }
    }

    fn word_value(&mut self, ty: &AbiType) -> U256 {
        // Rarely a value outside the type's range, which the precompiles must reject.
        if self.rng.chance(2) {
            return self.rng.edge_u256();
        }
        match ty {
            AbiType::Address => U256::from_be_slice(self.address_value().as_slice()),
            AbiType::Bool => U256::from(self.rng.chance(50)),
            AbiType::Uint(8) => {
                U256::from(*self.rng.pick(&[0u64, 0, 1, 1, 2, 3, 4, 5, 6, 8, 18, 36, 255]))
            }
            AbiType::Uint(bits) if *bits <= 32 => {
                let max = (1u64 << bits) - 1;
                U256::from(*self.rng.pick(&[0, 1, 2, 6, 18, 200, 999, max, max - 1]))
            }
            AbiType::Uint(64) => {
                let ids: Vec<u64> = self.registry.policies.iter().map(|p| p.id).collect();
                match self.rng.below(6) {
                    0 | 1 if !ids.is_empty() => U256::from(ids[self.rng.index(ids.len())]),
                    2 => U256::from(self.rng.below(4)),
                    3 => U256::from(u64::MAX - self.rng.below(2)),
                    _ => U256::from(self.rng.below(100)),
                }
            }
            AbiType::Uint(_) => match self.rng.below(8) {
                0 => B20_MAX_SUPPLY_CAP,
                1 => U256::from(1_700_000_000 + self.rng.below(200_000_000)),
                2 => U256::from(10).pow(U256::from(self.rng.range(0, 36))),
                3 => U256::from(u128::MAX),
                _ => Registry::amount(self.rng),
            },
            AbiType::Int(_) => {
                let small = self.rng.below(5) as i64 - 2;
                if small < 0 { U256::MAX - U256::from(-small - 1) } else { U256::from(small) }
            }
            AbiType::FixedBytes(32) => U256::from_be_bytes(self.bytes32_value().0),
            AbiType::FixedBytes(4) => {
                let ids = [
                    ERC165_INTERFACE_ID,
                    ERC8056_INTERFACE_IDS[0],
                    ERC8056_INTERFACE_IDS[1],
                    ERC8056_INTERFACE_IDS[2],
                    ERC8056_INTERFACE_IDS[3],
                ];
                let id = if self.rng.chance(70) {
                    ids[self.rng.index(ids.len())].0
                } else {
                    (self.rng.next_u64() as u32).to_be_bytes()
                };
                let mut word = [0u8; 32];
                word[..4].copy_from_slice(&id);
                U256::from_be_bytes(word)
            }
            AbiType::FixedBytes(n) => {
                let mut word = [0u8; 32];
                word[..*n].copy_from_slice(&self.rng.b256()[..*n]);
                U256::from_be_bytes(word)
            }
            _ => U256::ZERO,
        }
    }

    fn address_value(&mut self) -> Address {
        let keys = Keys::get();
        match self.rng.below(12) {
            0..=2 => keys.address(self.rng.index(Keys::COUNT)),
            3 => keys.operator(),
            4 if !self.registry.tokens.is_empty() => {
                self.registry.tokens[self.rng.index(self.registry.tokens.len())].address
            }
            5 => Address::ZERO,
            6 if !self.registry.programs.is_empty() => {
                self.registry.programs[self.rng.index(self.registry.programs.len())].address
            }
            7 => *self.rng.pick(&[
                B20FactoryStorage::ADDRESS,
                PolicyRegistryStorage::ADDRESS,
                ActivationRegistryStorage::ADDRESS,
                MAINNET_BERYL_ACTIVATION_ADMIN_ADDRESS,
            ]),
            8 => self.to,
            9 => self.rng.address(),
            _ => {
                let token = self.registry.tokens.first();
                token
                    .and_then(|t| t.holders.iter().nth(self.rng.index(t.holders.len().max(1))))
                    .copied()
                    .unwrap_or_else(|| keys.address(1))
            }
        }
    }

    fn bytes32_value(&mut self) -> B256 {
        const ROLES: [B20TokenRole; 8] = [
            B20TokenRole::DefaultAdmin,
            B20TokenRole::Mint,
            B20TokenRole::Burn,
            B20TokenRole::BurnBlocked,
            B20TokenRole::Seize,
            B20TokenRole::Pause,
            B20TokenRole::Unpause,
            B20TokenRole::Metadata,
        ];
        const SCOPES: [B20PolicyType; 6] = [
            B20PolicyType::TransferSender,
            B20PolicyType::TransferReceiver,
            B20PolicyType::TransferExecutor,
            B20PolicyType::MintReceiver,
            B20PolicyType::SeizeExempt,
            B20PolicyType::SeizeReceiver,
        ];
        const FEATURES: [ActivationFeature; 3] = [
            ActivationFeature::PolicyRegistry,
            ActivationFeature::B20Stablecoin,
            ActivationFeature::B20Asset,
        ];
        match self.rng.below(8) {
            0 | 1 => ROLES[self.rng.index(ROLES.len())].id(),
            2 => Native::ASSET_OPERATOR_ROLE,
            3 => SCOPES[self.rng.index(SCOPES.len())].id(),
            4 => FEATURES[self.rng.index(FEATURES.len())].id(),
            5 => B256::ZERO,
            _ => self.rng.b256(),
        }
    }

    fn string_value(&mut self) -> String {
        match self.rng.below(8) {
            0 => String::new(),
            1 => "USD".to_owned(),
            2 => (*self.rng.pick(&["EUR", "usd", "XXX", "JPY"])).to_owned(),
            3 => "x".repeat(self.rng.range(33, 300) as usize),
            _ => format!("Fuzz {}", self.rng.below(1000)),
        }
    }

    fn bytes_value(&mut self) -> Vec<u8> {
        let operator = Keys::get().operator();
        match self.rng.below(8) {
            0 => Vec::new(),
            1 => self.rng.bytes_between(1, 100),
            // Inner token calls, as factory init calls and asset announcements carry.
            2 | 3 => {
                IB20::mintCall { to: self.address_value(), amount: Registry::amount(self.rng) }
                    .abi_encode()
            }
            4 => IB20::grantRoleCall { role: self.bytes32_value(), account: operator }.abi_encode(),
            // Token creation parameters.
            5 => IB20Factory::B20AssetCreateParams {
                version: if self.rng.chance(80) { 1 } else { self.rng.below(4) as u8 },
                name: self.string_value(),
                symbol: "FA".to_owned(),
                initialAdmin: operator,
                decimals: *self.rng.pick(&[0u8, 6, 18, 255]),
            }
            .abi_encode(),
            6 => IB20Factory::B20StablecoinCreateParams {
                version: if self.rng.chance(80) { 1 } else { self.rng.below(4) as u8 },
                name: self.string_value(),
                symbol: "FUSD".to_owned(),
                initialAdmin: operator,
                currency: self.string_value(),
            }
            .abi_encode(),
            _ => self.rng.bytes_between(1, 8),
        }
    }
}
