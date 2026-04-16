//! Base constants used in the Base EVM.
use core::sync::atomic::{AtomicU64, Ordering};

use revm::primitives::{Address, U256, address, uint};

/// The cost of a non-zero byte in the EVM.
pub const NON_ZERO_BYTE_COST: u64 = 16;

/// The two 4-byte Ecotone fee scalar values are packed into the same storage slot as the 8-byte sequence number.
/// Byte offset within the storage slot of the 4-byte baseFeeScalar attribute.
pub const BASE_FEE_SCALAR_OFFSET: usize = 16;
/// The two 4-byte Ecotone fee scalar values are packed into the same storage slot as the 8-byte sequence number.
/// Byte offset within the storage slot of the 4-byte blobBaseFeeScalar attribute.
pub const BLOB_BASE_FEE_SCALAR_OFFSET: usize = 20;

/// The Isthmus operator fee scalar values are similarly packed. Byte offset within
/// the storage slot of the 4-byte operatorFeeScalar attribute.
pub const OPERATOR_FEE_SCALAR_OFFSET: usize = 20;
/// The Isthmus operator fee scalar values are similarly packed. Byte offset within
/// the storage slot of the 8-byte operatorFeeConstant attribute.
pub const OPERATOR_FEE_CONSTANT_OFFSET: usize = 24;

/// The Jovian daFootprintGasScalar value is packed into a single storage slot. Byte offset within
/// the storage slot of the 16-byte daFootprintGasScalar attribute.
pub const DA_FOOTPRINT_GAS_SCALAR_OFFSET: usize = 18;

/// The fixed point decimal scaling factor associated with the operator fee scalar.
///
/// Allows users to use 6 decimal points of precision when specifying the `operator_fee_scalar`.
pub const OPERATOR_FEE_SCALAR_DECIMAL: u64 = 1_000_000;

/// The Jovian multiplier applied to the operator fee scalar component.
pub const OPERATOR_FEE_JOVIAN_MULTIPLIER: u64 = 100;

/// The L1 base fee slot.
pub const L1_BASE_FEE_SLOT: U256 = uint!(1_U256);
/// The L1 overhead slot.
pub const L1_OVERHEAD_SLOT: U256 = uint!(5_U256);
/// The L1 scalar slot.
pub const L1_SCALAR_SLOT: U256 = uint!(6_U256);

/// [`ECOTONE_L1_BLOB_BASE_FEE_SLOT`] was added in the Ecotone upgrade and stores the L1 blobBaseFee attribute.
pub const ECOTONE_L1_BLOB_BASE_FEE_SLOT: U256 = uint!(7_U256);

/// As of the ecotone upgrade, this storage slot stores the 32-bit basefeeScalar and blobBaseFeeScalar attributes at
/// offsets [`BASE_FEE_SCALAR_OFFSET`] and [`BLOB_BASE_FEE_SCALAR_OFFSET`] respectively.
pub const ECOTONE_L1_FEE_SCALARS_SLOT: U256 = uint!(3_U256);

/// This storage slot stores the 32-bit operatorFeeScalar and operatorFeeConstant attributes at
/// offsets [`OPERATOR_FEE_SCALAR_OFFSET`] and [`OPERATOR_FEE_CONSTANT_OFFSET`] respectively.
pub const OPERATOR_FEE_SCALARS_SLOT: U256 = uint!(8_U256);

/// As of the Jovian upgrade, this storage slot stores the 16-bit daFootprintGasScalar attribute at
/// offset [`DA_FOOTPRINT_GAS_SCALAR_OFFSET`].
pub const DA_FOOTPRINT_GAS_SCALAR_SLOT: U256 = uint!(8_U256);

/// An empty 64-bit set of scalar values.
pub const EMPTY_SCALARS: [u8; 8] = [0u8; 8];

/// The address of L1 fee recipient.
pub const L1_FEE_RECIPIENT: Address = address!("0x420000000000000000000000000000000000001A");

/// The address of the operator fee recipient.
pub const OPERATOR_FEE_RECIPIENT: Address = address!("0x420000000000000000000000000000000000001B");

/// The address of the base fee recipient.
pub const BASE_FEE_RECIPIENT: Address = address!("0x4200000000000000000000000000000000000019");

/// The address of the `L1Block` contract.
pub const L1_BLOCK_CONTRACT: Address = address!("0x4200000000000000000000000000000000000015");

// ---------------------------------------------------------------------------
// EIP-8130 owner scope bitmask (mirrors base_alloy_consensus::OwnerScope)
// ---------------------------------------------------------------------------

/// Owner scope bit: allowed to sign as the sender.
pub const OWNER_SCOPE_SENDER: u8 = 0x02;

/// Owner scope bit: allowed to sign as the payer.
pub const OWNER_SCOPE_PAYER: u8 = 0x04;

/// Owner scope bit: allowed to authorize config changes.
pub const OWNER_SCOPE_CONFIG: u8 = 0x08;

/// Maximum number of calls across all EIP-8130 phases.
///
/// Mirrors `base_alloy_consensus::MAX_CALLS_PER_TX` and is enforced again at
/// inclusion time as a defense-in-depth check.
pub const MAX_CALLS_PER_TX: usize = 100;

/// Maximum number of account-change units in one EIP-8130 transaction.
///
/// Mirrors `base_alloy_consensus::MAX_ACCOUNT_CHANGES_PER_TX`.
pub const MAX_ACCOUNT_CHANGES_PER_TX: usize = 10;

/// Maximum number of owner-change operations across all config changes.
///
/// Mirrors `base_alloy_consensus::MAX_CONFIG_OPS_PER_TX`.
pub const MAX_CONFIG_OPS_PER_TX: usize = 5;

/// Delegate verifier contract address (1-hop delegation).
///
/// Mirrors `base_alloy_consensus::DELEGATE_VERIFIER_ADDRESS`.
pub const DELEGATE_VERIFIER_ADDRESS: Address =
    address!("0x30A76831b27732087561372f6a1bef6Fc391d805");

/// Default cap for aggregate gas spent across custom verifier STATICCALLs.
pub const DEFAULT_CUSTOM_VERIFIER_GAS_CAP: u64 = 200_000;

/// Runtime-configurable cap for aggregate custom verifier STATICCALL gas.
static CUSTOM_VERIFIER_GAS_CAP: AtomicU64 = AtomicU64::new(DEFAULT_CUSTOM_VERIFIER_GAS_CAP);

/// Returns the configured aggregate custom verifier STATICCALL gas cap.
pub fn custom_verifier_gas_cap() -> u64 {
    CUSTOM_VERIFIER_GAS_CAP.load(Ordering::Relaxed)
}

/// Sets the aggregate custom verifier STATICCALL gas cap.
pub fn set_custom_verifier_gas_cap(gas_cap: u64) {
    CUSTOM_VERIFIER_GAS_CAP.store(gas_cap, Ordering::Relaxed);
}
