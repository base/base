//! Bundle metering logic.

use std::{sync::Arc, time::Instant};

use alloy_consensus::{BlockHeader, Transaction as _};
use alloy_evm::block::TxResult as _;
use alloy_primitives::{
    Address, B256, U256,
    map::{HashMap, HashSet},
};
use base_bundles::{BundleExtensions, BundleTxs, OpcodeGas, ParsedBundle, TransactionResult};
use base_common_evm::{BaseSpecId, BaseUpgrade, L1BlockInfo};
use base_common_precompiles::{
    ActivationRegistryStorage, B20FactoryStorage, B20Variant, PolicyRegistryStorage,
};
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::{BaseEvmConfig, BaseNextBlockEnvAttributes};
use eyre::{Result as EyreResult, eyre};
use reth_evm::{ConfigureEvm, Evm as _, execute::BlockBuilder};
use reth_primitives_traits::{Account, SealedHeader};
use reth_revm::{
    database::StateProviderDatabase, db::State, primitives::KECCAK_EMPTY,
    revm::context_interface::cfg::GasParams,
};
use revm::{primitives::hardfork::SpecId, state::EvmState};
use revm_bytecode::opcode::OpCode;

use crate::{inspector::MeteringInspector, transaction::validate_tx};

const BLOCK_TIME: u64 = 2; // 2 seconds per block
// Static floor from the current minimum base fee for metering simulation.
// The protocol has a dynamic min_base_fee via system config, but for metering
// we use a static floor to reject transactions that will never make it onchain.
const MIN_BASEFEE: u64 = 5_000_000;
const MAX_NONCE_AHEAD: u64 = 10_000; // max nonce distance from on-chain state

/// Output from metering a bundle of transactions
#[derive(Debug)]
pub struct MeterBundleOutput {
    /// Transaction results with individual metrics
    pub results: Vec<TransactionResult>,
    /// Total gas used by all transactions
    pub total_gas_used: u64,
    /// Total gas fees paid by all transactions
    pub total_gas_fees: U256,
    /// Bundle hash
    pub bundle_hash: B256,
    /// Total time spent executing the bundle in microseconds.
    pub total_time_us: u128,
}

/// Transaction-level pseudo-opcodes exposed by bundle metering.
///
/// The string representation is the stable CLI/RPC name. Internally, metering
/// uses this enum so comparisons do not depend on repeated string literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PseudoOpcode {
    /// Aggregate of the active transaction intrinsic components.
    IntrinsicTotal,
    /// EIP-2028/EIP-7623 zero-byte transaction data cost.
    IntrinsicTxDataZeroByteCost,
    /// EIP-2028/EIP-7623 non-zero-byte transaction data cost.
    IntrinsicTxDataNonZeroByteCost,
    /// EIP-2930 prepaid access-list address cost.
    IntrinsicAccessListAddressCost,
    /// EIP-2930 prepaid access-list storage-key cost.
    IntrinsicAccessListStorageKeyCost,
    /// EIP-3860 transaction initcode word cost.
    IntrinsicInitcodeWordCost,
    /// EIP-7623 transaction floor-gas candidate.
    TxFloorGas,
    /// Pre-Amsterdam legacy transaction base cost.
    IntrinsicLegacyTxBaseCost,
    /// Pre-Amsterdam legacy contract-creation cost.
    IntrinsicLegacyCreateCost,
    /// EIP-7702 legacy authorization-list empty-account cost.
    IntrinsicPerEmptyAccountCost,
    /// EIP-2780 resource-based transaction base cost.
    IntrinsicTxBaseCost,
    /// EIP-2780 cold account access cost.
    IntrinsicColdAccountAccess,
    /// EIP-2780 transaction value cost.
    IntrinsicTxValueCost,
    /// EIP-2780/EIP-7708 transfer-log cost.
    IntrinsicTransferLogCost,
    /// EIP-2780 account-creation access cost.
    IntrinsicCreateAccess,
    /// EIP-2780 regular authorization base cost.
    IntrinsicRegularPerAuthBaseCost,
    /// Successful top-level ETH transfer to a nonexistent account.
    TxEffectEthTransferToNonexistentAccount,
    /// Successful top-level ETH transfer to an existing account.
    TxEffectEthTransferToExistingAccount,
    /// Successful top-level ETH self-transfer.
    TxEffectEthSelfTransfer,
    /// Zero-to-nonzero storage transitions in post-tx `EvmState`.
    StateNewStorageSlot,
    /// Storage slots whose present value differs from the original value.
    StateChangedStorageSlot,
    /// Nonzero-to-zero storage transitions in post-tx `EvmState`.
    StateClearedStorageSlot,
    /// Accounts marked touched in post-tx `EvmState`.
    StateTouchedAccount,
    /// Accounts whose balance, nonce, or code changed from the original info.
    StateChangedAccount,
}

impl PseudoOpcode {
    /// Returns the stable CLI/RPC name for this pseudo-opcode.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IntrinsicTotal => "INTRINSIC_TOTAL",
            Self::IntrinsicTxDataZeroByteCost => "INTRINSIC_TX_DATA_ZERO_BYTE_COST",
            Self::IntrinsicTxDataNonZeroByteCost => "INTRINSIC_TX_DATA_NON_ZERO_BYTE_COST",
            Self::IntrinsicAccessListAddressCost => "INTRINSIC_ACCESS_LIST_ADDRESS_COST",
            Self::IntrinsicAccessListStorageKeyCost => "INTRINSIC_ACCESS_LIST_STORAGE_KEY_COST",
            Self::IntrinsicInitcodeWordCost => "INTRINSIC_INITCODE_WORD_COST",
            Self::TxFloorGas => "TX_FLOOR_GAS",
            Self::IntrinsicLegacyTxBaseCost => "INTRINSIC_LEGACY_TX_BASE_COST",
            Self::IntrinsicLegacyCreateCost => "INTRINSIC_LEGACY_CREATE_COST",
            Self::IntrinsicPerEmptyAccountCost => "INTRINSIC_PER_EMPTY_ACCOUNT_COST",
            Self::IntrinsicTxBaseCost => "INTRINSIC_TX_BASE_COST",
            Self::IntrinsicColdAccountAccess => "INTRINSIC_COLD_ACCOUNT_ACCESS",
            Self::IntrinsicTxValueCost => "INTRINSIC_TX_VALUE_COST",
            Self::IntrinsicTransferLogCost => "INTRINSIC_TRANSFER_LOG_COST",
            Self::IntrinsicCreateAccess => "INTRINSIC_CREATE_ACCESS",
            Self::IntrinsicRegularPerAuthBaseCost => "INTRINSIC_REGULAR_PER_AUTH_BASE_COST",
            Self::TxEffectEthTransferToNonexistentAccount => {
                "TX_EFFECT_ETH_TRANSFER_TO_NONEXISTENT_ACCOUNT"
            }
            Self::TxEffectEthTransferToExistingAccount => {
                "TX_EFFECT_ETH_TRANSFER_TO_EXISTING_ACCOUNT"
            }
            Self::TxEffectEthSelfTransfer => "TX_EFFECT_ETH_SELF_TRANSFER",
            Self::StateNewStorageSlot => "STATE_NEW_STORAGE_SLOT",
            Self::StateChangedStorageSlot => "STATE_CHANGED_STORAGE_SLOT",
            Self::StateClearedStorageSlot => "STATE_CLEARED_STORAGE_SLOT",
            Self::StateTouchedAccount => "STATE_TOUCHED_ACCOUNT",
            Self::StateChangedAccount => "STATE_CHANGED_ACCOUNT",
        }
    }

    /// Returns whether this pseudo-opcode classifies a top-level ETH transfer.
    pub const fn is_eth_transfer_effect(self) -> bool {
        matches!(
            self,
            Self::TxEffectEthTransferToNonexistentAccount
                | Self::TxEffectEthTransferToExistingAccount
                | Self::TxEffectEthSelfTransfer
        )
    }

    /// Returns whether this pseudo-opcode is a net post-state effect.
    ///
    /// These names match the executed overlay in
    /// `base-execution-payload-builder::ResourceSample`.
    pub const fn is_state_effect(self) -> bool {
        matches!(
            self,
            Self::StateNewStorageSlot
                | Self::StateChangedStorageSlot
                | Self::StateClearedStorageSlot
                | Self::StateTouchedAccount
                | Self::StateChangedAccount
        )
    }
}

/// Opcodes and precompiles to track during bundle metering.
///
/// This is for targeted transaction and bundle simulation. It is not the primary production
/// failure-rate monitoring path for Beryl precompiles.
#[derive(Debug, Clone, Default)]
pub struct MeteredOpcodes {
    /// EVM opcodes to track.
    pub opcodes: HashSet<OpCode>,
    /// Precompile addresses to track, keyed by address with display name.
    pub precompiles: HashMap<Address, String>,
    /// Whether to track dynamic Beryl B-20 asset-token precompile addresses.
    pub beryl_b20_asset_precompiles: bool,
    /// Whether to track dynamic Beryl B-20 stablecoin-token precompile addresses.
    pub beryl_b20_stablecoin_precompiles: bool,
    /// Synthetic transaction-level gas buckets to track.
    pub pseudo_opcodes: HashSet<PseudoOpcode>,
}

/// Constructs a precompile address from a `u16` value.
const fn precompile_addr(n: u16) -> Address {
    let be = n.to_be_bytes();
    Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, be[0], be[1]])
}

/// Standard EVM precompile names and their addresses.
///
/// Names follow EIP-7910 conventions.
const PRECOMPILES: &[(&str, Address)] = &[
    ("ECREC", precompile_addr(0x01)),
    ("SHA256", precompile_addr(0x02)),
    ("RIPEMD160", precompile_addr(0x03)),
    ("ID", precompile_addr(0x04)),
    ("MODEXP", precompile_addr(0x05)),
    ("BN254_ADD", precompile_addr(0x06)),
    ("BN254_MUL", precompile_addr(0x07)),
    ("BN254_PAIRING", precompile_addr(0x08)),
    ("BLAKE2F", precompile_addr(0x09)),
    ("KZG_POINT_EVALUATION", precompile_addr(0x0a)),
    ("BLS12_G1ADD", precompile_addr(0x0b)),
    ("BLS12_G1MSM", precompile_addr(0x0c)),
    ("BLS12_G2ADD", precompile_addr(0x0d)),
    ("BLS12_G2MSM", precompile_addr(0x0e)),
    ("BLS12_PAIRING_CHECK", precompile_addr(0x0f)),
    ("BLS12_MAP_FP_TO_G1", precompile_addr(0x10)),
    ("BLS12_MAP_FP2_TO_G2", precompile_addr(0x11)),
    ("P256VERIFY", precompile_addr(0x100)),
];

const BERYL_B20_FACTORY_PRECOMPILE: &str = "BERYL_B20_FACTORY";
const BERYL_ACTIVATION_REGISTRY_PRECOMPILE: &str = "BERYL_ACTIVATION_REGISTRY";
const BERYL_POLICY_REGISTRY_PRECOMPILE: &str = "BERYL_POLICY_REGISTRY";
const BERYL_B20_ASSET_PRECOMPILE: &str = "BERYL_B20_ASSET";
const BERYL_B20_STABLECOIN_PRECOMPILE: &str = "BERYL_B20_STABLECOIN";

/// Beryl singleton precompile names and their fixed addresses.
const BERYL_PRECOMPILES: &[(&str, Address)] = &[
    (BERYL_B20_FACTORY_PRECOMPILE, B20FactoryStorage::ADDRESS),
    (BERYL_ACTIVATION_REGISTRY_PRECOMPILE, ActivationRegistryStorage::ADDRESS),
    (BERYL_POLICY_REGISTRY_PRECOMPILE, PolicyRegistryStorage::ADDRESS),
];

