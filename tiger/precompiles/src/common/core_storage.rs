//! Core B-20 EVM storage layout shared by all token variants, and the token storage adapter.

use alloc::string::String;
use core::marker::PhantomData;

use alloy_primitives::{Address, B256, LogData, U256, uint};
use base_tiger_precompile_storage::{BasePrecompileError, Result, StorageCtx, StorageLayout};

use crate::{B20PolicyType, B20TokenRole, IB20, TokenAccounting, TransferPolicyIds};

/// Core B-20 storage layout, rooted at the `base.b20` ERC-7201 namespace.
///
/// Mirrors base-std's `B20Storage`. Slots are append-only.
#[derive(Debug)]
pub struct B20CoreStorage;

impl B20CoreStorage {
    /// ERC-7201 namespace id.
    pub const NAMESPACE: &'static str = "base.b20";
    /// ERC-7201 namespace root.
    pub const ROOT: U256 =
        uint!(0xc78b71fee795ddd74aff64ea9b2474194c938c3196430e10bb5f01ed48434000_U256);
    /// `string name`.
    pub const NAME: U256 = StorageLayout::field(Self::ROOT, 0);
    /// `string symbol`.
    pub const SYMBOL: U256 = StorageLayout::field(Self::ROOT, 1);
    /// `string contractURI` (ERC-7572).
    pub const CONTRACT_URI: U256 = StorageLayout::field(Self::ROOT, 2);
    /// `uint256 totalSupply`.
    pub const TOTAL_SUPPLY: U256 = StorageLayout::field(Self::ROOT, 3);
    /// `mapping(address account => uint256) balances`.
    pub const BALANCES: U256 = StorageLayout::field(Self::ROOT, 4);
    /// `mapping(address owner => mapping(address spender => uint256)) allowances`.
    pub const ALLOWANCES: U256 = StorageLayout::field(Self::ROOT, 5);
    /// `mapping(bytes32 role => mapping(address account => bool)) roles`.
    pub const ROLES: U256 = StorageLayout::field(Self::ROOT, 6);
    /// `mapping(bytes32 role => bytes32 adminRole) roleAdmins`.
    pub const ROLE_ADMINS: U256 = StorageLayout::field(Self::ROOT, 7);
    /// `uint256 adminCount`: default-admin holder count.
    pub const ADMIN_COUNT: U256 = StorageLayout::field(Self::ROOT, 8);
    /// `uint64` transfer sender, receiver and executor policy ids at byte offsets 0, 8 and 16,
    /// and 8 reserved bytes.
    pub const TRANSFER_POLICY_IDS: U256 = StorageLayout::field(Self::ROOT, 9);
    /// `uint64` mint receiver policy id at byte offset 0, and 24 reserved bytes.
    pub const MINT_POLICY_IDS: U256 = StorageLayout::field(Self::ROOT, 10);
    /// `uint256 paused`: paused feature bitmask.
    pub const PAUSED: U256 = StorageLayout::field(Self::ROOT, 11);
    /// `uint256 supplyCap`.
    pub const SUPPLY_CAP: U256 = StorageLayout::field(Self::ROOT, 12);
    /// `mapping(address owner => uint256) nonces`: EIP-2612 permit nonces.
    pub const NONCES: U256 = StorageLayout::field(Self::ROOT, 13);
    /// `uint64` seize-exempt and seize-receiver policy ids at byte offsets 0 and 8, and 16
    /// reserved bytes.
    ///
    /// The base-std mock keeps an `initialized` bootstrap flag after this; this implementation
    /// checks factory initialization through the deployed marker code instead.
    pub const SEIZE_POLICY_IDS: U256 = StorageLayout::field(Self::ROOT, 14);

