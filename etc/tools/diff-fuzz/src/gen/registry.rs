//! What the generator knows about a chain: deployed programs, tokens, and policies.
//!
//! Everything here is derived from state and from the agreed responses (post-state code hashes
//! and receipt logs), never from assumptions about which transactions succeeded.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, B256, Bytes, Log as PrimitiveLog, LogData, U256};
use alloy_sol_types::SolEvent;
use base_common_precompiles::{B20Variant, IB20, IB20Factory, IPolicyRegistry};

use crate::{
    Log,
    r#gen::{program::Fragment, state::WorldState},
};

/// A generated program deployed on chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployedProgram {
    /// Contract address.
    pub address: Address,
    /// Fragment kinds in mask-bit order.
    pub fragments: Vec<Fragment>,
}

/// A deployment the generator submitted and has not yet seen confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDeploy {
    /// Predicted address.
    pub address: Address,
    /// Expected code hash.
    pub code_hash: B256,
    /// Program fragments, when the runtime is a generated program.
    pub fragments: Vec<Fragment>,
}

/// A B-20 token created through the factory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// Token address.
    pub address: Address,
    /// Variant.
    pub variant: B20Variant,
    /// Current name, which the EIP-712 permit domain commits to.
    pub name: String,
    /// Accounts holding each role, from `RoleGranted` and `RoleRevoked` logs.
    pub roles: BTreeMap<B256, BTreeSet<Address>>,
    /// Accounts that ever received tokens.
    pub holders: BTreeSet<Address>,
    /// Accounts that were approved as spenders, by owner.
    pub approvals: BTreeSet<(Address, Address)>,
}

impl Token {
    /// Accounts holding `role`.
    pub fn holders_of(&self, role: B256) -> Vec<Address> {
        self.roles.get(&role).map(|set| set.iter().copied().collect()).unwrap_or_default()
    }
}

/// A policy in the policy registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// Policy id.
    pub id: u64,
    /// Current admin.
    pub admin: Address,
    /// Pending admin, if staged.
    pub pending_admin: Option<Address>,
    /// Policy type discriminant (0 blocklist, 1 allowlist, 2 union, 3 intersect).
    pub kind: u8,
}

/// Knowledge about a chain.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Registry {
    /// Generated programs.
    pub programs: Vec<DeployedProgram>,
    /// Template contracts (loggers, reverters, destructors) found on chain.
    pub templates: BTreeSet<Address>,
    /// Deployments awaiting confirmation.
    pub pending: Vec<PendingDeploy>,
    /// Tokens.
    pub tokens: Vec<Token>,
    /// Policies.
    pub policies: Vec<Policy>,
    /// Code hashes of template runtimes.
    pub template_hashes: BTreeSet<B256>,
}

impl Registry {
    /// Confirms pending deployments against the state and discovers template contracts created
    /// by programs among the accounts in `changed`.
    pub fn sync_state(&mut self, state: &WorldState, changed: &[Address]) {
        let pending = std::mem::take(&mut self.pending);
        for deploy in pending {
            match state.account(&deploy.address) {
                Some(account) if account.code_hash == deploy.code_hash => {
                    if deploy.fragments.is_empty() {
                        self.templates.insert(deploy.address);
                    } else if !self.programs.iter().any(|p| p.address == deploy.address) {
                        self.programs.push(DeployedProgram {
                            address: deploy.address,
                            fragments: deploy.fragments,
                        });
                    }
                }
                _ => {}
            }
        }
        for address in changed {
            if let Some(account) = state.account(address)
                && self.template_hashes.contains(&account.code_hash)
            {
                self.templates.insert(*address);
            }
        }
        self.programs.retain(|program| {
            state
                .account(&program.address)
                .is_some_and(|a| a.code_hash != alloy_primitives::KECCAK256_EMPTY)
        });
        self.templates.retain(|address| {
            state.account(address).is_some_and(|a| a.code_hash != alloy_primitives::KECCAK256_EMPTY)
        });
    }