const PSEUDO_OPCODES: &[PseudoOpcode] = &[
    // EIP-2780: aggregate of the active transaction intrinsic components.
    PseudoOpcode::IntrinsicTotal,
    // EIP-2028 and EIP-7623: transaction data cost.
    PseudoOpcode::IntrinsicTxDataZeroByteCost,
    PseudoOpcode::IntrinsicTxDataNonZeroByteCost,
    // EIP-2930: prepaid access-list entry costs.
    PseudoOpcode::IntrinsicAccessListAddressCost,
    PseudoOpcode::IntrinsicAccessListStorageKeyCost,
    // EIP-3860: transaction initcode jumpdest-analysis cost.
    PseudoOpcode::IntrinsicInitcodeWordCost,
    // EIP-7623: floor candidate, separate from intrinsic gas.
    PseudoOpcode::TxFloorGas,
    // Pre-Amsterdam legacy aggregates. These are not EIP-2780 primitives.
    PseudoOpcode::IntrinsicLegacyTxBaseCost,
    PseudoOpcode::IntrinsicLegacyCreateCost,
    // EIP-7702: legacy authorization-list charge.
    PseudoOpcode::IntrinsicPerEmptyAccountCost,
    // EIP-2780: resource-based intrinsic transaction primitives.
    PseudoOpcode::IntrinsicTxBaseCost,
    PseudoOpcode::IntrinsicColdAccountAccess,
    PseudoOpcode::IntrinsicTxValueCost,
    PseudoOpcode::IntrinsicTransferLogCost,
    PseudoOpcode::IntrinsicCreateAccess,
    PseudoOpcode::IntrinsicRegularPerAuthBaseCost,
    // EIP-2780/EIP-7708: zero-gas top-level ETH-transfer classifiers.
    PseudoOpcode::TxEffectEthTransferToNonexistentAccount,
    PseudoOpcode::TxEffectEthTransferToExistingAccount,
    PseudoOpcode::TxEffectEthSelfTransfer,
    // Net post-state effects from post-tx `EvmState`. These are not opcodes.
    PseudoOpcode::StateNewStorageSlot,
    PseudoOpcode::StateChangedStorageSlot,
    PseudoOpcode::StateClearedStorageSlot,
    PseudoOpcode::StateTouchedAccount,
    PseudoOpcode::StateChangedAccount,
];

impl MeteredOpcodes {
    /// Returns true if no opcodes or precompiles are configured.
    pub fn is_empty(&self) -> bool {
        self.opcodes.is_empty()
            && self.precompiles.is_empty()
            && !self.beryl_b20_asset_precompiles
            && !self.beryl_b20_stablecoin_precompiles
            && self.pseudo_opcodes.is_empty()
    }

    /// Returns true if any fixed or dynamic precompile metering is configured.
    pub fn meters_any_precompile(&self) -> bool {
        !self.precompiles.is_empty()
            || self.beryl_b20_asset_precompiles
            || self.beryl_b20_stablecoin_precompiles
    }

    /// Adds all known standard and Beryl precompiles to the metered set.
    pub fn with_all_precompiles(mut self) -> Self {
        for &(name, addr) in PRECOMPILES {
            self.precompiles.insert(addr, name.to_string());
        }
        for &(name, addr) in BERYL_PRECOMPILES {
            self.precompiles.insert(addr, name.to_string());
        }
        self.beryl_b20_asset_precompiles = true;
        self.beryl_b20_stablecoin_precompiles = true;
        self
    }

    /// Filters the precompile set to those active in `spec`, consuming `self`.
    pub fn for_spec(mut self, spec: BaseSpecId) -> Self {
        if spec.is_enabled_in(BaseUpgrade::Beryl) {
            return self;
        }

        for &(_, addr) in BERYL_PRECOMPILES {
            self.precompiles.remove(&addr);
        }
        self.beryl_b20_asset_precompiles = false;
        self.beryl_b20_stablecoin_precompiles = false;
        self
    }

    /// Returns the configured display name for a metered precompile address.
    pub fn precompile_name(&self, address: Address) -> Option<&str> {
        self.precompiles
            .get(&address)
            .map(String::as_str)
            .or_else(|| self.beryl_b20_token_precompile_name(address))
    }

    /// Returns true when `address` is in the metered precompile set.
    pub fn meters_precompile(&self, address: Address) -> bool {
        self.precompile_name(address).is_some()
    }

    /// Returns the dynamic Beryl B-20 token precompile name for `address`, when enabled.
    pub fn beryl_b20_token_precompile_name(&self, address: Address) -> Option<&'static str> {
        match B20Variant::from_address(address) {
            Some(B20Variant::Asset) if self.beryl_b20_asset_precompiles => {
                Some(BERYL_B20_ASSET_PRECOMPILE)
            }
            Some(B20Variant::Stablecoin) if self.beryl_b20_stablecoin_precompiles => {
                Some(BERYL_B20_STABLECOIN_PRECOMPILE)
            }
            _ => None,
        }
    }

    /// Parses opcode and precompile name strings into a [`MeteredOpcodes`] filter.
    ///
    /// Recognizes EVM opcode names (e.g., `SSTORE`, `CALL`), fixed precompile
    /// names (e.g., `ECREC`, `BLAKE2F`, `BERYL_B20_FACTORY`), dynamic Beryl
    /// B-20 token address family names (`BERYL_B20_ASSET`, `BERYL_B20_STABLECOIN`),
    /// and transaction-level pseudo-opcodes including post-state effects
    /// (`STATE_NEW_STORAGE_SLOT`, `STATE_CHANGED_STORAGE_SLOT`,
    /// `STATE_CLEARED_STORAGE_SLOT`, `STATE_TOUCHED_ACCOUNT`,
    /// `STATE_CHANGED_ACCOUNT`).
    /// Matching is case-insensitive.
    pub fn parse(names: &[String]) -> EyreResult<Self> {
        let opcode_lookup: HashMap<&str, OpCode> =
            (0..=255u8).filter_map(|byte| OpCode::new(byte).map(|op| (op.as_str(), op))).collect();

        let precompile_lookup: HashMap<&str, (Address, &str)> = PRECOMPILES
            .iter()
            .chain(BERYL_PRECOMPILES.iter())
            .map(|&(name, addr)| (name, (addr, name)))
            .collect();
        let pseudo_lookup: HashMap<&str, PseudoOpcode> =
            PSEUDO_OPCODES.iter().map(|&opcode| (opcode.as_str(), opcode)).collect();

        let mut result = Self::default();
        for name in names {
            let upper = name.to_uppercase();
            if let Some(&opcode) = opcode_lookup.get(upper.as_str()) {
                result.opcodes.insert(opcode);
            } else if let Some(&(addr, display_name)) = precompile_lookup.get(upper.as_str()) {
                result.precompiles.insert(addr, display_name.to_string());
            } else if upper == BERYL_B20_ASSET_PRECOMPILE {
                result.beryl_b20_asset_precompiles = true;
            } else if upper == BERYL_B20_STABLECOIN_PRECOMPILE {
                result.beryl_b20_stablecoin_precompiles = true;
            } else if let Some(&pseudo_opcode) = pseudo_lookup.get(upper.as_str()) {
                result.pseudo_opcodes.insert(pseudo_opcode);
            } else {
                return Err(eyre!("unknown opcode or precompile: {name}"));
            }
        }
        Ok(result)
    }
}

fn is_dead_provider_account(account: &Account) -> bool {
    account.nonce == 0
        && account.balance.is_zero()
        && account.bytecode_hash.is_none_or(|hash| hash == KECCAK_EMPTY)
}

fn lookup_value_recipient_is_dead<SP>(state_provider: &SP, address: &Address) -> EyreResult<bool>
where
    SP: reth_provider::StateProvider,
{
    Ok(state_provider.basic_account(address)?.as_ref().is_none_or(is_dead_provider_account))
}

