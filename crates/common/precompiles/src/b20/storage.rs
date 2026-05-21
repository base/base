//! `B20TokenStorage` stores the EVM storage layout for B-20 tokens.

use alloc::string::String;

use alloy_primitives::{Address, B256, LogData, U256};
use base_precompile_macros::contract;
use base_precompile_storage::{
    BasePrecompileError, ContractStorage, Handler, Mapping, Result, StorageCtx,
};

use crate::{B20PolicyType, IB20, TokenAccounting, TokenVariant};

#[contract]
pub struct B20TokenStorage {
    pub total_supply: U256,                                   // slot 0
    pub supply_cap: U256,                                     // slot 1
    pub balances: Mapping<Address, U256>,                     // slot 2
    pub allowances: Mapping<Address, Mapping<Address, U256>>, // slot 3
    pub paused: U256,                                         // slot 4
    pub nonces: Mapping<Address, U256>,                       // slot 5
    pub name: String,                                         // slot 6
    pub symbol: String,                                       // slot 7
    pub minimum_redeemable: U256,                             // slot 8
    pub contract_uri: String,                                 // slot 9
    // slot 10 previously held pre-production capabilities; Beryl starts with fresh B-20 storage.
    pub roles: Mapping<B256, Mapping<Address, bool>>, // slot 10
    pub role_member_counts: Mapping<B256, U256>,      // slot 11
    pub role_admins: Mapping<B256, B256>,             // slot 12
    pub transfer_policy_ids: U256, // slot 13: sender, receiver, executor, reserved
    pub mint_policy_ids: U256,     // slot 14: receiver, reserved, reserved, reserved
    pub stablecoin_currency: String, // slot 15
    pub security_isin: String,     // slot 16
}

impl<'a> B20TokenStorage<'a> {
    /// Creates a `B20TokenStorage` instance targeting `addr`.
    ///
    /// Used by the factory to initialize token storage at a dynamically computed address.
    pub fn from_address(addr: Address, storage: StorageCtx<'a>) -> Self {
        Self::__new(addr, storage)
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
        self.balances.at(&account).read()
    }

    fn set_balance(&mut self, account: Address, balance: U256) -> Result<()> {
        self.balances.at_mut(&account).write(balance)
    }

    fn allowance(&self, owner: Address, spender: Address) -> Result<U256> {
        self.allowances.at(&owner).at(&spender).read()
    }

    fn set_allowance(&mut self, owner: Address, spender: Address, amount: U256) -> Result<()> {
        self.allowances.at_mut(&owner).at_mut(&spender).write(amount)
    }

    fn total_supply(&self) -> Result<U256> {
        self.total_supply.read()
    }

    fn set_total_supply(&mut self, supply: U256) -> Result<()> {
        self.total_supply.write(supply)
    }

    fn supply_cap(&self) -> Result<U256> {
        self.supply_cap.read()
    }

    fn set_supply_cap(&mut self, cap: U256) -> Result<()> {
        self.supply_cap.write(cap)
    }

    fn name(&self) -> Result<String> {
        self.name.read()
    }

    fn set_name(&mut self, name: String) -> Result<()> {
        self.name.write(name)
    }

    fn symbol(&self) -> Result<String> {
        self.symbol.read()
    }

    fn set_symbol(&mut self, symbol: String) -> Result<()> {
        self.symbol.write(symbol)
    }

    fn decimals(&self) -> Result<u8> {
        Ok(TokenVariant::from_address(self.address).map_or(0, TokenVariant::decimals))
    }

    fn currency(&self) -> Result<String> {
        self.stablecoin_currency.read()
    }

    fn security_identifier(&self, identifier_type: &str) -> Result<String> {
        if identifier_type == "ISIN" { self.security_isin.read() } else { Ok(String::new()) }
    }

    fn paused(&self) -> Result<U256> {
        self.paused.read()
    }

    fn set_paused(&mut self, vectors: U256) -> Result<()> {
        self.paused.write(vectors)
    }

