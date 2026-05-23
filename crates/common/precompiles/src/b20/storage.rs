//! `B20TokenStorage` stores the EVM storage layout for B-20 tokens.

use alloc::string::String;

use alloy_primitives::{Address, B256, LogData, U256};
use base_precompile_macros::{Storable, contract};
use base_precompile_storage::{
    BasePrecompileError, ContractStorage, Handler, Mapping, Result, StorageCtx,
};

use crate::{B20PolicyType, B20TokenRole, B20Variant, IB20, TokenAccounting};

/// Creation-time parameters for a B-20 token.
///
/// Passed to [`B20TokenStorage::initialize`] to write all fields atomically.
#[derive(Debug)]
pub struct B20TokenInit {
    /// Token name.
    pub name: String,
    /// Token symbol.
    pub symbol: String,
    /// Maximum total supply allowed.
    pub supply_cap: U256,
}

/// Core B-20 storage rooted at the `base.b20` ERC-7201 namespace.
#[derive(Debug, Clone, Storable)]
#[namespace("base.b20")]
pub struct B20CoreStorage {
    /// Mutable token name.
    pub name: String, // offset 0
    /// Mutable token symbol.
    pub symbol: String, // offset 1
    /// ERC-7572 contract metadata URI.
    pub contract_uri: String, // offset 2
    /// Total token supply.
    pub total_supply: U256, // offset 3
    /// Token balances by account.
    pub balances: Mapping<Address, U256>, // offset 4
    /// Spending allowances by owner and spender.
    pub allowances: Mapping<Address, Mapping<Address, U256>>, // offset 5
    /// Role membership flags by role and account.
    pub roles: Mapping<B256, Mapping<Address, bool>>, // offset 6
    /// Admin role configured for each role.
    pub role_admins: Mapping<B256, B256>, // offset 7
    /// Default-admin holder count.
    pub admin_count: U256, // offset 8
    /// Packed transfer-side policy IDs.
    pub transfer_policy_ids: U256, // offset 9: sender, receiver, executor, reserved
    /// Packed mint-side policy IDs.
    pub mint_policy_ids: U256, // offset 10: receiver, reserved, reserved, reserved
    /// Paused feature bitmask.
    pub paused: U256, // offset 11
    /// Maximum total supply.
    pub supply_cap: U256, // offset 12
    /// EIP-2612 permit nonces by owner.
    pub nonces: Mapping<Address, U256>, // offset 13
}

/// EVM-backed storage for the default B-20 variant.
#[contract]
pub struct B20TokenStorage {
    pub b20: B20CoreStorage,
}

impl<'a> B20TokenStorage<'a> {
    /// Creates a `B20TokenStorage` instance targeting `addr`.
    ///
    /// Used by the factory to initialize token storage at a dynamically computed address.
    pub fn from_address(addr: Address, storage: StorageCtx<'a>) -> Self {
        Self::__new(addr, storage)
    }

    /// Writes all creation-time fields atomically.
    pub fn initialize(&mut self, init: B20TokenInit) -> Result<()> {
        self.b20.name.write(init.name)?;
        self.b20.symbol.write(init.symbol)?;
        self.b20.supply_cap.write(init.supply_cap)?;
        Ok(())
    }
}