fn intrinsic_gas_entries<T: alloy_consensus::Transaction>(
    tx: &alloy_consensus::transaction::Recovered<T>,
    recipient_is_dead: bool,
    tx_succeeded: bool,
    metered: &MeteredOpcodes,
    spec: BaseSpecId,
) -> Vec<OpcodeGas> {
    if metered.pseudo_opcodes.is_empty() {
        return Vec::new();
    }

    let requested = |opcode: PseudoOpcode| metered.pseudo_opcodes.contains(&opcode);
    let mut entries = Vec::new();
    let gas_params = GasParams::new_spec(spec.into());

    // EIP-2028/EIP-7623: revm's active token schedule is the source of truth for
    // calldata pricing. In particular, do not hardcode the Istanbul non-zero
    // byte price because future schedules may change the multiplier.
    let zero_bytes = tx.input().iter().filter(|&&byte| byte == 0).count() as u64;
    let non_zero_bytes = tx.input().len() as u64 - zero_bytes;
    let zero_byte_cost = gas_params.tx_token_cost();
    let non_zero_byte_cost =
        zero_byte_cost.saturating_mul(gas_params.tx_token_non_zero_byte_multiplier());
    let calldata_zero_gas = zero_bytes.saturating_mul(zero_byte_cost);
    let calldata_non_zero_gas = non_zero_bytes.saturating_mul(non_zero_byte_cost);

    let is_create = tx.to().is_none();
    // Pre-Amsterdam Base execution uses this legacy aggregate. The exact
    // EIP-2780 primitive names are registered above but are deliberately not
    // emitted until a Base execution schedule implements that decomposition.
    let legacy_tx_base_gas = gas_params.tx_base_stipend();
    let legacy_create_gas = if is_create { gas_params.tx_create_cost() } else { 0 };
    let initcode_words = if is_create { tx.input().len().div_ceil(32) as u64 } else { 0 };
    let initcode_gas = if is_create { gas_params.tx_initcode_cost(tx.input().len()) } else { 0 };

    // EIP-2930: these are prepaid access-list entries, not EIP-7928 BAL
    // observations. The active schedule can update their per-entry prices.
    let access_list_addresses = tx.access_list().map_or(0, |access_list| access_list.len() as u64);
    let access_list_storage_keys = tx.access_list().map_or(0, |access_list| {
        access_list.iter().map(|item| item.storage_keys.len() as u64).sum()
    });
    let access_list_address_gas =
        access_list_addresses.saturating_mul(gas_params.tx_access_list_address_cost());
    let access_list_storage_key_gas =
        access_list_storage_keys.saturating_mul(gas_params.tx_access_list_storage_key_cost());

    // EIP-7702: `tx_eip7702_per_empty_account_cost` includes the active
    // regular/state portions, so it remains correct if the schedule changes.
    let authorization_count = tx.authorization_count().unwrap_or_default();
    let per_empty_account_cost = gas_params.tx_eip7702_per_empty_account_cost();
    let authorization_gas = authorization_count.saturating_mul(per_empty_account_cost);

    let eip2780 = spec.into_eth_spec().is_enabled_in(SpecId::AMSTERDAM).then(|| {
        revm::context_interface::cfg::gas_params::Eip2780TxInfo {
            value: tx.value(),
            // Self-transfer: a `Call` whose recipient is the sender itself.
            is_self_transfer: tx.kind().to() == Some(tx.signer_ref()),
        }
    });

    let initial_gas = gas_params.initial_tx_gas(
        tx.input(),
        is_create,
        access_list_addresses,
        access_list_storage_keys,
        authorization_count,
        eip2780,
    );
    let intrinsic_gas = initial_gas.initial_total_gas();
    let floor_gas = initial_gas.floor_gas();

    if requested(PseudoOpcode::IntrinsicTotal) {
        entries.push(OpcodeGas {
            contract_address: Address::ZERO,
            opcode: PseudoOpcode::IntrinsicTotal.as_str().to_string(),
            count: 1,
            gas_used: intrinsic_gas,
        });
    }
    if requested(PseudoOpcode::IntrinsicTxDataZeroByteCost) && zero_bytes > 0 {
        entries.push(OpcodeGas {
            contract_address: Address::ZERO,
            opcode: PseudoOpcode::IntrinsicTxDataZeroByteCost.as_str().to_string(),
            count: zero_bytes,
            gas_used: calldata_zero_gas,
        });
    }
    if requested(PseudoOpcode::IntrinsicTxDataNonZeroByteCost) && non_zero_bytes > 0 {
        entries.push(OpcodeGas {
            contract_address: Address::ZERO,
            opcode: PseudoOpcode::IntrinsicTxDataNonZeroByteCost.as_str().to_string(),
            count: non_zero_bytes,
            gas_used: calldata_non_zero_gas,
        });
    }
    if requested(PseudoOpcode::IntrinsicInitcodeWordCost) && initcode_words > 0 {
        entries.push(OpcodeGas {
            contract_address: Address::ZERO,
            opcode: PseudoOpcode::IntrinsicInitcodeWordCost.as_str().to_string(),
            count: initcode_words,
            gas_used: initcode_gas,
        });
    }
    if requested(PseudoOpcode::IntrinsicAccessListAddressCost) && access_list_addresses > 0 {
        entries.push(OpcodeGas {
            contract_address: Address::ZERO,
            opcode: PseudoOpcode::IntrinsicAccessListAddressCost.as_str().to_string(),
            count: access_list_addresses,
            gas_used: access_list_address_gas,
        });
    }
    if requested(PseudoOpcode::IntrinsicAccessListStorageKeyCost) && access_list_storage_keys > 0 {
        entries.push(OpcodeGas {
            contract_address: Address::ZERO,
            opcode: PseudoOpcode::IntrinsicAccessListStorageKeyCost.as_str().to_string(),
            count: access_list_storage_keys,
            gas_used: access_list_storage_key_gas,
        });
    }
    if requested(PseudoOpcode::IntrinsicLegacyTxBaseCost) {
        entries.push(OpcodeGas {
            contract_address: Address::ZERO,
            opcode: PseudoOpcode::IntrinsicLegacyTxBaseCost.as_str().to_string(),
            count: 1,
            gas_used: legacy_tx_base_gas,
        });
    }
    if requested(PseudoOpcode::IntrinsicLegacyCreateCost) && is_create {
        entries.push(OpcodeGas {
            contract_address: Address::ZERO,
            opcode: PseudoOpcode::IntrinsicLegacyCreateCost.as_str().to_string(),
            count: 1,
            gas_used: legacy_create_gas,
        });
    }
    if requested(PseudoOpcode::IntrinsicPerEmptyAccountCost) && authorization_count > 0 {
        entries.push(OpcodeGas {
            contract_address: Address::ZERO,
            opcode: PseudoOpcode::IntrinsicPerEmptyAccountCost.as_str().to_string(),
            count: authorization_count,
            gas_used: authorization_gas,
        });
    }
    if requested(PseudoOpcode::TxFloorGas) && floor_gas > 0 {
        entries.push(OpcodeGas {
            contract_address: Address::ZERO,
            opcode: PseudoOpcode::TxFloorGas.as_str().to_string(),
            count: 1,
            gas_used: floor_gas,
        });
    }

    if tx_succeeded
        && tx.value() > U256::ZERO
        && let Some(to) = tx.to()
    {
        let opcode = if to == tx.signer() {
            PseudoOpcode::TxEffectEthSelfTransfer
        } else if recipient_is_dead {
            PseudoOpcode::TxEffectEthTransferToNonexistentAccount
        } else {
            PseudoOpcode::TxEffectEthTransferToExistingAccount
        };
        if requested(opcode) {
            entries.push(OpcodeGas {
                contract_address: Address::ZERO,
                opcode: opcode.as_str().to_string(),
                count: 1,
                gas_used: 0,
            });
        }
    }

    entries
}

fn state_effect_entries(state: &EvmState, metered: &MeteredOpcodes) -> Vec<OpcodeGas> {
    if !metered.pseudo_opcodes.iter().any(|opcode| opcode.is_state_effect()) {
        return Vec::new();
    }

    let mut new_slots = 0u64;
    let mut changed_slots = 0u64;
    let mut cleared_slots = 0u64;
    let mut touched_accounts = 0u64;
    let mut changed_accounts = 0u64;
    for account in state.values() {
        if account.is_touched() {
            touched_accounts = touched_accounts.saturating_add(1);
        }
        if account.is_changed() {
            changed_accounts = changed_accounts.saturating_add(1);
        }
        for slot in account.storage.values() {
            if !slot.is_changed() {
                continue;
            }
            changed_slots = changed_slots.saturating_add(1);
            if slot.original_value().is_zero() {
                new_slots = new_slots.saturating_add(1);
            } else if slot.present_value().is_zero() {
                cleared_slots = cleared_slots.saturating_add(1);
            }
        }
    }

    let requested = |opcode: PseudoOpcode| metered.pseudo_opcodes.contains(&opcode);
    let mut entries = Vec::new();
    let mut push = |opcode: PseudoOpcode, count: u64| {
        if requested(opcode) && count > 0 {
            entries.push(OpcodeGas {
                contract_address: Address::ZERO,
                opcode: opcode.as_str().to_string(),
                count,
                gas_used: 0,
            });
        }
    };
    push(PseudoOpcode::StateNewStorageSlot, new_slots);
    push(PseudoOpcode::StateChangedStorageSlot, changed_slots);
    push(PseudoOpcode::StateClearedStorageSlot, cleared_slots);
    push(PseudoOpcode::StateTouchedAccount, touched_accounts);
    push(PseudoOpcode::StateChangedAccount, changed_accounts);
    entries
}

/// Inputs for [`meter_bundle`].
#[derive(Debug)]
pub struct MeterBundleInput<SP> {
    /// State provider used to read pre-execution account and storage state.
    pub state_provider: SP,
    /// Chain spec used to construct the EVM environment.
    pub chain_spec: Arc<BaseChainSpec>,
    /// The bundle to simulate.
    pub bundle: ParsedBundle,
    /// Header used as the parent block for simulation; the EVM env is derived from it.
    pub header: SealedHeader,
    /// L1 block info used to compute L1 data fees during simulation.
    pub l1_block_info: L1BlockInfo,
    /// Opcodes and precompiles to track gas usage for.
    pub metered_opcodes: Arc<MeteredOpcodes>,
}

/// Simulates and meters a bundle of transactions.
///
/// Executes transactions in sequence to measure gas usage and execution time.
/// When `metered_opcodes` is non-empty, a [`MeteringInspector`] is attached to the EVM
/// to collect per-opcode and precompile gas data. Only items in the filter set appear
/// in the output.
///
/// Returns [`MeterBundleOutput`] containing transaction results and aggregated metrics.
pub fn meter_bundle<SP>(input: MeterBundleInput<SP>) -> EyreResult<MeterBundleOutput>
where
    SP: reth_provider::StateProvider,
{
    let MeterBundleInput {
        state_provider,
        chain_spec,
        bundle,
        header,
        mut l1_block_info,
        metered_opcodes,
    } = input;
    let header = &header;
    let metered_opcodes = metered_opcodes.as_ref();
    // Get bundle hash
    let bundle_hash = bundle.bundle_hash();

    let meters_value_transfer_effects =
        metered_opcodes.pseudo_opcodes.iter().any(|opcode| opcode.is_eth_transfer_effect());
    let mut initial_value_recipient_is_dead: HashMap<Address, bool> = HashMap::default();
    if meters_value_transfer_effects {
        for tx in bundle.transactions() {
            if let Some(to) = tx.to()
                && tx.value() > U256::ZERO
                && !initial_value_recipient_is_dead.contains_key(&to)
            {
                let is_dead = lookup_value_recipient_is_dead(&state_provider, &to)?;
                initial_value_recipient_is_dead.insert(to, is_dead);
            }
        }
    }

    // Create state database
    let state_db = StateProviderDatabase::new(state_provider);
    let mut db = State::builder().with_database(state_db).with_bundle_update().build();

    // Override sender nonces to match their first transaction's nonce and collect
    // account info for pre-flight validation.
    let mut first_nonces: HashMap<Address, u64> = HashMap::default();
    for tx in bundle.transactions() {
        first_nonces.entry(tx.signer()).or_insert_with(|| tx.nonce());
    }

    let mut account_infos: HashMap<Address, Option<Account>> = HashMap::default();
    for (&addr, &nonce) in &first_nonces {
        let cache_account = db.load_cache_account(addr)?;
        if let Some(ref mut account) = cache_account.account {
            let max_nonce = account.info.nonce.saturating_add(MAX_NONCE_AHEAD);
            if nonce > max_nonce {
                return Err(eyre!(
                    "transaction nonce {} for {} exceeds max allowed (on-chain {} + {})",
                    nonce,
                    addr,
                    account.info.nonce,
                    MAX_NONCE_AHEAD,
                ));
            }
            account.info.nonce = nonce;

            account_infos.insert(
                addr,
                Some(Account {
                    nonce: account.info.nonce,
                    balance: account.info.balance,
                    bytecode_hash: (account.info.code_hash != KECCAK_EMPTY)
                        .then_some(account.info.code_hash),
                }),
            );
        } else {
            account_infos.insert(addr, None);
        }
    }

    // Set up next block attributes
    let timestamp = header.timestamp() + BLOCK_TIME;
    let attributes = BaseNextBlockEnvAttributes {
        timestamp,
        suggested_fee_recipient: header.beneficiary(),
        prev_randao: header.mix_hash().unwrap_or_else(B256::random),
        gas_limit: header.gas_limit(),
        parent_beacon_block_root: header.parent_beacon_block_root(),
        extra_data: header.extra_data().clone(),
    };

    // Execute transactions with a MeteringInspector to collect per-opcode and
    // precompile gas data. Precompile gas is always tracked; opcode gas is only
    // tracked for opcodes in the metered set.
    let mut results = Vec::new();
    let mut total_gas_used = 0u64;
    let mut total_gas_fees = U256::ZERO;

    let total_start = Instant::now();
    {
        let evm_config = BaseEvmConfig::base(chain_spec);
        let evm_env = evm_config.next_evm_env(header, &attributes)?;
        let spec = evm_env.cfg_env.spec;
        let metered_opcodes = Arc::new(metered_opcodes.clone().for_spec(spec));
        let inspector = MeteringInspector::new(Arc::clone(&metered_opcodes));
        let evm = evm_config.evm_with_env_and_inspector(&mut db, evm_env, inspector);
        let ctx = evm_config.context_for_next_block(header, attributes)?;
        let mut builder = evm_config.create_block_builder(evm, header, ctx);

        let block = &mut builder.evm_mut().block;
        block.basefee = block.basefee.min(MIN_BASEFEE);
        builder.apply_pre_execution_changes()?;

        // TX_EFFECT_ETH_* classifies top-level ETH transfers. Within a
        // bundle, only earlier successful top-level value transfers update this
        // liveness cache; internal CALL/CREATE effects from prior transactions
        // are intentionally not re-read here.
        let mut live_value_recipients: HashSet<Address> = HashSet::default();
        for tx in bundle.transactions() {
            let tx_start = Instant::now();
            let tx_hash = tx.tx_hash();
            let from = tx.signer();
            let to = tx.to();
            let value = tx.value();
            let gas_price = tx.max_fee_per_gas();
            let recipient_is_dead = if meters_value_transfer_effects
                && let Some(to) = to
                && value > U256::ZERO
            {
                if live_value_recipients.contains(&to) {
                    false
                } else {
                    initial_value_recipient_is_dead.get(&to).copied().unwrap_or(false)
                }
            } else {
                false
            };
            let account = account_infos
                .get(&from)
                .ok_or_else(|| eyre!("Account not found for address: {from}"))?
                .ok_or_else(|| eyre!("Account is none for tx: {tx_hash}"))?;

            validate_tx(account, tx, &mut l1_block_info, spec)
                .map_err(|e| eyre!("Transaction {tx_hash} validation failed: {e}"))?;

            let mut tx_succeeded = false;
            let mut state_effects = Vec::new();
            let gas_used = builder
                .execute_transaction_with_result_closure(tx.clone(), |result| {
                    let result_and_state = result.result();
                    tx_succeeded = result_and_state.result.is_success();
                    // Count net post-state even when the call reverts. Revm still
                    // commits gas payment, nonce, and coinbase balance; rolled-back
                    // storage writes are already absent from `EvmState`. Gating on
                    // success would undercount committed account effects that resource
                    // admission prices.
                    state_effects = state_effect_entries(&result_and_state.state, &metered_opcodes);
                })
                .map_err(|e| eyre!("Transaction {tx_hash} execution failed: {e}"))?
                .tx_gas_used();
            if tx_succeeded
                && let Some(to) = to
                && value > U256::ZERO
            {
                live_value_recipients.insert(to);
            }

            let gas_fees = U256::from(gas_used) * U256::from(gas_price);
            total_gas_used = total_gas_used.saturating_add(gas_used);
            total_gas_fees = total_gas_fees.saturating_add(gas_fees);

            // Extract per-transaction opcode and precompile gas, then reset for next tx.
            let inspector = builder.evm_mut().inspector_mut();
            let opcode_data = inspector.take_opcode_gas();
            let precompile_data = inspector.take_precompile_gas();

            let mut opcode_gas =
                intrinsic_gas_entries(tx, recipient_is_dead, tx_succeeded, &metered_opcodes, spec);
            opcode_gas.extend(state_effects);
            opcode_gas.extend(opcode_data.iter().filter(|(_, usage)| usage.count > 0).map(
                |(&(contract_address, opcode), usage)| OpcodeGas {
                    contract_address,
                    opcode: opcode.as_str().to_string(),
                    count: usage.count,
                    gas_used: usage.gas_used,
                },
            ));

            for (addr, usage) in &precompile_data {
                if let Some(name) = metered_opcodes.precompile_name(*addr)
                    && usage.count > 0
                {
                    opcode_gas.push(OpcodeGas {
                        contract_address: *addr,
                        opcode: name.to_string(),
                        count: usage.count,
                        gas_used: usage.gas_used,
                    });
                }
            }
            opcode_gas.sort_by(|a, b| {
                a.contract_address.cmp(&b.contract_address).then_with(|| a.opcode.cmp(&b.opcode))
            });

            results.push(TransactionResult {
                coinbase_diff: gas_fees,
                eth_sent_to_coinbase: U256::ZERO,
                from_address: from,
                gas_fees,
                gas_price: U256::from(gas_price),
                gas_used,
                to_address: to,
                tx_hash,
                value,
                execution_time_us: tx_start.elapsed().as_micros(),
                opcode_gas,
            });
        }
    }

    let total_time_us = total_start.elapsed().as_micros();

    Ok(MeterBundleOutput { results, total_gas_used, total_gas_fees, bundle_hash, total_time_us })
}

