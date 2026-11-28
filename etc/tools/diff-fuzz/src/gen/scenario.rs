//! Multi-block scenarios: early blocks set state up, later blocks exercise it.
//!
//! Scenarios:
//! - program suite: deploy generated programs, then call them (random traffic does the calls);
//! - native activation: on chains that cross Beryl, the chain-config admin activates the
//!   policy, stablecoin, and asset features by deposit; on chains that cross Cobalt, it hands
//!   the registry admin to the operator key;
//! - token lifecycle: create assets and stablecoins through the factory, grant every role,
//!   create allowlist and blocklist policies, then mint, transfer, approve, pause, burn, and
//!   seize from right and wrong callers (random native traffic, which knows roles and holders);
//! - delegations: install EIP-7702 delegations, call through them, replace and clear them;
//! - storage: fill slot ranges of the filler contract, then clear them (refunds and trie
//!   collapses), then refill;
//! - deposits: queue L1-to-L2 deposits that fail and succeed at every epoch start;
//! - withdrawals: initiate L2-to-L1 withdrawals and fee vault withdrawals.

use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::SolCall;
use base_common_chains::MAINNET_BERYL_ACTIVATION_ADMIN_ADDRESS;
use base_common_genesis::BaseFeature;
use base_common_precompiles::{
    ActivationFeature, ActivationRegistryStorage, B20TokenRole, B20Variant, IActivationRegistry,
    IB20, IPolicyRegistry, PolicyRegistryStorage,
};

use crate::r#gen::{
    era::Era, genesis::FILLER, keys::Keys, native::Native, plan::Planner, rng::Rng,
};

/// The scenario state of one chain.
#[derive(Debug, Clone)]
pub struct Scenarios {
    /// Native features still to activate, as a bit set over [`Self::FEATURES`] (the chain
    /// started before Beryl). Each is activated in a random later block, so native calls also
    /// run with none or only some of the features active.
    pub needs_activation: u8,
    /// The registry admin still needs to move to the operator (started before Cobalt).
    pub needs_admin: bool,
    /// Blocks planned so far.
    pub blocks: u64,
    /// Storage scenario phase.
    pub storage_phase: u64,
    /// Enabled scenarios, as a bit set over [`Self::ALL`].
    pub enabled: u64,
}