    /// Returns the slot and byte offset of the `uint64` policy id for `policy_type`.
    pub const fn policy_id_location(policy_type: B20PolicyType) -> (U256, usize) {
        match policy_type {
            B20PolicyType::TransferSender => (Self::TRANSFER_POLICY_IDS, 0),
            B20PolicyType::TransferReceiver => (Self::TRANSFER_POLICY_IDS, 8),
            B20PolicyType::TransferExecutor => (Self::TRANSFER_POLICY_IDS, 16),
            B20PolicyType::MintReceiver => (Self::MINT_POLICY_IDS, 0),
            B20PolicyType::SeizeExempt => (Self::SEIZE_POLICY_IDS, 0),
            B20PolicyType::SeizeReceiver => (Self::SEIZE_POLICY_IDS, 8),
        }
    }

    /// Returns the policy id location for `policy_scope`, reverting for unknown scopes.
    pub fn policy_scope_location(policy_scope: B256) -> Result<(U256, usize)> {
        B20PolicyType::from_id(policy_scope).map(Self::policy_id_location).ok_or_else(|| {
            BasePrecompileError::revert(IB20::UnsupportedPolicyType { policyScope: policy_scope })
        })
    }
}

/// EVM-backed storage of the B-20 token at `address`: the [`B20CoreStorage`] layout plus the
/// extension layout of token kind `K`.
#[derive(Debug)]
pub struct B20TokenStorage<'a, K> {
    /// Token address.
    pub address: Address,
    /// Storage context of the current call.
    pub storage: StorageCtx<'a>,
    kind: PhantomData<K>,
}

impl<'a, K> B20TokenStorage<'a, K> {
    /// Creates the storage adapter for the token at `address`.
    pub const fn from_address(address: Address, storage: StorageCtx<'a>) -> Self {
        Self { address, storage, kind: PhantomData }
    }
}

/// The part of [`TokenAccounting`] that differs between token kinds.
pub trait B20TokenKind: Sized {
    /// Returns the token's decimals.
    fn decimals(token: &B20TokenStorage<'_, Self>) -> Result<u8>;
}

impl<K: B20TokenKind> TokenAccounting for B20TokenStorage<'_, K> {
    fn token_address(&self) -> Address {
        self.address
    }

    fn is_initialized(&self) -> Result<bool> {
        self.storage.has_bytecode(self.address)
    }

    fn balance_of(&self, account: Address) -> Result<U256> {
        let slot = StorageLayout::mapping(account.as_slice(), B20CoreStorage::BALANCES);
        self.storage.read(self.address, slot)
    }

    fn set_balance(&mut self, account: Address, balance: U256) -> Result<()> {
        let slot = StorageLayout::mapping(account.as_slice(), B20CoreStorage::BALANCES);
        self.storage.write(self.address, slot, balance)
    }

    fn allowance(&self, owner: Address, spender: Address) -> Result<U256> {
        let owner_slot = StorageLayout::mapping(owner.as_slice(), B20CoreStorage::ALLOWANCES);
        self.storage.read(self.address, StorageLayout::mapping(spender.as_slice(), owner_slot))
    }

    fn set_allowance(&mut self, owner: Address, spender: Address, amount: U256) -> Result<()> {
        let owner_slot = StorageLayout::mapping(owner.as_slice(), B20CoreStorage::ALLOWANCES);
        let slot = StorageLayout::mapping(spender.as_slice(), owner_slot);
        self.storage.write(self.address, slot, amount)
    }

    fn total_supply(&self) -> Result<U256> {
        self.storage.read(self.address, B20CoreStorage::TOTAL_SUPPLY)
    }

    fn set_total_supply(&mut self, supply: U256) -> Result<()> {
        self.storage.write(self.address, B20CoreStorage::TOTAL_SUPPLY, supply)
    }

    fn supply_cap(&self) -> Result<U256> {
        self.storage.read(self.address, B20CoreStorage::SUPPLY_CAP)
    }

    fn set_supply_cap(&mut self, cap: U256) -> Result<()> {
        self.storage.write(self.address, B20CoreStorage::SUPPLY_CAP, cap)
    }

    fn name(&self) -> Result<String> {
        self.storage.read_string(self.address, B20CoreStorage::NAME)
    }

    fn set_name(&mut self, name: String) -> Result<()> {
        self.storage.write_string(self.address, B20CoreStorage::NAME, &name)
    }