#[cfg(test)]
mod tests {
    use alloy_consensus::transaction::Recovered;
    use alloy_eips::Encodable2718;
    use alloy_primitives::{Address, Bytes, keccak256, utils::Unit};
    use alloy_sol_types::{SolCall, SolValue};
    use base_bundles::{Bundle, ParsedBundle};
    use base_common_consensus::BaseTransactionSigned;
    use base_common_precompiles::{
        ActivationFeature, IActivationRegistry, IB20, IB20Factory, IB20Stablecoin, IPolicyRegistry,
    };
    use base_execution_chainspec::BaseChainSpecBuilder;
    use base_node_runner::test_utils::TestHarness;
    use base_test_utils::{
        Account, ContractFactory, DEVNET_CHAIN_ID, SimpleStorage, build_test_genesis,
    };
    use eyre::Context;
    use reth_provider::StateProviderFactory;
    use reth_transaction_pool::test_utils::TransactionBuilder;
    use revm::state::{Account as RevmAccount, EvmStorageSlot, TransactionId};

    use super::*;

    fn create_parsed_bundle(txs: Vec<BaseTransactionSigned>) -> eyre::Result<ParsedBundle> {
        let txs: Vec<Bytes> = txs.iter().map(|tx| Bytes::from(tx.encoded_2718())).collect();

        let bundle = Bundle { txs };

        ParsedBundle::try_from(bundle).map_err(|e| eyre::eyre!(e))
    }

    fn create_call_tx(
        chain_id: u64,
        nonce: u64,
        to: Address,
        input: impl Into<Bytes>,
        gas_limit: u64,
    ) -> BaseTransactionSigned {
        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(chain_id)
            .nonce(nonce)
            .to(to)
            .gas_limit(gas_limit)
            .max_fee_per_gas(MIN_BASEFEE as u128)
            .max_priority_fee_per_gas(0)
            .input(input.into())
            .into_eip1559();

        BaseTransactionSigned::Eip1559(signed_tx.as_eip1559().expect("eip1559 transaction").clone())
    }

    fn assert_precompile_gas(
        output: &MeterBundleOutput,
        tx_index: usize,
        address: Address,
        opcode: &str,
    ) {
        let tx_result = output.results.get(tx_index).expect("transaction result should exist");
        let entry = tx_result
            .opcode_gas
            .iter()
            .find(|entry| entry.contract_address == address && entry.opcode == opcode)
            .unwrap_or_else(|| {
                panic!(
                    "tx {tx_index} should report {opcode} gas for {address}; got {:?}",
                    tx_result.opcode_gas
                )
            });

        assert!(entry.count > 0, "{opcode} count should be non-zero");
        assert!(entry.gas_used > 0, "{opcode} gas_used should be non-zero");
    }

    fn opcode_count(entries: &[OpcodeGas], name: &str) -> Option<u64> {
        entries.iter().find(|entry| entry.opcode == name).map(|entry| entry.count)
    }

    fn state_effect_names() -> [String; 5] {
        [
            "STATE_NEW_STORAGE_SLOT".to_string(),
            "STATE_CHANGED_STORAGE_SLOT".to_string(),
            "STATE_CLEARED_STORAGE_SLOT".to_string(),
            "STATE_TOUCHED_ACCOUNT".to_string(),
            "STATE_CHANGED_ACCOUNT".to_string(),
        ]
    }

    #[test]
    fn parse_accepts_state_effect_pseudo_opcodes() {
        let metered = MeteredOpcodes::parse(&state_effect_names()).unwrap();
        assert!(metered.pseudo_opcodes.contains(&PseudoOpcode::StateNewStorageSlot));
        assert!(metered.pseudo_opcodes.contains(&PseudoOpcode::StateChangedStorageSlot));
        assert!(metered.pseudo_opcodes.contains(&PseudoOpcode::StateClearedStorageSlot));
        assert!(metered.pseudo_opcodes.contains(&PseudoOpcode::StateTouchedAccount));
        assert!(metered.pseudo_opcodes.contains(&PseudoOpcode::StateChangedAccount));
    }

    #[test]
    fn state_effect_entries_count_net_original_to_present() {
        let mut account = RevmAccount::default();
        account.mark_touch();
        account.storage.insert(
            U256::from(1),
            EvmStorageSlot::new_changed(U256::ZERO, U256::from(7), TransactionId::ZERO),
        );
        account.storage.insert(
            U256::from(2),
            EvmStorageSlot::new_changed(U256::from(4), U256::ZERO, TransactionId::ZERO),
        );
        account
            .storage
            .insert(U256::from(3), EvmStorageSlot::new(U256::from(5), TransactionId::ZERO));
        let mut state = EvmState::default();
        state.insert(Address::ZERO, account);

        let metered = MeteredOpcodes::parse(&state_effect_names()).unwrap();
        let entries = state_effect_entries(&state, &metered);
        assert_eq!(opcode_count(&entries, "STATE_NEW_STORAGE_SLOT"), Some(1));
        assert_eq!(opcode_count(&entries, "STATE_CHANGED_STORAGE_SLOT"), Some(2));
        assert_eq!(opcode_count(&entries, "STATE_CLEARED_STORAGE_SLOT"), Some(1));
        assert_eq!(opcode_count(&entries, "STATE_TOUCHED_ACCOUNT"), Some(1));
        assert!(opcode_count(&entries, "STATE_CHANGED_ACCOUNT").is_none());
    }

    fn sstore_then_revert_initcode() -> Bytes {
        // Runtime: SSTORE(0, 42); REVERT(0, 0)
        let runtime = [0x60, 0x2a, 0x60, 0x00, 0x55, 0x60, 0x00, 0x60, 0x00, 0xfd];
        let runtime_len = runtime.len() as u8;
        let mut initcode = vec![
            0x60,
            runtime_len,
            0x60,
            0x0a,
            0x5f,
            0x39, // CODECOPY(0, 10, runtime_len)
            0x60,
            runtime_len,
            0x5f,
            0xf3, // RETURN(0, runtime_len)
        ];
        initcode.extend_from_slice(&runtime);
        Bytes::from(initcode)
    }

    fn value_call_contract_initcode(target: Address) -> Bytes {
        // Runtime:
        //   CALL(gas(), target, 1 wei, 0, 0, 0, 0)
        //   STOP
        let mut runtime = Vec::new();
        runtime.extend_from_slice(&[0x60, 0x00]); // out size
        runtime.extend_from_slice(&[0x60, 0x00]); // out offset
        runtime.extend_from_slice(&[0x60, 0x00]); // in size
        runtime.extend_from_slice(&[0x60, 0x00]); // in offset
        runtime.extend_from_slice(&[0x60, 0x01]); // value
        runtime.push(0x73); // PUSH20 target
        runtime.extend_from_slice(target.as_slice());
        runtime.push(0x5a); // GAS
        runtime.push(0xf1); // CALL
        runtime.push(0x00); // STOP

        assert!(runtime.len() <= u8::MAX as usize);
        let runtime_len = runtime.len() as u8;

        let mut initcode = Vec::new();
        initcode.extend_from_slice(&[
            0x60,
            runtime_len,
            0x60,
            0x0a,
            0x5f,
            0x39, // CODECOPY(0, 10, runtime_len)
            0x60,
            runtime_len,
            0x5f,
            0xf3, // RETURN(0, runtime_len)
        ]);
        initcode.extend_from_slice(&runtime);
        Bytes::from(initcode)
    }

    async fn deploy_value_call_contract(
        harness: &TestHarness,
        target: Address,
        nonce: u64,
    ) -> eyre::Result<Address> {
        let (deployment_tx, contract_address, _) =
            Account::Deployer.create_deployment_tx(value_call_contract_initcode(target), nonce)?;
        harness.build_block_from_transactions(vec![deployment_tx]).await?;
        Ok(contract_address)
    }