impl Scenarios {
    /// Scenario names, in bit order.
    pub const ALL: [&'static str; 6] =
        ["programs", "tokens", "delegations", "storage", "deposits", "withdrawals"];

    /// The features the activation scenario activates, in bit order.
    pub const FEATURES: [ActivationFeature; 3] = [
        ActivationFeature::PolicyRegistry,
        ActivationFeature::B20Stablecoin,
        ActivationFeature::B20Asset,
    ];

    /// Every bit of [`Self::FEATURES`].
    pub const ALL_FEATURES: u8 = (1 << Self::FEATURES.len()) - 1;

    /// Scenario state for a chain whose genesis state is from `era`.
    pub fn new(rng: &mut Rng, era: Era) -> Self {
        let mut enabled = 0;
        for bit in 0..Self::ALL.len() {
            if rng.chance(70) {
                enabled |= 1 << bit;
            }
        }
        Self {
            needs_activation: if Native::active(era.upgrade()) { 0 } else { Self::ALL_FEATURES },
            needs_admin: !era.upgrade().is_feature_enabled(BaseFeature::StatefulActivationAdmin),
            blocks: 0,
            storage_phase: 0,
            enabled,
        }
    }

    fn on(&self, name: &str) -> bool {
        Self::ALL.iter().position(|n| *n == name).is_some_and(|bit| self.enabled & (1 << bit) != 0)
    }

    /// Activates the native features in `mask` (bits over [`Self::FEATURES`]) as the
    /// chain-config admin, by deposit.
    pub fn activate_features(planner: &mut Planner<'_>, mask: u8) {
        for (bit, feature) in Self::FEATURES.into_iter().enumerate() {
            if mask & (1 << bit) == 0 {
                continue;
            }
            let input = IActivationRegistry::activateCall { feature: feature.id() }.abi_encode();
            planner.deposit_call(
                MAINNET_BERYL_ACTIVATION_ADMIN_ADDRESS,
                ActivationRegistryStorage::ADDRESS,
                U256::ZERO,
                input.into(),
                "native:activation.activate",
            );
        }
    }

    /// Adds this block's scenario transactions: deposits when `deposits`, otherwise sequencer
    /// transactions.
    pub fn step(&mut self, planner: &mut Planner<'_>, deposits: bool) {
        if deposits {
            if self.on("deposits") {
                for _ in 0..planner.rng.range(1, 3) {
                    planner.deposit();
                }
            }
            return;
        }
        self.blocks += 1;
        let upgrade = planner.ctx.upgrade;
        if Native::active(upgrade) && self.needs_activation != 0 {
            let mask: u8 = (0..Self::FEATURES.len())
                .filter(|_| planner.rng.chance(40))
                .fold(0, |mask, bit| mask | (1 << bit))
                & self.needs_activation;
            Self::activate_features(planner, mask);
            self.needs_activation &= !mask;
        }
        if upgrade.is_feature_enabled(BaseFeature::StatefulActivationAdmin) && self.needs_admin {
            let input =
                IActivationRegistry::setAdminCall { newAdmin: Keys::get().operator() }.abi_encode();
            planner.deposit_call(
                MAINNET_BERYL_ACTIVATION_ADMIN_ADDRESS,
                ActivationRegistryStorage::ADDRESS,
                U256::ZERO,
                input.into(),
                "native:activation.setAdmin",
            );
            self.needs_admin = false;
        }
        if self.on("programs") && (self.blocks <= 2 || planner.rng.chance(10)) {
            for _ in 0..planner.rng.range(1, 3) {
                planner.deploy_program();
            }
        }
        if self.on("tokens") && Native::active(upgrade) {
            self.tokens(planner);
        }
        if self.on("delegations") && upgrade.is_feature_enabled(BaseFeature::Eip7702) {
            for _ in 0..planner.rng.range(0, 2) {
                planner.delegation();
            }
        }
        if self.on("storage") {
            self.storage(planner);
        }
        if self.on("withdrawals") && planner.rng.chance(40) {
            planner.system_call();
        }
    }

    fn tokens(&self, planner: &mut Planner<'_>) {
        let operator = Keys::get().operator();
        let registry = planner.registry;
        if registry.tokens.len() < 4 + (self.blocks / 20) as usize && planner.rng.chance(60) {
            let variant =
                if planner.rng.chance(50) { B20Variant::Asset } else { B20Variant::Stablecoin };
            let holders: Vec<Address> =
                (0..3).map(|_| Keys::get().address(planner.rng.range(1, 10) as usize)).collect();
            let salt = planner.rng.b256();
            planner.token_bootstrap(salt, variant, &holders);
        }
        if registry.policies.len() < 2 && planner.rng.chance(60) {
            let kind = if registry.policies.is_empty() {
                IPolicyRegistry::PolicyType::ALLOWLIST
            } else {
                IPolicyRegistry::PolicyType::BLOCKLIST
            };
            let accounts = vec![Keys::get().address(1), Keys::get().address(2), operator];
            let input = IPolicyRegistry::createPolicyWithAccountsCall {
                admin: operator,
                policyType: kind,
                accounts,
            }
            .abi_encode();
            planner.call(
                Some(operator),
                PolicyRegistryStorage::ADDRESS,
                U256::ZERO,
                input.into(),
                "native:policy.createPolicyWithAccounts".into(),
            );
        }
        // Grant the operator every role it lacks on one token.
        if let Some(token) = registry.tokens.get(planner.rng.index(registry.tokens.len().max(1))) {
            let admins = token.holders_of(B20TokenRole::DefaultAdmin.id());
            for role in [
                B20TokenRole::Pause,
                B20TokenRole::Unpause,
                B20TokenRole::Burn,
                B20TokenRole::BurnBlocked,
                B20TokenRole::Seize,
                B20TokenRole::Metadata,
                B20TokenRole::Mint,
            ] {
                if token.holders_of(role.id()).is_empty()
                    && admins.contains(&operator)
                    && planner.rng.chance(50)
                {
                    let account = if planner.rng.chance(70) {
                        operator
                    } else {
                        Keys::get().address(planner.rng.range(1, 5) as usize)
                    };
                    let input = IB20::grantRoleCall { role: role.id(), account }.abi_encode();
                    planner.call(
                        Some(operator),
                        token.address,
                        U256::ZERO,
                        input.into(),
                        "native:b20.grantRole".into(),
                    );
                }
            }
            if !registry.policies.is_empty() && planner.rng.chance(20) && admins.contains(&operator)
            {
                let policy = &registry.policies[planner.rng.index(registry.policies.len())];
                let scope = base_common_precompiles::B20PolicyType::TransferReceiver.id();
                let input = IB20::updatePolicyCall { policyScope: scope, newPolicyId: policy.id }
                    .abi_encode();
                planner.call(
                    Some(operator),
                    token.address,
                    U256::ZERO,
                    input.into(),
                    "native:b20.updatePolicy".into(),
                );
            }
        }
        for _ in 0..planner.rng.range(1, 4) {
            planner.native_call();
        }
    }

    fn storage(&mut self, planner: &mut Planner<'_>) {
        if !planner.rng.chance(50) {
            return;
        }
        self.storage_phase += 1;
        let start = U256::from(*planner.rng.pick(&[0u64, 0, 300, 1 << 20]));
        let (count, value) = match self.storage_phase % 4 {
            0 | 1 => (planner.rng.range(20, 400), U256::from(planner.rng.range(1, 1 << 40))),
            2 => (planner.rng.range(1, 400), U256::ZERO),
            _ => (planner.rng.range(1, 50), U256::from(1)),
        };
        let mut input = start.to_be_bytes::<32>().to_vec();
        input.extend(U256::from(count).to_be_bytes::<32>());
        input.extend(value.to_be_bytes::<32>());
        let gas = 60_000 + count * 23_000;
        planner.call_gas(
            None,
            FILLER,
            U256::ZERO,
            Bytes::from(input),
            "storage:fill".into(),
            Some(gas),
        );
    }
}