impl TokenAccounting for B20TokenStorage<'_> {
    fn token_address(&self) -> Address {
        ContractStorage::address(self)
    }

    fn is_initialized(&self) -> Result<bool> {
        ContractStorage::is_initialized(self)
    }

    fn balance_of(&self, account: Address) -> Result<U256> {
        self.b20.balances.at(&account).read()
    }

    fn set_balance(&mut self, account: Address, balance: U256) -> Result<()> {
        self.b20.balances.at_mut(&account).write(balance)
    }

    fn allowance(&self, owner: Address, spender: Address) -> Result<U256> {
        self.b20.allowances.at(&owner).at(&spender).read()
    }

    fn set_allowance(&mut self, owner: Address, spender: Address, amount: U256) -> Result<()> {
        self.b20.allowances.at_mut(&owner).at_mut(&spender).write(amount)
    }

    fn total_supply(&self) -> Result<U256> {
        self.b20.total_supply.read()
    }

    fn set_total_supply(&mut self, supply: U256) -> Result<()> {
        self.b20.total_supply.write(supply)
    }

    fn supply_cap(&self) -> Result<U256> {
        self.b20.supply_cap.read()
    }

    fn set_supply_cap(&mut self, cap: U256) -> Result<()> {
        self.b20.supply_cap.write(cap)
    }

    fn name(&self) -> Result<String> {
        self.b20.name.read()
    }

    fn set_name(&mut self, name: String) -> Result<()> {
        self.b20.name.write(name)
    }

    fn symbol(&self) -> Result<String> {
        self.b20.symbol.read()
    }

    fn set_symbol(&mut self, symbol: String) -> Result<()> {
        self.b20.symbol.write(symbol)
    }

    fn decimals(&self) -> Result<u8> {
        Ok(B20Variant::from_address(ContractStorage::address(self)).map_or(0, B20Variant::decimals))
    }

    fn paused(&self) -> Result<U256> {
        self.b20.paused.read()
    }

    fn set_paused(&mut self, vectors: U256) -> Result<()> {
        self.b20.paused.write(vectors)
    }

    fn nonce(&self, owner: Address) -> Result<U256> {
        self.b20.nonces.at(&owner).read()
    }

    fn increment_nonce(&mut self, owner: Address) -> Result<()> {
        let current = self.b20.nonces.at(&owner).read()?;
        let next =
            current.checked_add(U256::ONE).ok_or_else(BasePrecompileError::under_overflow)?;
        self.b20.nonces.at_mut(&owner).write(next)
    }

    fn contract_uri(&self) -> Result<String> {
        self.b20.contract_uri.read()
    }

    fn set_contract_uri(&mut self, uri: String) -> Result<()> {
        self.b20.contract_uri.write(uri)
    }

    fn has_role(&self, role: B256, account: Address) -> Result<bool> {
        self.b20.roles.at(&role).at(&account).read()
    }

    fn set_role(&mut self, role: B256, account: Address, enabled: bool) -> Result<()> {
        self.b20.roles.at_mut(&role).at_mut(&account).write(enabled)
    }

    fn role_member_count(&self, role: B256) -> Result<U256> {
        if role == B20TokenRole::DefaultAdmin.id() {
            self.b20.admin_count.read()
        } else {
            Ok(U256::ZERO)
        }
    }

    fn set_role_member_count(&mut self, role: B256, count: U256) -> Result<()> {
        if role == B20TokenRole::DefaultAdmin.id() {
            self.b20.admin_count.write(count)
        } else {
            Ok(())
        }
    }

    fn role_admin(&self, role: B256) -> Result<B256> {
        let admin_role = self.b20.role_admins.at(&role).read()?;
        if admin_role.is_zero() && role != B20TokenRole::DefaultAdmin.id() {
            Ok(B20TokenRole::DefaultAdmin.id())
        } else {
            Ok(admin_role)
        }
    }

    fn set_role_admin(&mut self, role: B256, admin_role: B256) -> Result<()> {
        self.b20.role_admins.at_mut(&role).write(admin_role)
    }

    fn policy_id(&self, policy_scope: B256) -> Result<u64> {
        let policy_type = Self::require_policy_type(policy_scope)?;
        match policy_type {
            B20PolicyType::TransferSender => Ok(Self::read_policy_lane(
                self.b20.transfer_policy_ids.read()?,
                Self::TRANSFER_SENDER_POLICY_LANE,
            )),
            B20PolicyType::TransferReceiver => Ok(Self::read_policy_lane(
                self.b20.transfer_policy_ids.read()?,
                Self::TRANSFER_RECEIVER_POLICY_LANE,
            )),
            B20PolicyType::TransferExecutor => Ok(Self::read_policy_lane(
                self.b20.transfer_policy_ids.read()?,
                Self::TRANSFER_EXECUTOR_POLICY_LANE,
            )),
            B20PolicyType::MintReceiver => Ok(Self::read_policy_lane(
                self.b20.mint_policy_ids.read()?,
                Self::MINT_RECEIVER_POLICY_LANE,
            )),
        }
    }

    fn set_policy_id(&mut self, policy_scope: B256, policy_id: u64) -> Result<()> {
        let policy_type = Self::require_policy_type(policy_scope)?;
        match policy_type {
            B20PolicyType::TransferSender => {
                let packed = Self::write_policy_lane(
                    self.b20.transfer_policy_ids.read()?,
                    Self::TRANSFER_SENDER_POLICY_LANE,
                    policy_id,
                );
                self.b20.transfer_policy_ids.write(packed)
            }
            B20PolicyType::TransferReceiver => {
                let packed = Self::write_policy_lane(
                    self.b20.transfer_policy_ids.read()?,
                    Self::TRANSFER_RECEIVER_POLICY_LANE,
                    policy_id,
                );
                self.b20.transfer_policy_ids.write(packed)
            }
            B20PolicyType::TransferExecutor => {
                let packed = Self::write_policy_lane(
                    self.b20.transfer_policy_ids.read()?,
                    Self::TRANSFER_EXECUTOR_POLICY_LANE,
                    policy_id,
                );
                self.b20.transfer_policy_ids.write(packed)
            }
            B20PolicyType::MintReceiver => {
                let packed = Self::write_policy_lane(
                    self.b20.mint_policy_ids.read()?,
                    Self::MINT_RECEIVER_POLICY_LANE,
                    policy_id,
                );
                self.b20.mint_policy_ids.write(packed)
            }
        }
    }

    fn emit_event(&mut self, log: LogData) -> Result<()> {
        self.emit_event(log)
    }
}