    #[tokio::test]
    async fn meter_bundle_empty_transactions() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let parsed_bundle = create_parsed_bundle(Vec::new())?;

        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header,
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(MeteredOpcodes::default()),
        })?;

        assert!(output.results.is_empty());
        assert_eq!(output.total_gas_used, 0);
        assert_eq!(output.total_gas_fees, U256::ZERO);
        // Even empty bundles have some EVM setup overhead
        assert!(output.total_time_us > 0);
        assert_eq!(output.bundle_hash, keccak256([]));

        Ok(())
    }

    #[tokio::test]
    async fn meter_bundle_single_transaction() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let to = Address::random();
        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(to)
            .value(1_000)
            .gas_limit(21_000)
            .max_fee_per_gas(10)
            .max_priority_fee_per_gas(1)
            .into_eip1559();

        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let tx_hash = tx.tx_hash();

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let parsed_bundle = create_parsed_bundle(vec![tx])?;

        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header,
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(MeteredOpcodes::default()),
        })?;

        assert_eq!(output.results.len(), 1);
        let result = &output.results[0];
        assert!(output.total_time_us > 0);

        assert_eq!(result.from_address, Account::Alice.address());
        assert_eq!(result.to_address, Some(to));
        assert_eq!(result.tx_hash, tx_hash);
        assert_eq!(result.gas_price, U256::from(10));
        assert_eq!(result.gas_used, 21_000);
        assert_eq!(result.coinbase_diff, (U256::from(21_000) * U256::from(10)),);

        assert_eq!(output.total_gas_used, 21_000);
        assert_eq!(output.total_gas_fees, U256::from(21_000) * U256::from(10));

        let mut concatenated = Vec::with_capacity(32);
        concatenated.extend_from_slice(tx_hash.as_slice());
        assert_eq!(output.bundle_hash, keccak256(concatenated));

        assert!(result.execution_time_us > 0, "execution_time_us should be greater than zero");

        Ok(())
    }

    #[tokio::test]
    async fn meter_bundle_reports_active_intrinsic_components_and_floor() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();
        let to = Address::random();
        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(to)
            .gas_limit(100_000)
            .max_fee_per_gas(MIN_BASEFEE as u128)
            .max_priority_fee_per_gas(0)
            .input(Bytes::from_static(&[0, 1]))
            .into_eip1559();
        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;
        let parsed_bundle = create_parsed_bundle(vec![tx])?;
        let metered = MeteredOpcodes::parse(&[
            "INTRINSIC_TOTAL".to_string(),
            "INTRINSIC_TX_DATA_ZERO_BYTE_COST".to_string(),
            "INTRINSIC_TX_DATA_NON_ZERO_BYTE_COST".to_string(),
            "INTRINSIC_LEGACY_TX_BASE_COST".to_string(),
            "TX_FLOOR_GAS".to_string(),
        ])?;

        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header,
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(metered),
        })?;

        let entries = &output.results[0].opcode_gas;
        let gas = |opcode: &str| {
            entries.iter().find(|entry| entry.opcode == opcode).map(|entry| entry.gas_used)
        };
        // Osaka inherits EIP-2028's 4/16 calldata prices and EIP-7623's
        // 21,000 + 10 * (zero + 4 * non-zero) floor candidate.
        assert_eq!(gas("INTRINSIC_LEGACY_TX_BASE_COST"), Some(21_000));
        assert_eq!(gas("INTRINSIC_TX_DATA_ZERO_BYTE_COST"), Some(4));
        assert_eq!(gas("INTRINSIC_TX_DATA_NON_ZERO_BYTE_COST"), Some(16));
        assert_eq!(gas("INTRINSIC_TOTAL"), Some(21_020));
        assert_eq!(gas("TX_FLOOR_GAS"), Some(21_050));
        assert_ne!(gas("INTRINSIC_TOTAL"), gas("TX_FLOOR_GAS"));

        Ok(())
    }

    #[tokio::test]
    async fn meter_bundle_storage_write_transaction() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;

        let (deployment_tx, contract_address, _deployment_hash) =
            Account::Deployer.create_deployment_tx(SimpleStorage::BYTECODE.clone(), 0)?;
        harness.build_block_from_transactions(vec![deployment_tx]).await?;

        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(contract_address)
            .gas_limit(100_000)
            .max_fee_per_gas(MIN_BASEFEE as u128)
            .max_priority_fee_per_gas(0)
            .input(SimpleStorage::setValueCall { v: U256::from(42) }.abi_encode())
            .into_eip1559();

        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let parsed_bundle = create_parsed_bundle(vec![tx])?;

        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header,
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(MeteredOpcodes::default()),
        })?;

        assert_eq!(output.results.len(), 1);
        assert!(output.total_time_us > 0);
        assert!(output.results[0].execution_time_us > 0);

        Ok(())
    }

    #[tokio::test]
    async fn meter_bundle_opcode_gas_for_storage_write() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;

        let (deployment_tx, contract_address, _deployment_hash) =
            Account::Deployer.create_deployment_tx(SimpleStorage::BYTECODE.clone(), 0)?;
        harness.build_block_from_transactions(vec![deployment_tx]).await?;

        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(contract_address)
            .gas_limit(100_000)
            .max_fee_per_gas(MIN_BASEFEE as u128)
            .max_priority_fee_per_gas(0)
            .input(SimpleStorage::setValueCall { v: U256::from(42) }.abi_encode())
            .into_eip1559();

        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let parsed_bundle = create_parsed_bundle(vec![tx])?;

        let metered = MeteredOpcodes::parse(&["SSTORE".to_string(), "SLOAD".to_string()]).unwrap();

        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header,
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(metered),
        })?;

        assert_eq!(output.results.len(), 1);
        let tx_opcodes = &output.results[0].opcode_gas;
        assert!(!tx_opcodes.is_empty(), "storage write should produce opcode gas data");

        let sstore = tx_opcodes.iter().find(|o| o.opcode == "SSTORE");
        assert!(sstore.is_some(), "SSTORE should appear in opcode gas results");
        let sstore = sstore.unwrap();
        assert_eq!(sstore.contract_address, contract_address);
        assert!(sstore.count > 0, "SSTORE count should be non-zero");
        assert!(sstore.gas_used > 0, "SSTORE gas_used should be non-zero");

        Ok(())
    }

    #[tokio::test]
    async fn meter_bundle_state_effects_for_storage_write_and_clear() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;

        let (deployment_tx, contract_address, _deployment_hash) =
            Account::Deployer.create_deployment_tx(SimpleStorage::BYTECODE.clone(), 0)?;
        harness.build_block_from_transactions(vec![deployment_tx]).await?;

        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();
        let write_tx = create_call_tx(
            harness.chain_id(),
            0,
            contract_address,
            SimpleStorage::setValueCall { v: U256::from(42) }.abi_encode(),
            100_000,
        );
        let clear_tx = create_call_tx(
            harness.chain_id(),
            1,
            contract_address,
            SimpleStorage::setValueCall { v: U256::ZERO }.abi_encode(),
            100_000,
        );

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;
        let metered = MeteredOpcodes::parse(&state_effect_names())?;
        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: create_parsed_bundle(vec![write_tx, clear_tx])?,
            header,
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(metered),
        })?;

        assert_eq!(output.results.len(), 2);
        let write = &output.results[0].opcode_gas;
        assert_eq!(opcode_count(write, "STATE_NEW_STORAGE_SLOT"), Some(1));
        assert_eq!(opcode_count(write, "STATE_CHANGED_STORAGE_SLOT"), Some(1));
        assert!(opcode_count(write, "STATE_CLEARED_STORAGE_SLOT").is_none());
        assert!(opcode_count(write, "STATE_TOUCHED_ACCOUNT").unwrap_or(0) >= 1);
        assert!(opcode_count(write, "STATE_CHANGED_ACCOUNT").unwrap_or(0) >= 1);

        let clear = &output.results[1].opcode_gas;
        assert!(opcode_count(clear, "STATE_NEW_STORAGE_SLOT").is_none());
        assert_eq!(opcode_count(clear, "STATE_CHANGED_STORAGE_SLOT"), Some(1));
        assert_eq!(opcode_count(clear, "STATE_CLEARED_STORAGE_SLOT"), Some(1));

        Ok(())
    }

    #[tokio::test]
    async fn meter_bundle_state_effects_omit_rolled_back_storage_on_revert() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;

        let (deployment_tx, contract_address, _) =
            Account::Deployer.create_deployment_tx(sstore_then_revert_initcode(), 0)?;
        harness.build_block_from_transactions(vec![deployment_tx]).await?;

        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();
        let revert_tx =
            create_call_tx(harness.chain_id(), 0, contract_address, Bytes::new(), 100_000);

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;
        let metered = MeteredOpcodes::parse(&state_effect_names())?;
        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: create_parsed_bundle(vec![revert_tx])?,
            header,
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(metered),
        })?;

        assert_eq!(output.results.len(), 1);
        let effects = &output.results[0].opcode_gas;
        assert!(opcode_count(effects, "STATE_NEW_STORAGE_SLOT").is_none());
        assert!(opcode_count(effects, "STATE_CHANGED_STORAGE_SLOT").is_none());
        assert!(opcode_count(effects, "STATE_CLEARED_STORAGE_SLOT").is_none());
        assert!(opcode_count(effects, "STATE_TOUCHED_ACCOUNT").unwrap_or(0) >= 1);
        assert!(opcode_count(effects, "STATE_CHANGED_ACCOUNT").unwrap_or(0) >= 1);

        Ok(())
    }

    #[tokio::test]
    async fn meter_bundle_opcode_gas_splits_by_contract() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;

        let (deployment_tx_1, contract_address_1, _) =
            Account::Deployer.create_deployment_tx(SimpleStorage::BYTECODE.clone(), 0)?;
        let (deployment_tx_2, contract_address_2, _) =
            Account::Deployer.create_deployment_tx(SimpleStorage::BYTECODE.clone(), 1)?;
        harness.build_block_from_transactions(vec![deployment_tx_1, deployment_tx_2]).await?;

        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let tx_1 = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(contract_address_1)
            .gas_limit(100_000)
            .max_fee_per_gas(MIN_BASEFEE as u128)
            .max_priority_fee_per_gas(0)
            .input(SimpleStorage::setValueCall { v: U256::from(1) }.abi_encode())
            .into_eip1559();
        let tx_1 =
            BaseTransactionSigned::Eip1559(tx_1.as_eip1559().expect("eip1559 transaction").clone());

        let tx_2 = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(1)
            .to(contract_address_2)
            .gas_limit(100_000)
            .max_fee_per_gas(MIN_BASEFEE as u128)
            .max_priority_fee_per_gas(0)
            .input(SimpleStorage::setValueCall { v: U256::from(2) }.abi_encode())
            .into_eip1559();
        let tx_2 =
            BaseTransactionSigned::Eip1559(tx_2.as_eip1559().expect("eip1559 transaction").clone());

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let parsed_bundle = create_parsed_bundle(vec![tx_1, tx_2])?;
        let metered = MeteredOpcodes::parse(&["SSTORE".to_string()]).unwrap();

        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header,
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(metered),
        })?;

        assert_eq!(output.results.len(), 2);
        let sstore_1 = output.results[0]
            .opcode_gas
            .iter()
            .find(|entry| entry.opcode == "SSTORE")
            .expect("first contract should report SSTORE gas");
        let sstore_2 = output.results[1]
            .opcode_gas
            .iter()
            .find(|entry| entry.opcode == "SSTORE")
            .expect("second contract should report SSTORE gas");

        assert_eq!(sstore_1.contract_address, contract_address_1);
        assert_eq!(sstore_2.contract_address, contract_address_2);
        assert_ne!(sstore_1.contract_address, sstore_2.contract_address);

        Ok(())
    }

    #[tokio::test]
    async fn meter_bundle_opcode_gas_for_create() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;

        let (factory_deployment_tx, factory_address, _) =
            Account::Deployer.create_deployment_tx(ContractFactory::BYTECODE.clone(), 0)?;
        harness.build_block_from_transactions(vec![factory_deployment_tx]).await?;

        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(factory_address)
            .gas_limit(1_000_000)
            .max_fee_per_gas(MIN_BASEFEE as u128)
            .max_priority_fee_per_gas(0)
            .input(
                ContractFactory::deployWithCreateCall { bytecode: SimpleStorage::BYTECODE.clone() }
                    .abi_encode(),
            )
            .into_eip1559();

        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let parsed_bundle = create_parsed_bundle(vec![tx])?;

        let metered = MeteredOpcodes::parse(&["CREATE".to_string()]).unwrap();

        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header,
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(metered),
        })?;

        assert_eq!(output.results.len(), 1);
        let create = output.results[0]
            .opcode_gas
            .iter()
            .find(|o| o.opcode == "CREATE")
            .expect("CREATE should appear in opcode gas results for a factory deployment");
        assert!(create.count > 0, "CREATE count should be non-zero");
        assert!(create.gas_used > 0, "CREATE gas_used should be non-zero");

        Ok(())
    }

    #[tokio::test]
    async fn meter_bundle_opcode_gas_subtracts_nested_call_gas() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;

        let (factory_deployment_tx, factory_address, _) =
            Account::Deployer.create_deployment_tx(ContractFactory::BYTECODE.clone(), 0)?;
        harness.build_block_from_transactions(vec![factory_deployment_tx]).await?;

        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(factory_address)
            .gas_limit(1_000_000)
            .max_fee_per_gas(MIN_BASEFEE as u128)
            .max_priority_fee_per_gas(0)
            .input(
                ContractFactory::deployAndCallCall {
                    bytecode: SimpleStorage::BYTECODE.clone(),
                    callData: SimpleStorage::setValueCall { v: U256::from(42) }.abi_encode().into(),
                }
                .abi_encode(),
            )
            .into_eip1559();

        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let parsed_bundle = create_parsed_bundle(vec![tx])?;
        let metered = MeteredOpcodes::parse(&["CALL".to_string(), "SSTORE".to_string()]).unwrap();

        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header,
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(metered),
        })?;

        assert_eq!(output.results.len(), 1);
        let tx_opcodes = &output.results[0].opcode_gas;
        let call = tx_opcodes
            .iter()
            .find(|entry| entry.opcode == "CALL" && entry.contract_address == factory_address)
            .expect("factory should report CALL gas");
        let sstore = tx_opcodes
            .iter()
            .find(|entry| entry.opcode == "SSTORE" && entry.contract_address != factory_address)
            .expect("callee should report SSTORE gas");

        assert_eq!(call.count, 1, "factory should execute one CALL into SimpleStorage");
        assert_eq!(sstore.count, 1, "callee should execute one SSTORE");
        assert!(call.gas_used > 0, "CALL gas_used should be non-zero");
        assert!(sstore.gas_used > 0, "SSTORE gas_used should be non-zero");
        assert!(
            call.gas_used < sstore.gas_used,
            "CALL gas should exclude nested callee gas: CALL={} SSTORE={}",
            call.gas_used,
            sstore.gas_used
        );

        Ok(())
    }

    #[tokio::test]
    async fn meter_bundle_opcode_gas_for_top_level_value_transfer() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;

        let existing_account = Address::random();
        let create_existing_account_tx = TransactionBuilder::default()
            .signer(Account::Bob.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(existing_account)
            .value(1)
            .gas_limit(21_000)
            .max_fee_per_gas(MIN_BASEFEE as u128)
            .max_priority_fee_per_gas(0)
            .into_eip1559();
        harness
            .build_block_from_transactions(vec![Bytes::from(
                BaseTransactionSigned::Eip1559(
                    create_existing_account_tx.as_eip1559().expect("eip1559 transaction").clone(),
                )
                .encoded_2718(),
            )])
            .await?;

        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();
        let new_account = Address::random();
        let transfers = [new_account, existing_account, Account::Alice.address()]
            .into_iter()
            .enumerate()
            .map(|(idx, to)| {
                let signed_tx = TransactionBuilder::default()
                    .signer(Account::Alice.signer_b256())
                    .chain_id(harness.chain_id())
                    .nonce(idx as u64)
                    .to(to)
                    .value(1)
                    .gas_limit(21_000)
                    .max_fee_per_gas(MIN_BASEFEE as u128)
                    .max_priority_fee_per_gas(0)
                    .into_eip1559();
                BaseTransactionSigned::Eip1559(
                    signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
                )
            })
            .collect();

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;
        let parsed_bundle = create_parsed_bundle(transfers)?;
        let metered = MeteredOpcodes::parse(&[
            "CALL".to_string(),
            "INTRINSIC_TOTAL".to_string(),
            "TX_EFFECT_ETH_TRANSFER_TO_NONEXISTENT_ACCOUNT".to_string(),
            "TX_EFFECT_ETH_TRANSFER_TO_EXISTING_ACCOUNT".to_string(),
            "TX_EFFECT_ETH_SELF_TRANSFER".to_string(),
        ])
        .unwrap();

        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header,
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(metered),
        })?;

        assert_eq!(output.results.len(), 3);
        for result in &output.results {
            assert_eq!(result.gas_used, 21_000);
            assert!(
                result
                    .opcode_gas
                    .iter()
                    .any(|entry| entry.opcode == "INTRINSIC_TOTAL" && entry.gas_used == 21_000),
                "top-level value transfers should report intrinsic gas"
            );
            assert!(
                result.opcode_gas.iter().all(|entry| entry.opcode != "CALL"),
                "top-level value transfers do not execute a CALL opcode"
            );
        }
        assert!(output.results[0].opcode_gas.iter().any(|entry| {
            entry.opcode == "TX_EFFECT_ETH_TRANSFER_TO_NONEXISTENT_ACCOUNT" && entry.count == 1
        }));
        assert!(output.results[1].opcode_gas.iter().any(|entry| {
            entry.opcode == "TX_EFFECT_ETH_TRANSFER_TO_EXISTING_ACCOUNT" && entry.count == 1
        }));
        assert!(
            output.results[2]
                .opcode_gas
                .iter()
                .any(|entry| { entry.opcode == "TX_EFFECT_ETH_SELF_TRANSFER" && entry.count == 1 })
        );

        Ok(())
    }

    #[tokio::test]
    async fn meter_bundle_call_value_distinguishes_new_and_existing_accounts() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;

        let existing_account = Address::random();
        let create_existing_account_tx = TransactionBuilder::default()
            .signer(Account::Bob.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(existing_account)
            .value(1)
            .gas_limit(21_000)
            .max_fee_per_gas(MIN_BASEFEE as u128)
            .max_priority_fee_per_gas(0)
            .into_eip1559();
        harness
            .build_block_from_transactions(vec![Bytes::from(
                BaseTransactionSigned::Eip1559(
                    create_existing_account_tx.as_eip1559().expect("eip1559 transaction").clone(),
                )
                .encoded_2718(),
            )])
            .await?;

        let new_account = Address::random();
        let call_new_contract = deploy_value_call_contract(&harness, new_account, 0).await?;
        let call_existing_contract =
            deploy_value_call_contract(&harness, existing_account, 1).await?;

        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();
        let calls = [call_new_contract, call_existing_contract]
            .into_iter()
            .enumerate()
            .map(|(idx, to)| {
                let signed_tx = TransactionBuilder::default()
                    .signer(Account::Alice.signer_b256())
                    .chain_id(harness.chain_id())
                    .nonce(idx as u64)
                    .to(to)
                    .value(1)
                    .gas_limit(100_000)
                    .max_fee_per_gas(MIN_BASEFEE as u128)
                    .max_priority_fee_per_gas(0)
                    .into_eip1559();
                BaseTransactionSigned::Eip1559(
                    signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
                )
            })
            .collect();

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;
        let parsed_bundle = create_parsed_bundle(calls)?;
        let metered = MeteredOpcodes::parse(&["CALL".to_string()]).unwrap();

        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header,
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(metered),
        })?;

        assert_eq!(output.results.len(), 2);
        let call_new = output.results[0]
            .opcode_gas
            .iter()
            .find(|entry| entry.opcode == "CALL")
            .expect("CALL to new account should be metered");
        let call_existing = output.results[1]
            .opcode_gas
            .iter()
            .find(|entry| entry.opcode == "CALL")
            .expect("CALL to existing account should be metered");

        assert_eq!(call_new.count, 1);
        assert_eq!(call_existing.count, 1);
        assert!(
            call_new.gas_used > call_existing.gas_used,
            "CALL with value to a new account should include the account-creation surcharge"
        );
        assert_eq!(
            call_new.gas_used - call_existing.gas_used,
            25_000,
            "new-account CALL value surcharge should be visible in opcode gas"
        );

        Ok(())
    }

    #[tokio::test]
    async fn meter_bundle_opcode_gas_empty_when_disabled() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let to = Address::random();
        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(to)
            .value(1_000)
            .gas_limit(21_000)
            .max_fee_per_gas(10)
            .max_priority_fee_per_gas(1)
            .into_eip1559();

        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let parsed_bundle = create_parsed_bundle(vec![tx])?;

        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header,
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(MeteredOpcodes::default()),
        })?;

        assert!(
            output.results[0].opcode_gas.is_empty(),
            "opcode gas should be empty when no metered opcodes are configured"
        );

        Ok(())
    }

    #[tokio::test]
    async fn meter_bundle_opcode_gas_filters_to_requested() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;

        let (deployment_tx, contract_address, _deployment_hash) =
            Account::Deployer.create_deployment_tx(SimpleStorage::BYTECODE.clone(), 0)?;
        harness.build_block_from_transactions(vec![deployment_tx]).await?;

        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(contract_address)
            .gas_limit(100_000)
            .max_fee_per_gas(MIN_BASEFEE as u128)
            .max_priority_fee_per_gas(0)
            .input(SimpleStorage::setValueCall { v: U256::from(42) }.abi_encode())
            .into_eip1559();

        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let parsed_bundle = create_parsed_bundle(vec![tx])?;

        // Only request SSTORE — other opcodes like PUSH, ADD, etc. should be filtered out.
        let metered = MeteredOpcodes::parse(&["SSTORE".to_string()]).unwrap();

        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header,
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(metered),
        })?;

        let tx_opcodes = &output.results[0].opcode_gas;
        for entry in tx_opcodes {
            assert_eq!(entry.opcode, "SSTORE", "only SSTORE should appear, found {}", entry.opcode);
        }
        assert!(!tx_opcodes.is_empty(), "SSTORE should appear in results");

        Ok(())
    }

    #[tokio::test]
    async fn meter_bundle_precompile_gas_for_beryl_calls() -> eyre::Result<()> {
        let chain_spec = Arc::new(
            BaseChainSpecBuilder::default()
                .chain(DEVNET_CHAIN_ID.into())
                .genesis(build_test_genesis())
                .activation_admin_address(Account::Alice.address())
                .beryl_activated()
                .build(),
        );
        let harness = TestHarness::builder().with_chain_spec(chain_spec).build().await?;
        let chain_id = harness.chain_id();
        let sender = Account::Alice.address();
        let asset_salt = B256::repeat_byte(0x11);
        let stablecoin_salt = B256::repeat_byte(0x22);
        let (asset_address, _) = B20Variant::Asset.compute_address(sender, asset_salt);
        let (stablecoin_address, _) =
            B20Variant::Stablecoin.compute_address(sender, stablecoin_salt);

        let asset_params = IB20Factory::B20AssetCreateParams {
            version: B20Variant::Asset.supported_version(),
            name: "Metered Asset".to_string(),
            symbol: "MTA".to_string(),
            initialAdmin: sender,
            decimals: 6,
        };
        let stablecoin_params = IB20Factory::B20StablecoinCreateParams {
            version: B20Variant::Stablecoin.supported_version(),
            name: "Metered Stablecoin".to_string(),
            symbol: "MTS".to_string(),
            initialAdmin: sender,
            currency: "USD".to_string(),
        };

        let txs = vec![
            create_call_tx(
                chain_id,
                0,
                ActivationRegistryStorage::ADDRESS,
                IActivationRegistry::activateCall { feature: ActivationFeature::B20Asset.id() }
                    .abi_encode(),
                100_000,
            ),
            create_call_tx(
                chain_id,
                1,
                ActivationRegistryStorage::ADDRESS,
                IActivationRegistry::activateCall {
                    feature: ActivationFeature::B20Stablecoin.id(),
                }
                .abi_encode(),
                100_000,
            ),
            create_call_tx(
                chain_id,
                2,
                B20FactoryStorage::ADDRESS,
                IB20Factory::createB20Call {
                    variant: IB20Factory::B20Variant::ASSET,
                    salt: asset_salt,
                    params: asset_params.abi_encode().into(),
                    initCalls: Vec::new(),
                }
                .abi_encode(),
                2_000_000,
            ),
            create_call_tx(
                chain_id,
                3,
                asset_address,
                IB20::totalSupplyCall {}.abi_encode(),
                200_000,
            ),
            create_call_tx(
                chain_id,
                4,
                B20FactoryStorage::ADDRESS,
                IB20Factory::createB20Call {
                    variant: IB20Factory::B20Variant::STABLECOIN,
                    salt: stablecoin_salt,
                    params: stablecoin_params.abi_encode().into(),
                    initCalls: Vec::new(),
                }
                .abi_encode(),
                2_000_000,
            ),
            create_call_tx(
                chain_id,
                5,
                stablecoin_address,
                IB20Stablecoin::currencyCall {}.abi_encode(),
                200_000,
            ),
            create_call_tx(
                chain_id,
                6,
                PolicyRegistryStorage::ADDRESS,
                IPolicyRegistry::policyExistsCall {
                    policyId: PolicyRegistryStorage::ALWAYS_ALLOW_ID,
                }
                .abi_encode(),
                200_000,
            ),
        ];

        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();
        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;
        let parsed_bundle = create_parsed_bundle(txs)?;

        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header,
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(MeteredOpcodes::default().with_all_precompiles()),
        })?;

        assert_eq!(output.results.len(), 7);
        assert_precompile_gas(
            &output,
            0,
            ActivationRegistryStorage::ADDRESS,
            BERYL_ACTIVATION_REGISTRY_PRECOMPILE,
        );
        assert_precompile_gas(
            &output,
            1,
            ActivationRegistryStorage::ADDRESS,
            BERYL_ACTIVATION_REGISTRY_PRECOMPILE,
        );
        assert_precompile_gas(&output, 2, B20FactoryStorage::ADDRESS, BERYL_B20_FACTORY_PRECOMPILE);
        assert_precompile_gas(&output, 3, asset_address, BERYL_B20_ASSET_PRECOMPILE);
        assert_precompile_gas(&output, 4, B20FactoryStorage::ADDRESS, BERYL_B20_FACTORY_PRECOMPILE);
        assert_precompile_gas(&output, 5, stablecoin_address, BERYL_B20_STABLECOIN_PRECOMPILE);
        assert_precompile_gas(
            &output,
            6,
            PolicyRegistryStorage::ADDRESS,
            BERYL_POLICY_REGISTRY_PRECOMPILE,
        );

        Ok(())
    }

    #[test]
    fn metered_opcodes_parse_rejects_unknown() {
        let result = MeteredOpcodes::parse(&["NOTAREALOPCODE".to_string()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("NOTAREALOPCODE"));
    }

    #[test]
    fn metered_opcodes_parse_case_insensitive() {
        let result = MeteredOpcodes::parse(&["sstore".to_string(), "Sload".to_string()]);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().opcodes.len(), 2);
    }

    #[test]
    fn metered_opcodes_parse_recognizes_precompiles() {
        let result = MeteredOpcodes::parse(&[
            "SSTORE".to_string(),
            "BLAKE2F".to_string(),
            "ECREC".to_string(),
        ]);
        assert!(result.is_ok());
        let metered = result.unwrap();
        assert_eq!(metered.opcodes.len(), 1);
        assert_eq!(metered.precompiles.len(), 2);
        assert!(metered.precompiles.values().any(|n| n == "BLAKE2F"));
        assert!(metered.precompiles.values().any(|n| n == "ECREC"));
    }

    #[test]
    fn metered_opcodes_parse_recognizes_intrinsic_pseudo_opcodes() {
        let result = MeteredOpcodes::parse(&[
            "INTRINSIC_TOTAL".to_string(),
            "intrinsic_tx_data_zero_byte_cost".to_string(),
            "intrinsic_tx_data_non_zero_byte_cost".to_string(),
            "intrinsic_access_list_address_cost".to_string(),
            "INTRINSIC_ACCESS_LIST_STORAGE_KEY_COST".to_string(),
            "intrinsic_initcode_word_cost".to_string(),
            "tx_floor_gas".to_string(),
            "intrinsic_legacy_tx_base_cost".to_string(),
            "intrinsic_legacy_create_cost".to_string(),
            "intrinsic_per_empty_account_cost".to_string(),
            "intrinsic_tx_base_cost".to_string(),
            "intrinsic_cold_account_access".to_string(),
            "intrinsic_tx_value_cost".to_string(),
            "intrinsic_transfer_log_cost".to_string(),
            "intrinsic_create_access".to_string(),
            "intrinsic_regular_per_auth_base_cost".to_string(),
            "tx_effect_eth_transfer_to_nonexistent_account".to_string(),
            "tx_effect_eth_transfer_to_existing_account".to_string(),
            "tx_effect_eth_self_transfer".to_string(),
        ])
        .unwrap();
        assert!(result.pseudo_opcodes.contains(&PseudoOpcode::IntrinsicTotal));
        assert!(result.pseudo_opcodes.contains(&PseudoOpcode::IntrinsicTxDataZeroByteCost));
        assert!(result.pseudo_opcodes.contains(&PseudoOpcode::IntrinsicTxDataNonZeroByteCost));
        assert!(result.pseudo_opcodes.contains(&PseudoOpcode::IntrinsicAccessListAddressCost));
        assert!(result.pseudo_opcodes.contains(&PseudoOpcode::IntrinsicAccessListStorageKeyCost));
        assert!(result.pseudo_opcodes.contains(&PseudoOpcode::IntrinsicInitcodeWordCost));
        assert!(result.pseudo_opcodes.contains(&PseudoOpcode::TxFloorGas));
        assert!(result.pseudo_opcodes.contains(&PseudoOpcode::IntrinsicLegacyTxBaseCost));
        assert!(result.pseudo_opcodes.contains(&PseudoOpcode::IntrinsicLegacyCreateCost));
        assert!(result.pseudo_opcodes.contains(&PseudoOpcode::IntrinsicPerEmptyAccountCost));
        assert!(result.pseudo_opcodes.contains(&PseudoOpcode::IntrinsicTxBaseCost));
        assert!(result.pseudo_opcodes.contains(&PseudoOpcode::IntrinsicColdAccountAccess));
        assert!(result.pseudo_opcodes.contains(&PseudoOpcode::IntrinsicTxValueCost));
        assert!(result.pseudo_opcodes.contains(&PseudoOpcode::IntrinsicTransferLogCost));
        assert!(result.pseudo_opcodes.contains(&PseudoOpcode::IntrinsicCreateAccess));
        assert!(result.pseudo_opcodes.contains(&PseudoOpcode::IntrinsicRegularPerAuthBaseCost));
        assert!(
            result.pseudo_opcodes.contains(&PseudoOpcode::TxEffectEthTransferToNonexistentAccount)
        );
        assert!(
            result.pseudo_opcodes.contains(&PseudoOpcode::TxEffectEthTransferToExistingAccount)
        );
        assert!(result.pseudo_opcodes.contains(&PseudoOpcode::TxEffectEthSelfTransfer));
    }

    #[test]
    fn old_intrinsic_names_are_not_compatibility_aliases() {
        for name in [
            "INTRINSIC_BASE",
            "INTRINSIC_CALLDATA_ZERO",
            "INTRINSIC_CALLDATA_NON_ZERO",
            "INTRINSIC_CREATE",
            "INTRINSIC_INITCODE_WORD",
            "INTRINSIC_ACCESS_LIST_ADDRESS",
            "INTRINSIC_ACCESS_LIST_STORAGE_KEY",
            "INTRINSIC_AUTHORIZATION",
            "TX_EFFECT_VALUE_TO_NEW_ACCOUNT",
            "TX_EFFECT_VALUE_TO_EXISTING_ACCOUNT",
        ] {
            assert!(MeteredOpcodes::parse(&[name.to_string()]).is_err(), "{name} must be rejected");
        }
    }

    #[test]
    fn osaka_schedule_does_not_emit_eip2780_primitives() {
        let signed_tx = create_call_tx(
            DEVNET_CHAIN_ID,
            0,
            Address::repeat_byte(0x11),
            Bytes::from_static(&[0, 1]),
            100_000,
        );
        let tx = Recovered::new_unchecked(signed_tx, Account::Alice.address());
        let metered = MeteredOpcodes::parse(&[
            "INTRINSIC_LEGACY_TX_BASE_COST".to_string(),
            "INTRINSIC_TX_BASE_COST".to_string(),
            "INTRINSIC_COLD_ACCOUNT_ACCESS".to_string(),
            "INTRINSIC_TX_VALUE_COST".to_string(),
            "INTRINSIC_TRANSFER_LOG_COST".to_string(),
            "INTRINSIC_CREATE_ACCESS".to_string(),
            "INTRINSIC_REGULAR_PER_AUTH_BASE_COST".to_string(),
        ])
        .unwrap();

        // Azul is Base's Osaka execution schedule. EIP-2780 is not active, so its
        // primitive names must not be populated with legacy gas values.
        let entries =
            intrinsic_gas_entries(&tx, false, true, &metered, BaseSpecId::new(BaseUpgrade::Azul));
        assert!(entries.iter().any(|entry| entry.opcode == "INTRINSIC_LEGACY_TX_BASE_COST"));
        for name in [
            "INTRINSIC_TX_BASE_COST",
            "INTRINSIC_COLD_ACCOUNT_ACCESS",
            "INTRINSIC_TX_VALUE_COST",
            "INTRINSIC_TRANSFER_LOG_COST",
            "INTRINSIC_CREATE_ACCESS",
            "INTRINSIC_REGULAR_PER_AUTH_BASE_COST",
        ] {
            assert!(!entries.iter().any(|entry| entry.opcode == name), "{name} must be gated");
        }
    }

    #[test]
    fn metered_opcodes_parse_recognizes_azul_additions() {
        // CLZ opcode (EIP-7939) and P256VERIFY precompile gas-cost change (EIP-7951)
        // are the new metering surfaces introduced by Azul.
        let result = MeteredOpcodes::parse(&["CLZ".to_string(), "P256VERIFY".to_string()]).unwrap();
        assert_eq!(result.opcodes.len(), 1, "CLZ should be recognized as an opcode");
        assert!(result.precompiles.values().any(|n| n == "P256VERIFY"));
    }

    #[test]
    fn metered_opcodes_parse_recognizes_beryl_precompiles() {
        let result = MeteredOpcodes::parse(&[
            "BERYL_B20_FACTORY".to_string(),
            "beryl_b20_asset".to_string(),
            "Beryl_B20_Stablecoin".to_string(),
        ])
        .unwrap();

        assert_eq!(
            result.precompile_name(B20FactoryStorage::ADDRESS),
            Some(BERYL_B20_FACTORY_PRECOMPILE)
        );
        assert!(result.beryl_b20_asset_precompiles);
        assert!(result.beryl_b20_stablecoin_precompiles);
        assert!(!result.is_empty());
    }

    #[test]
    fn metered_opcodes_with_all_precompiles_includes_beryl_precompiles() {
        let result = MeteredOpcodes::default().with_all_precompiles();
        let (asset, _) =
            B20Variant::Asset.compute_address(Address::repeat_byte(0x11), B256::repeat_byte(0x22));
        let (stablecoin, _) = B20Variant::Stablecoin
            .compute_address(Address::repeat_byte(0x33), B256::repeat_byte(0x44));
        let (unsupported, _) = B20Variant::compute_address_for_discriminant(
            Address::repeat_byte(0x55),
            2,
            B256::repeat_byte(0x66),
        );

        assert_eq!(
            result.precompile_name(B20FactoryStorage::ADDRESS),
            Some(BERYL_B20_FACTORY_PRECOMPILE)
        );
        assert_eq!(
            result.precompile_name(ActivationRegistryStorage::ADDRESS),
            Some(BERYL_ACTIVATION_REGISTRY_PRECOMPILE)
        );
        assert_eq!(
            result.precompile_name(PolicyRegistryStorage::ADDRESS),
            Some(BERYL_POLICY_REGISTRY_PRECOMPILE)
        );
        assert_eq!(result.precompile_name(asset), Some(BERYL_B20_ASSET_PRECOMPILE));
        assert_eq!(result.precompile_name(stablecoin), Some(BERYL_B20_STABLECOIN_PRECOMPILE));
        assert_eq!(result.precompile_name(unsupported), None);
    }

    #[test]
    fn metered_opcodes_for_spec_filters_beryl_before_activation() {
        let result = MeteredOpcodes::default()
            .with_all_precompiles()
            .for_spec(BaseSpecId::new(BaseUpgrade::Azul));
        let (asset, _) =
            B20Variant::Asset.compute_address(Address::repeat_byte(0x11), B256::repeat_byte(0x22));

        assert_eq!(result.precompile_name(B20FactoryStorage::ADDRESS), None);
        assert_eq!(result.precompile_name(ActivationRegistryStorage::ADDRESS), None);
        assert_eq!(result.precompile_name(PolicyRegistryStorage::ADDRESS), None);
        assert_eq!(result.precompile_name(asset), None);
        assert!(result.precompile_name(precompile_addr(0x01)).is_some());
    }

    #[test]
    fn metered_opcodes_for_spec_keeps_beryl_after_activation() {
        let result = MeteredOpcodes::default()
            .with_all_precompiles()
            .for_spec(BaseSpecId::new(BaseUpgrade::Beryl));
        let (asset, _) =
            B20Variant::Asset.compute_address(Address::repeat_byte(0x11), B256::repeat_byte(0x22));

        assert_eq!(
            result.precompile_name(B20FactoryStorage::ADDRESS),
            Some(BERYL_B20_FACTORY_PRECOMPILE)
        );
        assert_eq!(result.precompile_name(asset), Some(BERYL_B20_ASSET_PRECOMPILE));
    }

    #[tokio::test]
    async fn meter_bundle_requires_parent_beacon_block_root() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let parsed_bundle = create_parsed_bundle(Vec::new())?;

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        // Headers without a parent beacon block root fail EVM env construction (EIP-4788).
        let mut header_without_root = header.clone_header();
        header_without_root.parent_beacon_block_root = None;
        let sealed_without_root = SealedHeader::new(header_without_root, header.hash());

        let err = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle.clone(),
            header: sealed_without_root,
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(MeteredOpcodes::default()),
        })
        .expect_err("missing parent beacon block root should fail");
        assert!(
            err.to_string().to_lowercase().contains("parent beacon block root"),
            "expected missing parent beacon block root error, got {err:?}"
        );

        let state_provider2 = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let output = meter_bundle(MeterBundleInput {
            state_provider: state_provider2,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header,
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(MeteredOpcodes::default()),
        })?;

        assert!(output.total_time_us > 0);

        Ok(())
    }

    #[tokio::test]
    async fn meter_bundle_multiple_transactions() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let to_1 = Address::random();
        let to_2 = Address::random();

        // Create first transaction
        let signed_tx_1 = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(to_1)
            .value(1_000)
            .gas_limit(21_000)
            .max_fee_per_gas(10)
            .max_priority_fee_per_gas(1)
            .into_eip1559();

        let tx_1 = BaseTransactionSigned::Eip1559(
            signed_tx_1.as_eip1559().expect("eip1559 transaction").clone(),
        );

        // Create second transaction
        let signed_tx_2 = TransactionBuilder::default()
            .signer(Account::Bob.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(to_2)
            .value(2_000)
            .gas_limit(21_000)
            .max_fee_per_gas(15)
            .max_priority_fee_per_gas(2)
            .into_eip1559();

        let tx_2 = BaseTransactionSigned::Eip1559(
            signed_tx_2.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let tx_hash_1 = tx_1.tx_hash();
        let tx_hash_2 = tx_2.tx_hash();

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let parsed_bundle = create_parsed_bundle(vec![tx_1, tx_2])?;

        let output = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header,
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(MeteredOpcodes::default()),
        })?;

        assert_eq!(output.results.len(), 2);
        assert!(output.total_time_us > 0);

        // Check first transaction
        let result_1 = &output.results[0];
        assert_eq!(result_1.from_address, Account::Alice.address());
        assert_eq!(result_1.to_address, Some(to_1));
        assert_eq!(result_1.tx_hash, tx_hash_1);
        assert_eq!(result_1.gas_price, U256::from(10));
        assert_eq!(result_1.gas_used, 21_000);
        assert_eq!(result_1.coinbase_diff, (U256::from(21_000) * U256::from(10)),);

        // Check second transaction
        let result_2 = &output.results[1];
        assert_eq!(result_2.from_address, Account::Bob.address());
        assert_eq!(result_2.to_address, Some(to_2));
        assert_eq!(result_2.tx_hash, tx_hash_2);
        assert_eq!(result_2.gas_price, U256::from(15));
        assert_eq!(result_2.gas_used, 21_000);
        assert_eq!(result_2.coinbase_diff, U256::from(21_000) * U256::from(15),);

        // Check aggregated values
        assert_eq!(output.total_gas_used, 42_000);
        let expected_total_fees =
            U256::from(21_000) * U256::from(10) + U256::from(21_000) * U256::from(15);
        assert_eq!(output.total_gas_fees, expected_total_fees);

        // Check bundle hash includes both transactions
        let mut concatenated = Vec::with_capacity(64);
        concatenated.extend_from_slice(tx_hash_1.as_slice());
        concatenated.extend_from_slice(tx_hash_2.as_slice());
        assert_eq!(output.bundle_hash, keccak256(concatenated));

        assert!(result_1.execution_time_us > 0, "execution_time_us should be greater than zero");
        assert!(result_2.execution_time_us > 0, "execution_time_us should be greater than zero");

        Ok(())
    }

    /// Verifies that a nonce ahead of on-chain state succeeds via override.
    ///
    /// Canonical nonce is 0, but the transaction uses nonce=1. The nonce override
    /// sets the account nonce to match, so simulation succeeds.
    #[tokio::test]
    async fn meter_bundle_overrides_nonce_too_high() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let to = Address::random();
        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(1) // Ahead of canonical nonce (0)
            .to(to)
            .value(100)
            .gas_limit(21_000)
            .max_fee_per_gas(MIN_BASEFEE as u128)
            .max_priority_fee_per_gas(0)
            .into_eip1559();

        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );
        let parsed_bundle = create_parsed_bundle(vec![tx])?;

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let result = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header,
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(MeteredOpcodes::default()),
        });

        assert!(
            result.is_ok(),
            "Nonce ahead of on-chain state should succeed via override: {:?}",
            result.err()
        );

        let output = result.unwrap();
        assert_eq!(output.results.len(), 1);
        assert_eq!(output.total_gas_used, 21_000);

        Ok(())
    }

    /// Verifies that nonce overrides are rejected when too far ahead of on-chain state.
    #[tokio::test]
    async fn meter_bundle_err_nonce_too_far_ahead() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let to = Address::random();
        let nonce = MAX_NONCE_AHEAD + 1; // Just over the limit (on-chain nonce is 0)
        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(nonce)
            .to(to)
            .value(100)
            .gas_limit(21_000)
            .max_fee_per_gas(MIN_BASEFEE as u128)
            .max_priority_fee_per_gas(0)
            .into_eip1559();

        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let parsed_bundle = create_parsed_bundle(vec![tx])?;

        let result = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header,
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(MeteredOpcodes::default()),
        });

        assert!(result.is_err(), "Nonce exceeding MAX_NONCE_AHEAD should fail");
        assert!(
            result.unwrap_err().to_string().contains("exceeds max allowed"),
            "Expected max nonce error"
        );

        Ok(())
    }

    /// Verifies that the base fee is capped at `MIN_BASEFEE` for simulation.
    ///
    /// The test genesis produces a next-block base fee of ~980M wei. A transaction with
    /// `max_fee_per_gas` at the `MIN_BASEFEE` floor (5M wei) would normally be rejected,
    /// but `meter_bundle` caps the base fee so simulation succeeds.
    #[tokio::test]
    async fn meter_bundle_caps_basefee_at_minimum() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let to = Address::random();
        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(to)
            .value(1_000)
            .gas_limit(21_000)
            .max_fee_per_gas(MIN_BASEFEE as u128) // At the floor, below the ~980M on-chain base fee
            .max_priority_fee_per_gas(0)
            .into_eip1559();

        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let parsed_bundle = create_parsed_bundle(vec![tx])?;

        let result = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header,
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(MeteredOpcodes::default()),
        });

        assert!(
            result.is_ok(),
            "Transaction with max_fee_per_gas below base fee but at least MIN_BASEFEE should succeed: {:?}",
            result.err()
        );

        let output = result.unwrap();
        assert_eq!(output.results.len(), 1);
        assert_eq!(output.total_gas_used, 21_000);

        Ok(())
    }

    #[tokio::test]
    async fn meter_bundle_err_insufficient_funds() -> eyre::Result<()> {
        let harness = TestHarness::new().await?;
        let latest = harness.latest_block();
        let header = latest.sealed_header().clone();

        let to = Address::random();
        // TestHarness uses build_test_genesis() which gives accounts 1 million ETH.
        // Transaction cost = value + (gas_limit * max_fee_per_gas)
        // We set value to 2 million ETH which exceeds the 1 million ETH balance
        let value_eth = 2_000_000u128;
        let value_in_wei = value_eth.saturating_mul(Unit::ETHER.wei().to::<u128>());

        let signed_tx = TransactionBuilder::default()
            .signer(Account::Alice.signer_b256())
            .chain_id(harness.chain_id())
            .nonce(0)
            .to(to)
            .value(value_in_wei)
            .gas_limit(21_000)
            .max_fee_per_gas(10)
            .max_priority_fee_per_gas(1)
            .into_eip1559();

        let tx = BaseTransactionSigned::Eip1559(
            signed_tx.as_eip1559().expect("eip1559 transaction").clone(),
        );

        let state_provider = harness
            .blockchain_provider()
            .state_by_block_hash(latest.hash())
            .context("getting state provider")?;

        let parsed_bundle = create_parsed_bundle(vec![tx])?;

        let result = meter_bundle(MeterBundleInput {
            state_provider,
            chain_spec: harness.chain_spec(),
            bundle: parsed_bundle,
            header,
            l1_block_info: L1BlockInfo::default(),
            metered_opcodes: Arc::new(MeteredOpcodes::default()),
        });

        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("Insufficient funds"),
            "Expected insufficient funds error"
        );

        Ok(())
    }
}