    /// Learns tokens, roles, holders, and policies from the logs of successful receipts.
    pub fn learn_logs<'a>(&mut self, logs: impl IntoIterator<Item = &'a Log>) {
        for log in logs {
            let data = LogData::new_unchecked(log.topics.clone(), log.data.clone());
            let primitive = PrimitiveLog { address: log.address, data };
            let Some(topic0) = log.topics.first() else { continue };
            if *topic0 == IB20Factory::B20Created::SIGNATURE_HASH {
                if let Ok(event) = IB20Factory::B20Created::decode_log(&primitive)
                    && let Some(variant) = B20Variant::from_address(event.token)
                    && !self.tokens.iter().any(|t| t.address == event.token)
                {
                    self.tokens.push(Token {
                        address: event.token,
                        variant,
                        name: event.name.clone(),
                        roles: BTreeMap::new(),
                        holders: BTreeSet::new(),
                        approvals: BTreeSet::new(),
                    });
                }
            } else if *topic0 == IB20::NameUpdated::SIGNATURE_HASH {
                if let Ok(event) = IB20::NameUpdated::decode_log(&primitive)
                    && let Some(token) = self.token_mut(&log.address)
                {
                    token.name = event.newName.clone();
                }
            } else if *topic0 == IB20::RoleGranted::SIGNATURE_HASH {
                if let Ok(event) = IB20::RoleGranted::decode_log(&primitive)
                    && let Some(token) = self.token_mut(&log.address)
                {
                    token.roles.entry(event.role).or_default().insert(event.account);
                }
            } else if *topic0 == IB20::RoleRevoked::SIGNATURE_HASH {
                if let Ok(event) = IB20::RoleRevoked::decode_log(&primitive)
                    && let Some(token) = self.token_mut(&log.address)
                {
                    token.roles.entry(event.role).or_default().remove(&event.account);
                }
            } else if *topic0 == IB20::Transfer::SIGNATURE_HASH {
                if let Ok(event) = IB20::Transfer::decode_log(&primitive)
                    && let Some(token) = self.token_mut(&log.address)
                    && event.to != Address::ZERO
                {
                    token.holders.insert(event.to);
                }
            } else if *topic0 == IB20::Approval::SIGNATURE_HASH {
                if let Ok(event) = IB20::Approval::decode_log(&primitive)
                    && let Some(token) = self.token_mut(&log.address)
                {
                    if event.amount.is_zero() {
                        token.approvals.remove(&(event.owner, event.spender));
                    } else {
                        token.approvals.insert((event.owner, event.spender));
                    }
                }
            } else if *topic0 == IPolicyRegistry::PolicyCreated::SIGNATURE_HASH {
                if let Ok(event) = IPolicyRegistry::PolicyCreated::decode_log(&primitive)
                    && !self.policies.iter().any(|p| p.id == event.policyId)
                {
                    self.policies.push(Policy {
                        id: event.policyId,
                        admin: event.creator,
                        pending_admin: None,
                        kind: event.policyType as u8,
                    });
                }
            } else if *topic0 == IPolicyRegistry::PolicyAdminUpdated::SIGNATURE_HASH {
                if let Ok(event) = IPolicyRegistry::PolicyAdminUpdated::decode_log(&primitive)
                    && let Some(policy) = self.policies.iter_mut().find(|p| p.id == event.policyId)
                {
                    policy.admin = event.newAdmin;
                    policy.pending_admin = None;
                }
            } else if *topic0 == IPolicyRegistry::PolicyAdminStaged::SIGNATURE_HASH
                && let Ok(event) = IPolicyRegistry::PolicyAdminStaged::decode_log(&primitive)
                && let Some(policy) = self.policies.iter_mut().find(|p| p.id == event.policyId)
            {
                policy.pending_admin = Some(event.pendingAdmin);
            }
        }
    }

    fn token_mut(&mut self, address: &Address) -> Option<&mut Token> {
        self.tokens.iter_mut().find(|t| t.address == *address)
    }

    /// EIP-7702 delegations in the state: `(authority, delegate)`.
    pub fn delegations(state: &WorldState) -> Vec<(Address, Address)> {
        state
            .accounts
            .iter()
            .filter_map(|(address, account)| {
                let code: &Bytes = state.codes.get(&account.code_hash)?;
                (code.len() == 23 && code[..3] == [0xef, 0x01, 0x00])
                    .then(|| (*address, Address::from_slice(&code[3..])))
            })
            .collect()
    }

    /// The token balance proxy used for amounts: a plausible amount to move.
    pub fn amount(rng: &mut crate::r#gen::rng::Rng) -> U256 {
        match rng.below(6) {
            0 => U256::ZERO,
            1 => U256::from(1),
            2 => U256::MAX,
            3 => U256::from(rng.range(1, 1_000_000)),
            _ => U256::from(rng.range(1, 1_000)) * U256::from(10).pow(U256::from(18)),
        }
    }
}