    fn symbol(&self) -> Result<String> {
        self.storage.read_string(self.address, B20CoreStorage::SYMBOL)
    }

    fn set_symbol(&mut self, symbol: String) -> Result<()> {
        self.storage.write_string(self.address, B20CoreStorage::SYMBOL, &symbol)
    }

    fn decimals(&self) -> Result<u8> {
        K::decimals(self)
    }

    fn paused(&self) -> Result<U256> {
        self.storage.read(self.address, B20CoreStorage::PAUSED)
    }

    fn set_paused(&mut self, vectors: U256) -> Result<()> {
        self.storage.write(self.address, B20CoreStorage::PAUSED, vectors)
    }

    fn nonce(&self, owner: Address) -> Result<U256> {
        let slot = StorageLayout::mapping(owner.as_slice(), B20CoreStorage::NONCES);
        self.storage.read(self.address, slot)
    }

    fn increment_nonce(&mut self, owner: Address) -> Result<()> {
        let slot = StorageLayout::mapping(owner.as_slice(), B20CoreStorage::NONCES);
        let current: U256 = self.storage.read(self.address, slot)?;
        let next = current.checked_add(U256::ONE).ok_or_else(BasePrecompileError::under_overflow)?;
        self.storage.write(self.address, slot, next)
    }

    fn contract_uri(&self) -> Result<String> {
        self.storage.read_string(self.address, B20CoreStorage::CONTRACT_URI)
    }

    fn set_contract_uri(&mut self, uri: String) -> Result<()> {
        self.storage.write_string(self.address, B20CoreStorage::CONTRACT_URI, &uri)
    }

    fn has_role(&self, role: B256, account: Address) -> Result<bool> {
        let role_slot = StorageLayout::mapping(role.as_slice(), B20CoreStorage::ROLES);
        self.storage.read(self.address, StorageLayout::mapping(account.as_slice(), role_slot))
    }

    fn set_role(&mut self, role: B256, account: Address, enabled: bool) -> Result<()> {
        let role_slot = StorageLayout::mapping(role.as_slice(), B20CoreStorage::ROLES);
        let slot = StorageLayout::mapping(account.as_slice(), role_slot);
        self.storage.write(self.address, slot, enabled)
    }

    fn role_member_count(&self, role: B256) -> Result<U256> {
        if role == B20TokenRole::DefaultAdmin.id() {
            self.storage.read(self.address, B20CoreStorage::ADMIN_COUNT)
        } else {
            Ok(U256::ZERO)
        }
    }

    fn set_role_member_count(&mut self, role: B256, count: U256) -> Result<()> {
        if role == B20TokenRole::DefaultAdmin.id() {
            self.storage.write(self.address, B20CoreStorage::ADMIN_COUNT, count)
        } else {
            Ok(())
        }
    }

    fn role_admin(&self, role: B256) -> Result<B256> {
        let slot = StorageLayout::mapping(role.as_slice(), B20CoreStorage::ROLE_ADMINS);
        self.storage.read(self.address, slot)
    }

    fn set_role_admin(&mut self, role: B256, admin_role: B256) -> Result<()> {
        let slot = StorageLayout::mapping(role.as_slice(), B20CoreStorage::ROLE_ADMINS);
        self.storage.write(self.address, slot, admin_role)
    }

    fn policy_id(&self, policy_scope: B256) -> Result<u64> {
        let (slot, offset) = B20CoreStorage::policy_scope_location(policy_scope)?;
        let id = self.storage.read_packed(self.address, slot, offset, StorageLayout::U64_BYTES)?;
        Ok(id.to::<u64>())
    }

    /// Reads all three transfer policy ids with the single SLOAD of the word they share.
    fn transfer_policy_ids(&self) -> Result<TransferPolicyIds> {
        let word = self.storage.sload(self.address, B20CoreStorage::TRANSFER_POLICY_IDS)?;
        let lane = |policy_type| {
            let (_, offset) = B20CoreStorage::policy_id_location(policy_type);
            StorageLayout::extract(word, offset, StorageLayout::U64_BYTES).to::<u64>()
        };
        Ok(TransferPolicyIds {
            sender: lane(B20PolicyType::TransferSender),
            receiver: lane(B20PolicyType::TransferReceiver),
            executor: lane(B20PolicyType::TransferExecutor),
        })
    }