impl B20TokenStorage<'_> {
    const TRANSFER_SENDER_POLICY_LANE: usize = 0;
    const TRANSFER_RECEIVER_POLICY_LANE: usize = 1;
    const TRANSFER_EXECUTOR_POLICY_LANE: usize = 2;
    const MINT_RECEIVER_POLICY_LANE: usize = 0;
    const POLICY_LANE_BITS: usize = 64;

    fn require_policy_type(policy_scope: B256) -> Result<B20PolicyType> {
        B20PolicyType::from_id(policy_scope).ok_or_else(|| {
            BasePrecompileError::revert(IB20::UnsupportedPolicyType { policyScope: policy_scope })
        })
    }

    fn read_policy_lane(packed: U256, lane: usize) -> u64 {
        ((packed >> (lane * Self::POLICY_LANE_BITS)) & U256::from(u64::MAX)).to::<u64>()
    }

    fn write_policy_lane(packed: U256, lane: usize, policy_id: u64) -> U256 {
        let shift = lane * Self::POLICY_LANE_BITS;
        let mask = U256::from(u64::MAX) << shift;
        (packed & !mask) | (U256::from(policy_id) << shift)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256, address, uint};
    use base_precompile_storage::{Handler, StorableType, StorageCtx, StorageKey, setup_storage};

    use super::{__packing_b20_core_storage, B20CoreStorage, B20TokenStorage, slots};
    use crate::{B20TokenRole, TokenAccounting};

    const TOKEN: Address = address!("000000000000000000000000000000000000b020");
    const B20_ROOT: U256 =
        uint!(0xc78b71fee795ddd74aff64ea9b2474194c938c3196430e10bb5f01ed48434000_U256);

    #[test]
    fn b20_namespaces_match_base_std_roots() {
        assert_eq!(<B20CoreStorage as StorableType>::STORAGE_NAMESPACE_ID, "base.b20");
        assert_eq!(<B20CoreStorage as StorableType>::STORAGE_NAMESPACE_ROOT, B20_ROOT);

        assert_eq!(slots::B20, B20_ROOT);
    }

    #[test]
    fn b20_core_offsets_match_mock_b20_storage() {
        assert_eq!(__packing_b20_core_storage::NAME_LOC.offset_slots, 0);
        assert_eq!(__packing_b20_core_storage::SYMBOL_LOC.offset_slots, 1);
        assert_eq!(__packing_b20_core_storage::CONTRACT_URI_LOC.offset_slots, 2);
        assert_eq!(__packing_b20_core_storage::TOTAL_SUPPLY_LOC.offset_slots, 3);
        assert_eq!(__packing_b20_core_storage::BALANCES_LOC.offset_slots, 4);
        assert_eq!(__packing_b20_core_storage::ALLOWANCES_LOC.offset_slots, 5);
        assert_eq!(__packing_b20_core_storage::ROLES_LOC.offset_slots, 6);
        assert_eq!(__packing_b20_core_storage::ROLE_ADMINS_LOC.offset_slots, 7);
        assert_eq!(__packing_b20_core_storage::ADMIN_COUNT_LOC.offset_slots, 8);
        assert_eq!(__packing_b20_core_storage::TRANSFER_POLICY_IDS_LOC.offset_slots, 9);
        assert_eq!(__packing_b20_core_storage::MINT_POLICY_IDS_LOC.offset_slots, 10);
        assert_eq!(__packing_b20_core_storage::PAUSED_LOC.offset_slots, 11);
        assert_eq!(__packing_b20_core_storage::SUPPLY_CAP_LOC.offset_slots, 12);
        assert_eq!(__packing_b20_core_storage::NONCES_LOC.offset_slots, 13);
    }

    #[test]
    fn b20_core_mapping_slots_are_rooted_at_namespace_offsets() {
        let (mut storage, _) = setup_storage();
        let holder = Address::repeat_byte(0xaa);
        let spender = Address::repeat_byte(0xbb);
        let role = B20TokenRole::Mint.id();

        StorageCtx::enter(&mut storage, |ctx| {
            let mut token = B20TokenStorage::from_address(TOKEN, ctx);
            token.b20.balances.at_mut(&holder).write(U256::from(100)).unwrap();
            token.b20.allowances.at_mut(&holder).at_mut(&spender).write(U256::from(25)).unwrap();
            token.b20.roles.at_mut(&role).at_mut(&holder).write(true).unwrap();
            token.set_role_member_count(B20TokenRole::DefaultAdmin.id(), U256::ONE).unwrap();

            let balances_slot =
                B20_ROOT + U256::from(__packing_b20_core_storage::BALANCES_LOC.offset_slots);
            let allowances_slot =
                B20_ROOT + U256::from(__packing_b20_core_storage::ALLOWANCES_LOC.offset_slots);
            let roles_slot =
                B20_ROOT + U256::from(__packing_b20_core_storage::ROLES_LOC.offset_slots);
            let admin_count_slot =
                B20_ROOT + U256::from(__packing_b20_core_storage::ADMIN_COUNT_LOC.offset_slots);

            assert_eq!(
                ctx.sload(TOKEN, holder.mapping_slot(balances_slot)).unwrap(),
                U256::from(100)
            );
            assert_eq!(
                ctx.sload(TOKEN, spender.mapping_slot(holder.mapping_slot(allowances_slot)))
                    .unwrap(),
                U256::from(25)
            );
            assert_eq!(
                ctx.sload(TOKEN, holder.mapping_slot(role.mapping_slot(roles_slot))).unwrap(),
                U256::ONE
            );
            assert_eq!(ctx.sload(TOKEN, admin_count_slot).unwrap(), U256::ONE);
        });
    }
}
