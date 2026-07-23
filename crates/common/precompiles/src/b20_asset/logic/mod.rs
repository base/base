//! Versioned business logic for the asset B-20 precompile.
//!
//! [`Asset`] (in [`interface`](self)) is the append-only business-logic
//! interface each version implements; [`B20AssetToken`] is the minimal
//! storage + policy holder the logic operates on; and [`AssetV1`] is the
//! first frozen implementation.

use alloy_primitives::Address;

use crate::{AssetAccounting, PolicyAccounting, PolicyRegistryLogic, PolicyVersion, Token};

mod interface;
pub use interface::Asset;

mod v1;
pub use v1::AssetV1;

/// Emits a fully-delegating [`Asset`] impl that forwards each listed method to a prior version.
///
/// Every precompile version is a distinct, frozen [`Asset`] implementation. A version that only
/// *adds* behavior (e.g. [`AssetV2`] at Cobalt) would otherwise restate the entire inherited
/// surface verbatim just to forward it to the version it extends. This macro generates that
/// forwarding from a single method-name list, so a version's module contains only the methods
/// whose behavior actually diverges.
///
/// `delegate_asset!(NewVersion => prior, { method_a, method_b, ... })` forwards each named method
/// to `prior` (a unit-struct value such as [`AssetV1`]). A method the new version overrides is
/// simply omitted from the list and written by hand in a separate `impl` block.
macro_rules! delegate_asset {
    ($target:ty => $to:expr, { $($method:ident),+ $(,)? }) => {
        impl<S: $crate::AssetAccounting, A: $crate::PolicyAccounting> $crate::Asset<S, A>
            for $target
        {
            $(delegate_asset!(@fwd $to, $method);)+
        }
    };

    (@fwd $to:expr, transfer) => {
        fn transfer(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            to: ::alloy_primitives::Address,
            amount: ::alloy_primitives::U256,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.transfer(token, caller, to, amount, privileged)
        }
    };
    (@fwd $to:expr, transfer_from) => {
        fn transfer_from(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            from: ::alloy_primitives::Address,
            to: ::alloy_primitives::Address,
            amount: ::alloy_primitives::U256,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.transfer_from(token, caller, from, to, amount, privileged)
        }
    };
    (@fwd $to:expr, approve) => {
        fn approve(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            spender: ::alloy_primitives::Address,
            amount: ::alloy_primitives::U256,
        ) -> ::base_precompile_storage::Result<()> {
            $to.approve(token, caller, spender, amount)
        }
    };
    (@fwd $to:expr, emit_memo) => {
        fn emit_memo(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            memo: ::alloy_primitives::B256,
        ) -> ::base_precompile_storage::Result<()> {
            $to.emit_memo(token, caller, memo)
        }
    };
    (@fwd $to:expr, mint) => {
        fn mint(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            to: ::alloy_primitives::Address,
            amount: ::alloy_primitives::U256,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.mint(token, caller, to, amount, privileged)
        }
    };
    (@fwd $to:expr, burn) => {
        fn burn(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            amount: ::alloy_primitives::U256,
        ) -> ::base_precompile_storage::Result<()> {
            $to.burn(token, caller, amount)
        }
    };
    (@fwd $to:expr, burn_blocked) => {
        fn burn_blocked(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            from: ::alloy_primitives::Address,
            amount: ::alloy_primitives::U256,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.burn_blocked(token, caller, from, amount, privileged)
        }
    };
    (@fwd $to:expr, pause) => {
        fn pause(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            features: ::alloc::vec::Vec<$crate::IB20::PausableFeature>,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.pause(token, caller, features, privileged)
        }
    };
    (@fwd $to:expr, unpause) => {
        fn unpause(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            features: ::alloc::vec::Vec<$crate::IB20::PausableFeature>,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.unpause(token, caller, features, privileged)
        }
    };
    (@fwd $to:expr, update_supply_cap) => {
        fn update_supply_cap(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            new_cap: ::alloy_primitives::U256,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.update_supply_cap(token, caller, new_cap, privileged)
        }
    };
    (@fwd $to:expr, update_name) => {
        fn update_name(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            name: ::alloc::string::String,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.update_name(token, caller, name, privileged)
        }
    };
    (@fwd $to:expr, update_symbol) => {
        fn update_symbol(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            symbol: ::alloc::string::String,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.update_symbol(token, caller, symbol, privileged)
        }
    };
    (@fwd $to:expr, update_contract_uri) => {
        fn update_contract_uri(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            uri: ::alloc::string::String,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.update_contract_uri(token, caller, uri, privileged)
        }
    };
    (@fwd $to:expr, grant_role) => {
        fn grant_role(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            role: ::alloy_primitives::B256,
            account: ::alloy_primitives::Address,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.grant_role(token, caller, role, account, privileged)
        }
    };
    (@fwd $to:expr, revoke_role) => {
        fn revoke_role(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            role: ::alloy_primitives::B256,
            account: ::alloy_primitives::Address,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.revoke_role(token, caller, role, account, privileged)
        }
    };
    (@fwd $to:expr, renounce_role) => {
        fn renounce_role(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            role: ::alloy_primitives::B256,
            confirmation: ::alloy_primitives::Address,
        ) -> ::base_precompile_storage::Result<()> {
            $to.renounce_role(token, caller, role, confirmation)
        }
    };
    (@fwd $to:expr, renounce_last_admin) => {
        fn renounce_last_admin(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
        ) -> ::base_precompile_storage::Result<()> {
            $to.renounce_last_admin(token, caller)
        }
    };
    (@fwd $to:expr, set_role_admin) => {
        fn set_role_admin(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            role: ::alloy_primitives::B256,
            new_admin_role: ::alloy_primitives::B256,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.set_role_admin(token, caller, role, new_admin_role, privileged)
        }
    };
    (@fwd $to:expr, update_policy) => {
        fn update_policy(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            policy_scope: ::alloy_primitives::B256,
            new_policy_id: u64,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.update_policy(token, caller, policy_scope, new_policy_id, privileged)
        }
    };
    (@fwd $to:expr, permit) => {
        fn permit(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            chain_id: u64,
            now: ::alloy_primitives::U256,
            args: $crate::PermitArgs,
        ) -> ::base_precompile_storage::Result<()> {
            $to.permit(token, chain_id, now, args)
        }
    };
    (@fwd $to:expr, update_multiplier) => {
        fn update_multiplier(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            new_multiplier: ::alloy_primitives::U256,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.update_multiplier(token, caller, new_multiplier, privileged)
        }
    };
    (@fwd $to:expr, update_extra_metadata) => {
        fn update_extra_metadata(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            key: ::alloc::string::String,
            value: ::alloc::string::String,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.update_extra_metadata(token, caller, key, value, privileged)
        }
    };
    (@fwd $to:expr, batch_mint) => {
        fn batch_mint(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            recipients: ::alloc::vec::Vec<::alloy_primitives::Address>,
            amounts: ::alloc::vec::Vec<::alloy_primitives::U256>,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.batch_mint(token, caller, recipients, amounts, privileged)
        }
    };
    (@fwd $to:expr, begin_announce) => {
        fn begin_announce(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            caller: ::alloy_primitives::Address,
            id: ::alloc::string::String,
            description: ::alloc::string::String,
            uri: ::alloc::string::String,
            privileged: bool,
        ) -> ::base_precompile_storage::Result<()> {
            $to.begin_announce(token, caller, id, description, uri, privileged)
        }
    };
    (@fwd $to:expr, end_announce) => {
        fn end_announce(
            &self,
            token: &mut $crate::B20AssetToken<S, A>,
            id: ::alloc::string::String,
        ) -> ::base_precompile_storage::Result<()> {
            $to.end_announce(token, id)
        }
    };
    (@fwd $to:expr, is_paused) => {
        fn is_paused(
            &self,
            token: &$crate::B20AssetToken<S, A>,
            feature: $crate::IB20::PausableFeature,
        ) -> ::base_precompile_storage::Result<bool> {
            $to.is_paused(token, feature)
        }
    };
    (@fwd $to:expr, paused_features) => {
        fn paused_features(
            &self,
            token: &$crate::B20AssetToken<S, A>,
        ) -> ::base_precompile_storage::Result<::alloc::vec::Vec<$crate::IB20::PausableFeature>> {
            $to.paused_features(token)
        }
    };
    (@fwd $to:expr, policy_id) => {
        fn policy_id(
            &self,
            token: &$crate::B20AssetToken<S, A>,
            policy_scope: ::alloy_primitives::B256,
        ) -> ::base_precompile_storage::Result<u64> {
            $to.policy_id(token, policy_scope)
        }
    };
    (@fwd $to:expr, domain_separator) => {
        fn domain_separator(
            &self,
            token: &$crate::B20AssetToken<S, A>,
            chain_id: u64,
        ) -> ::base_precompile_storage::Result<::alloy_primitives::B256> {
            $to.domain_separator(token, chain_id)
        }
    };
    (@fwd $to:expr, eip712_domain) => {
        fn eip712_domain(
            &self,
            token: &$crate::B20AssetToken<S, A>,
            chain_id: u64,
        ) -> ::base_precompile_storage::Result<$crate::Eip712Domain> {
            $to.eip712_domain(token, chain_id)
        }
    };
    (@fwd $to:expr, to_scaled_balance) => {
        fn to_scaled_balance(
            &self,
            token: &$crate::B20AssetToken<S, A>,
            balance: ::alloy_primitives::U256,
        ) -> ::base_precompile_storage::Result<::alloy_primitives::U256> {
            $to.to_scaled_balance(token, balance)
        }
    };
    (@fwd $to:expr, to_raw_balance) => {
        fn to_raw_balance(
            &self,
            token: &$crate::B20AssetToken<S, A>,
            balance: ::alloy_primitives::U256,
        ) -> ::base_precompile_storage::Result<::alloy_primitives::U256> {
            $to.to_raw_balance(token, balance)
        }
    };
    (@fwd $to:expr, scaled_balance_of) => {
        fn scaled_balance_of(
            &self,
            token: &$crate::B20AssetToken<S, A>,
            account: ::alloy_primitives::Address,
        ) -> ::base_precompile_storage::Result<::alloy_primitives::U256> {
            $to.scaled_balance_of(token, account)
        }
    };
    (@fwd $to:expr, operator_role) => {
        fn operator_role(&self) -> ::alloy_primitives::B256 {
            // Unlike the token-taking methods, this signature never mentions `S`/`A`, so the
            // delegate's generic params cannot be inferred from an argument. Name the trait
            // instantiation explicitly to disambiguate.
            $crate::Asset::<S, A>::operator_role(&$to)
        }
    };
}

