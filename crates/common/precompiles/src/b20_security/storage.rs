//! EVM storage adapter for the security B-20 variant.

use alloc::string::String;

use alloy_primitives::{Address, B256, LogData, U256};
use base_precompile_macros::{Storable, contract};
use base_precompile_storage::{
    BasePrecompileError, ContractStorage, Handler, Mapping, Result, StorageCtx,
};

use super::accounting::SecurityAccounting;
use crate::{B20CoreStorage, B20PolicyType, B20TokenRole, IB20, TokenAccounting, TokenVariant};

/// Security-specific B-20 storage rooted at the `base.b20.security` ERC-7201 namespace.
#[derive(Debug, Clone, Storable)]
#[namespace("base.b20.security")]
pub struct B20SecurityExtensionStorage {
    /// Share-to-token conversion ratio scaled to WAD.
    pub shares_to_tokens_ratio: U256, // offset 0
    /// Announcement IDs that have already been consumed.
    pub used_announcement_ids: Mapping<String, bool>, // offset 1
    /// Security identifier values by identifier type.
    pub identifiers: Mapping<String, String>, // offset 2
}

/// Redemption-specific B-20 storage rooted at the `base.b20.redeem` ERC-7201 namespace.
#[derive(Debug, Clone, Storable)]
#[namespace("base.b20.redeem")]
pub struct B20RedeemStorage {
    /// Minimum share amount required for a redeem operation.
    pub minimum_redeemable: U256, // offset 0
    /// Packed redeem-side policy IDs.
    pub redeem_policy_ids: U256, // offset 1
}

/// EVM-backed storage for a security B-20 token.
#[contract]
pub struct B20SecurityStorage {
    pub b20: B20CoreStorage,
    pub security: B20SecurityExtensionStorage,
    pub redeem: B20RedeemStorage,
}

impl<'a> B20SecurityStorage<'a> {
    /// Creates a `B20SecurityStorage` instance targeting `addr`.
    pub fn from_address(addr: Address, storage: StorageCtx<'a>) -> Self {
        Self::__new(addr, storage)
    }

    /// Writes all creation-time fields atomically.
    ///
    /// `initial_isin` may be empty; when non-empty it is stored under the raw
    /// `"ISIN"` key in the security identifiers mapping.
    pub fn initialize(
        &mut self,
        name: String,
        symbol: String,
        supply_cap: U256,
        initial_shares_to_tokens_ratio: U256,
        initial_isin: String,
        minimum_redeemable: U256,
    ) -> Result<()> {
        self.b20.name.write(name)?;
        self.b20.symbol.write(symbol)?;
        self.b20.supply_cap.write(supply_cap)?;
        self.security.shares_to_tokens_ratio.write(initial_shares_to_tokens_ratio)?;
        self.redeem.minimum_redeemable.write(minimum_redeemable)?;
        if !initial_isin.is_empty() {
            self.security.identifiers.at_mut(&String::from("ISIN")).write(initial_isin)?;
        }
        Ok(())
    }
}