    fn set_policy_id(&mut self, policy_scope: B256, policy_id: u64) -> Result<()> {
        let (slot, offset) = B20CoreStorage::policy_scope_location(policy_scope)?;
        let id = U256::from(policy_id);
        self.storage.write_packed(self.address, slot, offset, StorageLayout::U64_BYTES, id)
    }

    fn emit_event(&mut self, log: LogData) -> Result<()> {
        self.storage.emit_event(self.address, log)
    }

    fn metered_keccak256(&self, data: &[u8]) -> Result<B256> {
        self.storage.metered_keccak256(data)
    }

    fn deduct_gas(&self, gas: u64) -> Result<()> {
        self.storage.deduct_gas(gas)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256, address};
    use base_tiger_precompile_storage::{StorageCtx, StorageLayout, setup_storage};

    use crate::{B20AssetStorage, B20CoreStorage, B20PolicyType, TokenAccounting};

    const TOKEN: Address = address!("000000000000000000000000000000000000b021");

    #[test]
    fn b20_namespace_matches_base_std_root() {
        assert_eq!(StorageLayout::erc7201_root(B20CoreStorage::NAMESPACE), B20CoreStorage::ROOT);
    }

    #[test]
    fn b20_core_offsets_match_mock_b20_storage() {
        let slots = [
            B20CoreStorage::NAME,
            B20CoreStorage::SYMBOL,
            B20CoreStorage::CONTRACT_URI,
            B20CoreStorage::TOTAL_SUPPLY,
            B20CoreStorage::BALANCES,
            B20CoreStorage::ALLOWANCES,
            B20CoreStorage::ROLES,
            B20CoreStorage::ROLE_ADMINS,
            B20CoreStorage::ADMIN_COUNT,
            B20CoreStorage::TRANSFER_POLICY_IDS,
            B20CoreStorage::MINT_POLICY_IDS,
            B20CoreStorage::PAUSED,
            B20CoreStorage::SUPPLY_CAP,
            B20CoreStorage::NONCES,
            B20CoreStorage::SEIZE_POLICY_IDS,
        ];
        for (offset, slot) in slots.into_iter().enumerate() {
            assert_eq!(slot, B20CoreStorage::ROOT + U256::from(offset));
        }
    }

    #[test]
    fn policy_ids_pack_into_base_std_lanes() {
        let (mut storage, _) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let mut token = B20AssetStorage::from_address(TOKEN, ctx);
            let ids = [
                (B20PolicyType::TransferSender, 1u64),
                (B20PolicyType::TransferReceiver, 2),
                (B20PolicyType::TransferExecutor, 3),
                (B20PolicyType::MintReceiver, 4),
                (B20PolicyType::SeizeExempt, 5),
                (B20PolicyType::SeizeReceiver, 6),
            ];
            for (policy_type, id) in ids {
                token.set_policy_id(policy_type.id(), id).unwrap();
            }
            let word = |slot| ctx.sload(TOKEN, slot).unwrap();
            let lanes = |ids: &[u64]| {
                let lane = |(i, id): (usize, &u64)| U256::from(*id) << (64 * i);
                ids.iter().enumerate().map(lane).fold(U256::ZERO, |word, lane| word | lane)
            };
            assert_eq!(word(B20CoreStorage::TRANSFER_POLICY_IDS), lanes(&[1, 2, 3]));
            assert_eq!(word(B20CoreStorage::MINT_POLICY_IDS), lanes(&[4]));
            assert_eq!(word(B20CoreStorage::SEIZE_POLICY_IDS), lanes(&[5, 6]));
            for (policy_type, id) in ids {
                assert_eq!(token.policy_id(policy_type.id()).unwrap(), id);
            }
        });
    }
}
