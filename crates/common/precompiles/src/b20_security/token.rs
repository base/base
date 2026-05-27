//! `B20SecurityToken` struct — the security B-20 token type.

use alloy_primitives::{Address, B256, b256};

use crate::{
    B20SecurityStorage, Burnable, Configurable, Mintable, Pausable, Permittable, Policy,
    RoleManaged, SecurityAccounting, Token, Transferable,
};

/// EVM precompile for the security B-20 variant.
///
/// Mirrors the structure of [`crate::B20Token`] but requires `S: SecurityAccounting`
/// so the dispatch layer can read and write security-specific storage (share ratio,
/// security identifiers, announcement IDs). The `in_announcement` flag guards against
/// recursive `announce` calls within a single precompile invocation.
#[derive(Debug, Clone)]
pub struct B20SecurityToken<S: SecurityAccounting, P: Policy> {
    accounting: S,
    policy: P,
    in_announcement: bool,
}

impl<S: SecurityAccounting, P: Policy> B20SecurityToken<S, P> {
    /// Role identifier for security operators: `keccak256("SECURITY_OPERATOR_ROLE")`.
    pub const SECURITY_OPERATOR_ROLE: B256 =
        b256!("e63901dfe7775ace99fa3654743976eb0ab2009f5d19c4fc1ecd40aed27d59af");

    /// Role identifier for delegated burns: `keccak256("BURN_FROM_ROLE")`.
    pub const BURN_FROM_ROLE: B256 =
        b256!("25400dba76bf0d00acf274c2b61ff56aa4ed19826e21e0186e3fecd6a6671875");

    /// Policy scope identifier for redeem senders: `keccak256("REDEEM_SENDER_POLICY")`.
    pub const REDEEM_SENDER_POLICY: B256 = B20SecurityStorage::REDEEM_SENDER_POLICY;

    /// Creates a `B20SecurityToken` backed by the provided storage and policy adapters.
    pub const fn with_storage_and_policy(accounting: S, policy: P) -> Self {
        Self { accounting, policy, in_announcement: false }
    }

    /// Returns whether this token is currently executing an announcement.
    pub const fn is_announcement_active(&self) -> bool {
        self.in_announcement
    }

    /// Marks this token as executing an announcement.
    pub const fn begin_announcement(&mut self) {
        self.in_announcement = true;
    }
}

impl<S: SecurityAccounting, P: Policy> Token for B20SecurityToken<S, P> {
    type Accounting = S;
    type Policy = P;

    fn accounting(&self) -> &S {
        &self.accounting
    }

    fn accounting_mut(&mut self) -> &mut S {
        &mut self.accounting
    }

    fn policy(&self) -> &P {
        &self.policy
    }

    fn policy_mut(&mut self) -> &mut P {
        &mut self.policy
    }

    fn token_address(&self) -> Address {
        self.accounting.token_address()
    }
}

impl<S: SecurityAccounting, P: Policy> Transferable for B20SecurityToken<S, P> {}
impl<S: SecurityAccounting, P: Policy> Mintable for B20SecurityToken<S, P> {}
impl<S: SecurityAccounting, P: Policy> Burnable for B20SecurityToken<S, P> {}
impl<S: SecurityAccounting, P: Policy> Pausable for B20SecurityToken<S, P> {}
impl<S: SecurityAccounting, P: Policy> Configurable for B20SecurityToken<S, P> {}
impl<S: SecurityAccounting, P: Policy> Permittable for B20SecurityToken<S, P> {}
impl<S: SecurityAccounting, P: Policy> RoleManaged for B20SecurityToken<S, P> {}

#[cfg(test)]
mod tests {
    use alloy_primitives::keccak256;

    use crate::{B20SecurityToken, InMemoryPolicy, InMemoryTokenAccounting};

    type TestSecurityToken = B20SecurityToken<InMemoryTokenAccounting, InMemoryPolicy>;

    #[test]
    fn role_and_policy_ids_match_solidity_hashes() {
        assert_eq!(TestSecurityToken::SECURITY_OPERATOR_ROLE, keccak256("SECURITY_OPERATOR_ROLE"));
        assert_eq!(TestSecurityToken::BURN_FROM_ROLE, keccak256("BURN_FROM_ROLE"));
        assert_eq!(TestSecurityToken::REDEEM_SENDER_POLICY, keccak256("REDEEM_SENDER_POLICY"));
    }
}