mod v2;
pub use v2::AssetV2;

/// Storage + policy binding the asset logic operates on.
///
/// A minimal `(accounting, policy, policy_version)` holder implementing [`Token`];
/// it carries no behavior of its own — all business logic lives in the version
/// implementations resolved from [`crate::AssetVersions`]. Authorization goes
/// through [`crate::PolicyRegistryLogic`] via [`Token::policy`].
#[derive(Debug, Clone)]
pub struct B20AssetToken<S: AssetAccounting, A: PolicyAccounting> {
    accounting: S,
    policy: A,
    policy_version: PolicyVersion,
}

impl<S: AssetAccounting, A: PolicyAccounting> B20AssetToken<S, A> {
    /// Creates a holder backed by token storage, policy-registry storage, and version.
    pub const fn with_storage_and_policy(
        accounting: S,
        policy: A,
        policy_version: PolicyVersion,
    ) -> Self {
        Self { accounting, policy, policy_version }
    }
}

impl<S: AssetAccounting, A: PolicyAccounting> Token for B20AssetToken<S, A> {
    type Accounting = S;
    type PolicyAccounting = A;

    fn accounting(&self) -> &S {
        &self.accounting
    }

    fn accounting_mut(&mut self) -> &mut S {
        &mut self.accounting
    }

    fn policy(&self) -> &dyn PolicyRegistryLogic<A> {
        self.policy_version.implementation()
    }

    fn policy_storage(&self) -> &A {
        &self.policy
    }

    fn policy_storage_mut(&mut self) -> &mut A {
        &mut self.policy
    }

    fn token_address(&self) -> Address {
        self.accounting.token_address()
    }
}