    fn nonce(&self, owner: Address) -> Result<U256> {
        self.nonces.at(&owner).read()
    }

    fn increment_nonce(&mut self, owner: Address) -> Result<()> {
        let current = self.nonces.at(&owner).read()?;
        let next =
            current.checked_add(U256::ONE).ok_or_else(BasePrecompileError::under_overflow)?;
        self.nonces.at_mut(&owner).write(next)
    }

    fn minimum_redeemable(&self) -> Result<U256> {
        self.minimum_redeemable.read()
    }

    fn set_minimum_redeemable(&mut self, minimum: U256) -> Result<()> {
        self.minimum_redeemable.write(minimum)
    }

    fn contract_uri(&self) -> Result<String> {
        self.contract_uri.read()
    }

    fn set_contract_uri(&mut self, uri: String) -> Result<()> {
        self.contract_uri.write(uri)
    }

    fn has_role(&self, role: B256, account: Address) -> Result<bool> {
        self.roles.at(&role).at(&account).read()
    }

    fn set_role(&mut self, role: B256, account: Address, enabled: bool) -> Result<()> {
        self.roles.at_mut(&role).at_mut(&account).write(enabled)
    }

    fn role_member_count(&self, role: B256) -> Result<U256> {
        self.role_member_counts.at(&role).read()
    }

    fn set_role_member_count(&mut self, role: B256, count: U256) -> Result<()> {
        self.role_member_counts.at_mut(&role).write(count)
    }

    fn role_admin(&self, role: B256) -> Result<B256> {
        self.role_admins.at(&role).read()
    }

    fn set_role_admin(&mut self, role: B256, admin_role: B256) -> Result<()> {
        self.role_admins.at_mut(&role).write(admin_role)
    }

    fn policy_id(&self, policy_type: B256) -> Result<u64> {
        let policy_type = Self::require_policy_type(policy_type)?;
        match policy_type {
            B20PolicyType::TransferSender => Ok(Self::read_policy_lane(
                self.transfer_policy_ids.read()?,
                Self::TRANSFER_SENDER_POLICY_LANE,
            )),
            B20PolicyType::TransferReceiver => Ok(Self::read_policy_lane(
                self.transfer_policy_ids.read()?,
                Self::TRANSFER_RECEIVER_POLICY_LANE,
            )),
            B20PolicyType::TransferExecutor => Ok(Self::read_policy_lane(
                self.transfer_policy_ids.read()?,
                Self::TRANSFER_EXECUTOR_POLICY_LANE,
            )),
            B20PolicyType::MintReceiver => Ok(Self::read_policy_lane(
                self.mint_policy_ids.read()?,
                Self::MINT_RECEIVER_POLICY_LANE,
            )),
        }
    }

    fn set_policy_id(&mut self, policy_type: B256, policy_id: u64) -> Result<()> {
        let policy_type = Self::require_policy_type(policy_type)?;
        match policy_type {
            B20PolicyType::TransferSender => {
                let packed = Self::write_policy_lane(
                    self.transfer_policy_ids.read()?,
                    Self::TRANSFER_SENDER_POLICY_LANE,
                    policy_id,
                );
                self.transfer_policy_ids.write(packed)
            }
            B20PolicyType::TransferReceiver => {
                let packed = Self::write_policy_lane(
                    self.transfer_policy_ids.read()?,
                    Self::TRANSFER_RECEIVER_POLICY_LANE,
                    policy_id,
                );
                self.transfer_policy_ids.write(packed)
            }
            B20PolicyType::TransferExecutor => {
                let packed = Self::write_policy_lane(
                    self.transfer_policy_ids.read()?,
                    Self::TRANSFER_EXECUTOR_POLICY_LANE,
                    policy_id,
                );
                self.transfer_policy_ids.write(packed)
            }
            B20PolicyType::MintReceiver => {
                let packed = Self::write_policy_lane(
                    self.mint_policy_ids.read()?,
                    Self::MINT_RECEIVER_POLICY_LANE,
                    policy_id,
                );
                self.mint_policy_ids.write(packed)
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
