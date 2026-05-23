//! EVM storage adapter for the stablecoin B-20 variant.

use alloc::string::String;

use alloy_primitives::{Address, B256, LogData, U256};
use base_precompile_macros::{Storable, contract};
use base_precompile_storage::{BasePrecompileError, ContractStorage, Handler, Result, StorageCtx};

use super::accounting::StablecoinAccounting;
use crate::{
    B20CoreStorage, B20PolicyType, B20TokenRole, B20Variant, IB20, IB20Factory, TokenAccounting,
};

/// Stablecoin-specific B-20 storage rooted at the `base.b20.stablecoin` ERC-7201 namespace.
#[derive(Debug, Clone, Storable)]
#[namespace("base.b20.stablecoin")]
pub struct B20StablecoinExtensionStorage {
    /// Stablecoin currency identifier.
    pub currency: String, // offset 0
}

/// EVM-backed storage for a stablecoin B-20 token.
#[contract]
pub struct B20StablecoinStorage {
    pub b20: B20CoreStorage,
    pub stablecoin: B20StablecoinExtensionStorage,
}

/// Creation-time parameters for a stablecoin B-20 token.
///
/// Passed to [`B20StablecoinStorage::initialize`] to write all fields atomically.
#[derive(Debug)]
pub struct B20StablecoinInit {
    /// ERC-20 token name.
    pub name: String,
    /// ERC-20 token symbol.
    pub symbol: String,
    /// Maximum total supply.
    pub supply_cap: U256,
    /// ISO 4217 fiat currency code (e.g. `"USD"`).
    pub currency: String,
}

impl<'a> B20StablecoinStorage<'a> {
    /// Creates a `B20StablecoinStorage` instance targeting `addr`.
    pub fn from_address(addr: Address, storage: StorageCtx<'a>) -> Self {
        Self::__new(addr, storage)
    }

    /// Writes all creation-time fields atomically.
    ///
    /// Validates that `currency` contains only `A-Z` characters before writing
    /// anything; reverts `ITokenFactory::InvalidCurrency` otherwise.
    pub fn initialize(&mut self, init: B20StablecoinInit) -> Result<()> {
        if init.currency.is_empty() || !init.currency.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err(BasePrecompileError::revert(IB20Factory::InvalidCurrency {
                code: init.currency,
            }));
        }
        self.b20.name.write(init.name)?;
        self.b20.symbol.write(init.symbol)?;
        self.b20.supply_cap.write(init.supply_cap)?;
        self.stablecoin.currency.write(init.currency)
    }
}

impl TokenAccounting for B20StablecoinStorage<'_> {
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

impl B20StablecoinStorage<'_> {
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

impl StablecoinAccounting for B20StablecoinStorage<'_> {
    fn currency(&self) -> Result<String> {
        self.stablecoin.currency.read()
    }

    fn set_currency(&mut self, currency: String) -> Result<()> {
        self.stablecoin.currency.write(currency)
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::String;

    use alloy_primitives::{Address, U256, address, uint};
    use base_precompile_storage::{Handler, StorableType, StorageCtx, setup_storage};

    use super::{
        __packing_b20_stablecoin_extension_storage, B20StablecoinExtensionStorage,
        B20StablecoinStorage, slots,
    };
    use crate::B20CoreStorage;

    const TOKEN: Address = address!("000000000000000000000000000000000000b022");
    const B20_ROOT: U256 =
        uint!(0xc78b71fee795ddd74aff64ea9b2474194c938c3196430e10bb5f01ed48434000_U256);
    const STABLECOIN_ROOT: U256 =
        uint!(0x35827975a06ca0e9367ea3129b19441d45d0ca58e30b7693f09e73d0943d6200_U256);

    #[test]
    fn stablecoin_namespaces_match_base_std_roots() {
        assert_eq!(<B20CoreStorage as StorableType>::STORAGE_NAMESPACE_ROOT, B20_ROOT);
        assert_eq!(
            <B20StablecoinExtensionStorage as StorableType>::STORAGE_NAMESPACE_ID,
            "base.b20.stablecoin"
        );
        assert_eq!(
            <B20StablecoinExtensionStorage as StorableType>::STORAGE_NAMESPACE_ROOT,
            STABLECOIN_ROOT
        );

        assert_eq!(slots::B20, B20_ROOT);
        assert_eq!(slots::STABLECOIN, STABLECOIN_ROOT);
        assert_eq!(__packing_b20_stablecoin_extension_storage::CURRENCY_LOC.offset_slots, 0);
    }

    #[test]
    fn stablecoin_currency_is_rooted_at_extension_namespace() {
        let (mut storage, _) = setup_storage();

        StorageCtx::enter(&mut storage, |ctx| {
            let mut token = B20StablecoinStorage::from_address(TOKEN, ctx);
            token.b20.name.write(String::from("Stablecoin")).unwrap();
            token.stablecoin.currency.write(String::from("USD")).unwrap();

            assert_eq!(ctx.sload(TOKEN, B20_ROOT).unwrap(), short_string_word("Stablecoin"));
            assert_eq!(ctx.sload(TOKEN, STABLECOIN_ROOT).unwrap(), short_string_word("USD"));
        });
    }

    fn short_string_word(value: &str) -> U256 {
        let mut word = [0u8; 32];
        word[..value.len()].copy_from_slice(value.as_bytes());
        word[31] = (value.len() * 2) as u8;
        U256::from_be_bytes(word)
    }
}