impl TokenAccounting for B20SecurityStorage<'_> {
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
        Ok(TokenVariant::from_address(ContractStorage::address(self))
            .map_or(0, TokenVariant::decimals))
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
            Ok(Self::read_admin_count(self.b20.admin_count_and_initialized.read()?))
        } else {
            Ok(U256::ZERO)
        }
    }

    fn set_role_member_count(&mut self, role: B256, count: U256) -> Result<()> {
        if role == B20TokenRole::DefaultAdmin.id() {
            let packed = self.b20.admin_count_and_initialized.read()?;
            self.b20.admin_count_and_initialized.write(Self::write_admin_count(packed, count)?)
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

    fn policy_id(&self, policy_type: B256) -> Result<u64> {
        let policy_type = Self::require_policy_type(policy_type)?;
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

    fn set_policy_id(&mut self, policy_type: B256, policy_id: u64) -> Result<()> {
        let policy_type = Self::require_policy_type(policy_type)?;
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

impl B20SecurityStorage<'_> {
    const ADMIN_COUNT_BITS: usize = 248;
    const TRANSFER_SENDER_POLICY_LANE: usize = 0;
    const TRANSFER_RECEIVER_POLICY_LANE: usize = 1;
    const TRANSFER_EXECUTOR_POLICY_LANE: usize = 2;
    const MINT_RECEIVER_POLICY_LANE: usize = 0;
    const POLICY_LANE_BITS: usize = 64;

    fn admin_count_mask() -> U256 {
        (U256::ONE << Self::ADMIN_COUNT_BITS) - U256::ONE
    }

    fn read_admin_count(packed: U256) -> U256 {
        packed & Self::admin_count_mask()
    }

    fn write_admin_count(packed: U256, count: U256) -> Result<U256> {
        let mask = Self::admin_count_mask();
        if count > mask {
            return Err(BasePrecompileError::under_overflow());
        }
        Ok((packed & !mask) | count)
    }

    fn require_policy_type(policy_type: B256) -> Result<B20PolicyType> {
        B20PolicyType::from_id(policy_type).ok_or_else(|| {
            BasePrecompileError::revert(IB20::UnsupportedPolicyType { policyType: policy_type })
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

impl SecurityAccounting for B20SecurityStorage<'_> {
    fn shares_to_tokens_ratio(&self) -> Result<U256> {
        self.security.shares_to_tokens_ratio.read()
    }

    fn set_shares_to_tokens_ratio(&mut self, ratio: U256) -> Result<()> {
        self.security.shares_to_tokens_ratio.write(ratio)
    }

    fn security_identifier(&self, identifier_type: &str) -> Result<String> {
        self.security.identifiers.at(&String::from(identifier_type)).read()
    }

    fn set_security_identifier_value(
        &mut self,
        identifier_type: &str,
        value: String,
    ) -> Result<()> {
        let key = String::from(identifier_type);
        if value.is_empty() {
            self.security.identifiers.at_mut(&key).delete()
        } else {
            self.security.identifiers.at_mut(&key).write(value)
        }
    }

    fn minimum_redeemable(&self) -> Result<U256> {
        self.redeem.minimum_redeemable.read()
    }

    fn set_minimum_redeemable(&mut self, minimum: U256) -> Result<()> {
        self.redeem.minimum_redeemable.write(minimum)
    }

    fn is_announcement_id_used(&self, id: &str) -> Result<bool> {
        self.security.used_announcement_ids.at(&String::from(id)).read()
    }

    fn mark_announcement_id_used(&mut self, id: &str) -> Result<()> {
        self.security.used_announcement_ids.at_mut(&String::from(id)).write(true)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256, address, uint};
    use base_precompile_storage::{Handler, StorableType, StorageCtx, StorageKey, setup_storage};

    use super::{
        __packing_b20_redeem_storage, __packing_b20_security_extension_storage, B20RedeemStorage,
        B20SecurityExtensionStorage, B20SecurityStorage, slots,
    };
    use crate::B20CoreStorage;

    const TOKEN: Address = address!("000000000000000000000000000000000000b021");
    const B20_ROOT: U256 =
        uint!(0xc78b71fee795ddd74aff64ea9b2474194c938c3196430e10bb5f01ed48434000_U256);
    const SECURITY_ROOT: U256 =
        uint!(0x4a21e1b7f963e21baf0daffe6bab858a1e5fecef1144f3aca3c0c4534c7ac600_U256);
    const REDEEM_ROOT: U256 =
        uint!(0xc95c24ab0255f9fb9fcdcd524f71c4fe0437265856b7e5e6d0801df0e6cf5100_U256);

    #[test]
    fn security_namespaces_match_base_std_roots() {
        assert_eq!(<B20CoreStorage as StorableType>::STORAGE_NAMESPACE_ROOT, B20_ROOT);
        assert_eq!(
            <B20SecurityExtensionStorage as StorableType>::STORAGE_NAMESPACE_ID,
            "base.b20.security"
        );
        assert_eq!(
            <B20SecurityExtensionStorage as StorableType>::STORAGE_NAMESPACE_ROOT,
            SECURITY_ROOT
        );
        assert_eq!(<B20RedeemStorage as StorableType>::STORAGE_NAMESPACE_ID, "base.b20.redeem");
        assert_eq!(<B20RedeemStorage as StorableType>::STORAGE_NAMESPACE_ROOT, REDEEM_ROOT);

        assert_eq!(slots::B20, B20_ROOT);
        assert_eq!(slots::SECURITY, SECURITY_ROOT);
        assert_eq!(slots::REDEEM, REDEEM_ROOT);
    }

    #[test]
    fn security_extension_offsets_match_mock_storage() {
        assert_eq!(
            __packing_b20_security_extension_storage::SHARES_TO_TOKENS_RATIO_LOC.offset_slots,
            0
        );
        assert_eq!(
            __packing_b20_security_extension_storage::USED_ANNOUNCEMENT_IDS_LOC.offset_slots,
            1
        );
        assert_eq!(__packing_b20_security_extension_storage::IDENTIFIERS_LOC.offset_slots, 2);
        assert_eq!(__packing_b20_redeem_storage::MINIMUM_REDEEMABLE_LOC.offset_slots, 0);
        assert_eq!(__packing_b20_redeem_storage::REDEEM_POLICY_IDS_LOC.offset_slots, 1);
    }

    #[test]
    fn security_string_mapping_slots_use_solidity_string_key_derivation() {
        let (mut storage, _) = setup_storage();
        let announcement_id = String::from("2026-Q1-split");
        let identifier_type = String::from("ISIN");
        let identifier_value = String::from("US0000000000");

        StorageCtx::enter(&mut storage, |ctx| {
            let mut token = B20SecurityStorage::from_address(TOKEN, ctx);
            token.security.used_announcement_ids.at_mut(&announcement_id).write(true).unwrap();
            token
                .security
                .identifiers
                .at_mut(&identifier_type)
                .write(identifier_value.clone())
                .unwrap();
            token.redeem.minimum_redeemable.write(U256::from(10u64)).unwrap();

            let announcement_slot = SECURITY_ROOT
                + U256::from(
                    __packing_b20_security_extension_storage::USED_ANNOUNCEMENT_IDS_LOC
                        .offset_slots,
                );
            let identifiers_slot = SECURITY_ROOT
                + U256::from(
                    __packing_b20_security_extension_storage::IDENTIFIERS_LOC.offset_slots,
                );
            let minimum_slot = REDEEM_ROOT
                + U256::from(__packing_b20_redeem_storage::MINIMUM_REDEEMABLE_LOC.offset_slots);

            assert_eq!(
                ctx.sload(TOKEN, announcement_id.mapping_slot(announcement_slot)).unwrap(),
                U256::ONE
            );
            assert_eq!(
                ctx.sload(TOKEN, identifier_type.mapping_slot(identifiers_slot)).unwrap(),
                short_string_word(&identifier_value)
            );
            assert_eq!(ctx.sload(TOKEN, minimum_slot).unwrap(), U256::from(10u64));
        });
    }

    fn short_string_word(value: &str) -> U256 {
        let mut word = [0u8; 32];
        word[..value.len()].copy_from_slice(value.as_bytes());
        word[31] = (value.len() * 2) as u8;
        U256::from_be_bytes(word)
    }
}
