//! Wire (ABI) surfaces for the shared B-20 token interface, one per hardfork that moved them.
//!
//! The latest surface is always named `IB20` in its `vN` module, then re-exported here as both
//! [`IB20`] (canonical) and `IB20VN`. Older forks keep the same Rust name inside their module so
//! truncated-calldata revert bytes stay stable, and are re-exported as [`IB20V1`], [`IB20V2`], etc.
//!
//! Token variants compose this surface with their own extension ABI. Asset does so via
//! [`crate::AssetAbiPair`]; stablecoin still decodes against canonical [`IB20`] directly until it
//! adopts the same composite shape.
//!
//! A fork that changes the common wire adds `abi/vN.rs` and retargets the canonical alias below.
//! Token versions then map onto the new [`B20Abi`] variant through their own `abi()` join — there
//! is no independent `B20Abi::from_base_upgrade`.

mod v1;
pub use v1::IB20 as IB20V1;

mod v2;
pub use v2::{IB20, IB20 as IB20V2};

mod b20_abi;
pub use b20_abi::B20Abi;

/// Lifts a Beryl-frozen common B-20 call into the canonical (Cobalt) enum without re-parsing.
///
/// V1 selectors are a subset of V2. Shared parameter layouts move; `PausableFeature` values are
/// remapped by name (V1 never carried `SEIZE`).
impl From<IB20V1::IB20Calls> for IB20::IB20Calls {
    fn from(call: IB20V1::IB20Calls) -> Self {
        match call {
            IB20V1::IB20Calls::DEFAULT_ADMIN_ROLE(_) => {
                Self::DEFAULT_ADMIN_ROLE(IB20::DEFAULT_ADMIN_ROLECall {})
            }
            IB20V1::IB20Calls::MINT_ROLE(_) => Self::MINT_ROLE(IB20::MINT_ROLECall {}),
            IB20V1::IB20Calls::BURN_ROLE(_) => Self::BURN_ROLE(IB20::BURN_ROLECall {}),
            IB20V1::IB20Calls::BURN_BLOCKED_ROLE(_) => {
                Self::BURN_BLOCKED_ROLE(IB20::BURN_BLOCKED_ROLECall {})
            }
            IB20V1::IB20Calls::PAUSE_ROLE(_) => Self::PAUSE_ROLE(IB20::PAUSE_ROLECall {}),
            IB20V1::IB20Calls::UNPAUSE_ROLE(_) => Self::UNPAUSE_ROLE(IB20::UNPAUSE_ROLECall {}),
            IB20V1::IB20Calls::METADATA_ROLE(_) => Self::METADATA_ROLE(IB20::METADATA_ROLECall {}),
            IB20V1::IB20Calls::TRANSFER_SENDER_POLICY(_) => {
                Self::TRANSFER_SENDER_POLICY(IB20::TRANSFER_SENDER_POLICYCall {})
            }
            IB20V1::IB20Calls::TRANSFER_RECEIVER_POLICY(_) => {
                Self::TRANSFER_RECEIVER_POLICY(IB20::TRANSFER_RECEIVER_POLICYCall {})
            }
            IB20V1::IB20Calls::TRANSFER_EXECUTOR_POLICY(_) => {
                Self::TRANSFER_EXECUTOR_POLICY(IB20::TRANSFER_EXECUTOR_POLICYCall {})
            }
            IB20V1::IB20Calls::MINT_RECEIVER_POLICY(_) => {
                Self::MINT_RECEIVER_POLICY(IB20::MINT_RECEIVER_POLICYCall {})
            }
            IB20V1::IB20Calls::name(_) => Self::name(IB20::nameCall {}),
            IB20V1::IB20Calls::symbol(_) => Self::symbol(IB20::symbolCall {}),
            IB20V1::IB20Calls::decimals(_) => Self::decimals(IB20::decimalsCall {}),
            IB20V1::IB20Calls::totalSupply(_) => Self::totalSupply(IB20::totalSupplyCall {}),
            IB20V1::IB20Calls::balanceOf(c) => {
                Self::balanceOf(IB20::balanceOfCall { account: c.account })
            }
            IB20V1::IB20Calls::allowance(c) => {
                Self::allowance(IB20::allowanceCall { owner: c.owner, spender: c.spender })
            }
            IB20V1::IB20Calls::transfer(c) => {
                Self::transfer(IB20::transferCall { to: c.to, amount: c.amount })
            }
            IB20V1::IB20Calls::transferFrom(c) => Self::transferFrom(IB20::transferFromCall {
                from: c.from,
                to: c.to,
                amount: c.amount,
            }),
            IB20V1::IB20Calls::approve(c) => {
                Self::approve(IB20::approveCall { spender: c.spender, amount: c.amount })
            }
            IB20V1::IB20Calls::updateName(c) => {
                Self::updateName(IB20::updateNameCall { newName: c.newName })
            }
            IB20V1::IB20Calls::updateSymbol(c) => {
                Self::updateSymbol(IB20::updateSymbolCall { newSymbol: c.newSymbol })
            }
            IB20V1::IB20Calls::transferWithMemo(c) => {
                Self::transferWithMemo(IB20::transferWithMemoCall {
                    to: c.to,
                    amount: c.amount,
                    memo: c.memo,
                })
            }
            IB20V1::IB20Calls::transferFromWithMemo(c) => {
                Self::transferFromWithMemo(IB20::transferFromWithMemoCall {
                    from: c.from,
                    to: c.to,
                    amount: c.amount,
                    memo: c.memo,
                })
            }
            IB20V1::IB20Calls::mint(c) => {
                Self::mint(IB20::mintCall { to: c.to, amount: c.amount })
            }
            IB20V1::IB20Calls::mintWithMemo(c) => Self::mintWithMemo(IB20::mintWithMemoCall {
                to: c.to,
                amount: c.amount,
                memo: c.memo,
            }),
            IB20V1::IB20Calls::burn(c) => Self::burn(IB20::burnCall { amount: c.amount }),
            IB20V1::IB20Calls::burnWithMemo(c) => {
                Self::burnWithMemo(IB20::burnWithMemoCall { amount: c.amount, memo: c.memo })
            }
            IB20V1::IB20Calls::burnBlocked(c) => {
                Self::burnBlocked(IB20::burnBlockedCall { from: c.from, amount: c.amount })
            }
            IB20V1::IB20Calls::hasRole(c) => {
                Self::hasRole(IB20::hasRoleCall { role: c.role, account: c.account })
            }
            IB20V1::IB20Calls::getRoleAdmin(c) => {
                Self::getRoleAdmin(IB20::getRoleAdminCall { role: c.role })
            }
            IB20V1::IB20Calls::grantRole(c) => {
                Self::grantRole(IB20::grantRoleCall { role: c.role, account: c.account })
            }
            IB20V1::IB20Calls::revokeRole(c) => {
                Self::revokeRole(IB20::revokeRoleCall { role: c.role, account: c.account })
            }
            IB20V1::IB20Calls::renounceRole(c) => Self::renounceRole(IB20::renounceRoleCall {
                role: c.role,
                callerConfirmation: c.callerConfirmation,
            }),
            IB20V1::IB20Calls::renounceLastAdmin(_) => {
                Self::renounceLastAdmin(IB20::renounceLastAdminCall {})
            }
            IB20V1::IB20Calls::setRoleAdmin(c) => Self::setRoleAdmin(IB20::setRoleAdminCall {
                role: c.role,
                newAdminRole: c.newAdminRole,
            }),
            IB20V1::IB20Calls::pausedFeatures(_) => {
                Self::pausedFeatures(IB20::pausedFeaturesCall {})
            }
            IB20V1::IB20Calls::isPaused(c) => {
                Self::isPaused(IB20::isPausedCall { feature: lift_pausable_feature(c.feature) })
            }
            IB20V1::IB20Calls::pause(c) => Self::pause(IB20::pauseCall {
                features: c.features.into_iter().map(lift_pausable_feature).collect(),
            }),
            IB20V1::IB20Calls::unpause(c) => Self::unpause(IB20::unpauseCall {
                features: c.features.into_iter().map(lift_pausable_feature).collect(),
            }),
            IB20V1::IB20Calls::policyId(c) => {
                Self::policyId(IB20::policyIdCall { policyScope: c.policyScope })
            }
            IB20V1::IB20Calls::updatePolicy(c) => Self::updatePolicy(IB20::updatePolicyCall {
                policyScope: c.policyScope,
                newPolicyId: c.newPolicyId,
            }),
            IB20V1::IB20Calls::supplyCap(_) => Self::supplyCap(IB20::supplyCapCall {}),
            IB20V1::IB20Calls::updateSupplyCap(c) => {
                Self::updateSupplyCap(IB20::updateSupplyCapCall { newSupplyCap: c.newSupplyCap })
            }
            IB20V1::IB20Calls::DOMAIN_SEPARATOR(_) => {
                Self::DOMAIN_SEPARATOR(IB20::DOMAIN_SEPARATORCall {})
            }
            IB20V1::IB20Calls::nonces(c) => Self::nonces(IB20::noncesCall { owner: c.owner }),
            IB20V1::IB20Calls::permit(c) => Self::permit(IB20::permitCall {
                owner: c.owner,
                spender: c.spender,
                value: c.value,
                deadline: c.deadline,
                v: c.v,
                r: c.r,
                s: c.s,
            }),
            IB20V1::IB20Calls::eip712Domain(_) => Self::eip712Domain(IB20::eip712DomainCall {}),
            IB20V1::IB20Calls::contractURI(_) => Self::contractURI(IB20::contractURICall {}),
            IB20V1::IB20Calls::updateContractURI(c) => {
                Self::updateContractURI(IB20::updateContractURICall { newURI: c.newURI })
            }
        }
    }
}

fn lift_pausable_feature(feature: IB20V1::PausableFeature) -> IB20::PausableFeature {
    IB20::PausableFeature::try_from(feature as u8)
        .expect("V1 PausableFeature discriminant must lift into V2")
}
