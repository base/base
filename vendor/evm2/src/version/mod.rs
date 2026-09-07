//! EVM version definitions.

use alloy_eips::{eip4844::MAX_BLOBS_PER_BLOCK_DENCUN, eip7825::MAX_TX_GAS_LIMIT_OSAKA};

use crate::{
    EvmConfig, EvmTypesHost, SpecId,
    constants::{
        BLOB_BASE_FEE_UPDATE_FRACTION_AMSTERDAM, BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN,
        BLOB_BASE_FEE_UPDATE_FRACTION_PRAGUE, MAX_CODE_SIZE, MAX_CODE_SIZE_AMSTERDAM,
        MAX_INITCODE_SIZE, MAX_INITCODE_SIZE_AMSTERDAM,
    },
    interpreter::{instructions as instr, op},
};

mod gas_params;
pub use gas_params::{GasId, GasParams};

mod features;
pub use features::EvmFeatures;

mod opcode_config;
pub use opcode_config::OpcodeConfig;

/// Runtime configuration data.
///
/// The name is a bit misleading: this is a catch-all runtime configuration object. It stores fork
/// configuration such as active EVM features, and also stores regular runtime configuration values
/// such as chain ID, memory limits, code size limits, gas caps, and gas parameters.
#[derive(Clone, Copy, Debug)]
pub struct Version {
    /// Dynamic gas parameter table.
    // Gas params are data on the active version so changes automatically affect every
    // instruction that reads them. Tracking instruction dependencies on opcode config is not
    // sustainable for custom forks.
    pub gas_params: GasParams,
    /// EVM feature set.
    pub features: EvmFeatures,
    /// Chain ID returned by the `CHAINID` opcode and used for transaction chain ID validation.
    pub chain_id: u64,
    /// Transaction gas limit cap.
    pub tx_gas_limit_cap: u64,
    /// Hard memory limit in bytes.
    pub memory_limit: u64,
    /// Maximum deployed contract bytecode size.
    pub max_code_size: usize,
    /// Maximum contract creation initcode size.
    pub max_initcode_size: usize,
    /// Maximum blobs allowed in a single blob transaction.
    pub max_blobs_per_tx: usize,
    /// Blob base fee update fraction.
    pub blob_base_fee_update_fraction: u64,

    #[doc(hidden)] // Not public API. Please use an existing constructor.
    pub _non_exhaustive: (),
}

impl Version {
    /// Creates the base EVM version for `spec_id`.
    #[inline]
    pub const fn new(spec_id: SpecId) -> Self {
        *Self::base(spec_id)
    }

    /// Returns the base EVM version for `spec_id`.
    #[inline]
    pub const fn base(spec_id: SpecId) -> &'static Self {
        &BASE_VERSIONS[spec_id as usize]
    }

    /// Returns `true` if the active feature set contains `feature`.
    #[inline]
    pub const fn feature(&self, feature: EvmFeatures) -> bool {
        self.features.contains(feature)
    }
}

const fn base_tx_gas_limit_cap(spec_id: SpecId) -> u64 {
    if spec_id.enables(SpecId::OSAKA) { MAX_TX_GAS_LIMIT_OSAKA } else { u64::MAX }
}

const fn base_max_code_size(spec_id: SpecId) -> usize {
    if spec_id.enables(SpecId::AMSTERDAM) { MAX_CODE_SIZE_AMSTERDAM } else { MAX_CODE_SIZE }
}

const fn base_max_initcode_size(spec_id: SpecId) -> usize {
    if spec_id.enables(SpecId::AMSTERDAM) { MAX_INITCODE_SIZE_AMSTERDAM } else { MAX_INITCODE_SIZE }
}

const fn base_max_blobs_per_tx(spec_id: SpecId) -> usize {
    // EIP-7594 Osaka tests keep the per-transaction blob cap at the Cancun cap, while
    // Prague allows nine blobs per transaction.
    if spec_id.enables(SpecId::PRAGUE) && !spec_id.enables(SpecId::OSAKA) {
        9
    } else {
        MAX_BLOBS_PER_BLOCK_DENCUN
    }
}

const fn base_blob_base_fee_update_fraction(spec_id: SpecId) -> u64 {
    if spec_id.enables(SpecId::AMSTERDAM) {
        BLOB_BASE_FEE_UPDATE_FRACTION_AMSTERDAM
    } else if spec_id.enables(SpecId::PRAGUE) {
        BLOB_BASE_FEE_UPDATE_FRACTION_PRAGUE
    } else {
        BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN
    }
}

const DEFAULT_MEMORY_LIMIT: u64 = (1 << 32) - 1;
const DEFAULT_CHAIN_ID: u64 = 1;

static BASE_VERSIONS: [Version; SpecId::COUNT] = {
    let mut versions = [const {
        Version {
            gas_params: GasParams::empty(),
            features: EvmFeatures::empty(),
            chain_id: DEFAULT_CHAIN_ID,
            tx_gas_limit_cap: u64::MAX,
            memory_limit: DEFAULT_MEMORY_LIMIT,
            max_code_size: MAX_CODE_SIZE,
            max_initcode_size: MAX_INITCODE_SIZE,
            max_blobs_per_tx: MAX_BLOBS_PER_BLOCK_DENCUN,
            blob_base_fee_update_fraction: BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN,
            _non_exhaustive: (),
        }
    }; SpecId::COUNT];
    let mut i = 0;
    while i < SpecId::COUNT {
        let spec_id = SpecId::try_from_u32(i as u32).unwrap();
        versions[i] = Version {
            gas_params: base_gas_params(spec_id),
            features: base_features(spec_id),
            chain_id: DEFAULT_CHAIN_ID,
            tx_gas_limit_cap: base_tx_gas_limit_cap(spec_id),
            memory_limit: DEFAULT_MEMORY_LIMIT,
            max_code_size: base_max_code_size(spec_id),
            max_initcode_size: base_max_initcode_size(spec_id),
            max_blobs_per_tx: base_max_blobs_per_tx(spec_id),
            blob_base_fee_update_fraction: base_blob_base_fee_update_fraction(spec_id),
            _non_exhaustive: (),
        };
        i += 1;
    }
    versions
};

macro_rules! apply_base_gas_params {
    ($gp:ident, features: [$($tokens:tt)*]) => {};
    ($gp:ident, ops: [$($tokens:tt)*]) => {};
    ($gp:ident, static_gas: [$($tokens:tt)*]) => {};
    ($gp:ident, dynamic_gas: [$($id:ident: $value:expr,)*]) => {
        $(
            $gp.set($id, $value);
        )*
    };
}

macro_rules! apply_base_features {
    ($features:ident, features: [$($feature:ident,)*]) => {
        $(
            $features.insert(EvmFeatures::$feature);
        )*
    };
    ($features:ident, ops: [$($tokens:tt)*]) => {};
    ($features:ident, static_gas: [$($tokens:tt)*]) => {};
    ($features:ident, dynamic_gas: [$($tokens:tt)*]) => {};
}

macro_rules! apply_opcode_config {
    ($v:ident, $ty:ident, features: [$($tokens:tt)*]) => {};
    ($v:ident, $ty:ident, ops: [$($name:ident: $cost:expr,)*]) => {
        $(
            $v.set_instruction::<op_instr!($ty, $name)>(op::$name, $cost as u16);
        )*
    };
    ($v:ident, $ty:ident, static_gas: [$($name:ident: $cost:expr,)*]) => {
        $(
            $v.set_static_gas(op::$name, $cost as u16);
        )*
    };
    ($v:ident, $ty:ident, dynamic_gas: [$($tokens:tt)*]) => {};
}

macro_rules! evm_versions {
    ($($spec:ident { $($section:ident: [$($tokens:tt)*],)* })*) => {
        /// Creates the base feature set for `spec_id`.
        const fn base_features(spec_id: SpecId) -> EvmFeatures {
            let mut features = EvmFeatures::empty();

            $(
                if spec_id.enables(SpecId::$spec) {
                    $(
                        apply_base_features!(features, $section: [$($tokens)*]);
                    )*
                }
            )*

            features
        }

        /// Creates the base dynamic gas parameters for `spec_id`.
        const fn base_gas_params(spec_id: SpecId) -> GasParams {
            use crate::interpreter::gas::*;
            use GasId::*;

            let mut gp = GasParams::empty();

            $(
                if spec_id.enables(SpecId::$spec) {
                    $(
                        apply_base_gas_params!(gp, $section: [$($tokens)*]);
                    )*
                }
            )*

            gp
        }

        const fn base_opcode_config<T: EvmTypesHost, Cfg: EvmConfig<T>>() -> OpcodeConfig<T> {
            use crate::interpreter::gas::*;

            let spec_id = Cfg::BASE_SPEC_ID;
            let mut v = OpcodeConfig::empty();

            $(
                if spec_id.enables(SpecId::$spec) {
                    $(
                        apply_opcode_config!(v, T, $section: [$($tokens)*]);
                    )*
                }
            )*

            v
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BaseEvmConfig, BaseEvmTypes, OpcodeConfig, interpreter::op};

    fn opcode_config(spec: SpecId) -> &'static OpcodeConfig<BaseEvmTypes> {
        crate::spec_to_generic!(spec, |BASE_SPEC_ID| {
            <BaseEvmConfig<BASE_SPEC_ID> as EvmConfig<BaseEvmTypes>>::OPCODE_CONFIG
        })
    }

    #[test]
    fn base_versions_set_cfg_env_defaults() {
        let osaka = Version::base(SpecId::OSAKA);
        assert!(osaka.feature(EvmFeatures::TX_CHAIN_ID_CHECK));
        assert!(osaka.feature(EvmFeatures::NONCE_CHECK));
        assert!(osaka.feature(EvmFeatures::BALANCE_CHECK));
        assert!(osaka.feature(EvmFeatures::BALANCE_TOP_UP));
        assert!(osaka.feature(EvmFeatures::BLOCK_GAS_LIMIT_CHECK));
        assert!(osaka.feature(EvmFeatures::CODE_SIZE_CHECK));
        assert!(osaka.feature(EvmFeatures::EIP2));
        assert!(osaka.feature(EvmFeatures::EIP150));
        assert!(osaka.feature(EvmFeatures::EIP161));
        assert!(osaka.feature(EvmFeatures::EIP2028));
        assert!(osaka.feature(EvmFeatures::EIP2200));
        assert!(osaka.feature(EvmFeatures::EIP2929));
        assert!(osaka.feature(EvmFeatures::EIP3529));
        assert!(osaka.feature(EvmFeatures::EIP3651));
        assert!(osaka.feature(EvmFeatures::EIP3860));
        assert!(osaka.feature(EvmFeatures::EIP4399));
        assert!(osaka.feature(EvmFeatures::EIP6780));
        assert!(osaka.feature(EvmFeatures::EIP3541));
        assert!(osaka.feature(EvmFeatures::EIP3607));
        assert!(osaka.feature(EvmFeatures::EIP7623));
        assert!(osaka.feature(EvmFeatures::EIP7702));
        assert!(osaka.feature(EvmFeatures::BASE_FEE_CHECK));
        assert!(osaka.feature(EvmFeatures::PRIORITY_FEE_CHECK));
        assert!(osaka.feature(EvmFeatures::FEE_CHARGE));
        assert!(!osaka.feature(EvmFeatures::EIP7708));
        assert!(!osaka.feature(EvmFeatures::EIP8246));
        assert!(!osaka.feature(EvmFeatures::EIP8037));
        assert_eq!(osaka.chain_id, DEFAULT_CHAIN_ID);
        assert_eq!(osaka.tx_gas_limit_cap, MAX_TX_GAS_LIMIT_OSAKA);
        assert_eq!(osaka.memory_limit, DEFAULT_MEMORY_LIMIT);
        assert_eq!(osaka.max_code_size, MAX_CODE_SIZE);
        assert_eq!(osaka.max_initcode_size, MAX_INITCODE_SIZE);
        assert_eq!(osaka.max_blobs_per_tx, MAX_BLOBS_PER_BLOCK_DENCUN);
        assert_eq!(osaka.blob_base_fee_update_fraction, BLOB_BASE_FEE_UPDATE_FRACTION_PRAGUE);

        let amsterdam = Version::base(SpecId::AMSTERDAM);
        assert!(amsterdam.feature(EvmFeatures::TX_CHAIN_ID_CHECK));
        assert!(amsterdam.feature(EvmFeatures::EIP8037));
        assert!(amsterdam.feature(EvmFeatures::EIP7708));
        assert!(amsterdam.feature(EvmFeatures::EIP8246));
        assert_eq!(amsterdam.chain_id, DEFAULT_CHAIN_ID);
        assert_eq!(amsterdam.tx_gas_limit_cap, MAX_TX_GAS_LIMIT_OSAKA);
        assert_eq!(amsterdam.memory_limit, DEFAULT_MEMORY_LIMIT);
        assert_eq!(amsterdam.max_code_size, MAX_CODE_SIZE_AMSTERDAM);
        assert_eq!(amsterdam.max_initcode_size, MAX_INITCODE_SIZE_AMSTERDAM);
        assert_eq!(amsterdam.max_blobs_per_tx, MAX_BLOBS_PER_BLOCK_DENCUN);
        assert_eq!(
            amsterdam.blob_base_fee_update_fraction,
            BLOB_BASE_FEE_UPDATE_FRACTION_AMSTERDAM
        );

        let cancun = Version::base(SpecId::CANCUN);
        assert_eq!(cancun.max_blobs_per_tx, MAX_BLOBS_PER_BLOCK_DENCUN);
        assert_eq!(cancun.blob_base_fee_update_fraction, BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN);

        let prague = Version::base(SpecId::PRAGUE);
        assert_eq!(prague.max_blobs_per_tx, 9);
        assert_eq!(prague.blob_base_fee_update_fraction, BLOB_BASE_FEE_UPDATE_FRACTION_PRAGUE);
    }

    #[test]
    fn default_gas_table_static_costs() {
        let default_gas_table = opcode_config(SpecId::FRONTIER);
        assert_eq!(default_gas_table.static_gas(op::STOP), 0);
        assert_eq!(default_gas_table.static_gas(op::ADD), 3);
        assert_eq!(default_gas_table.static_gas(op::MUL), 5);
        assert_eq!(default_gas_table.static_gas(op::EXP), 10);
        assert_eq!(default_gas_table.static_gas(op::BALANCE), 20);
        assert_eq!(default_gas_table.static_gas(op::SLOAD), 50);
        assert_eq!(default_gas_table.static_gas(op::CALL), 40);
        assert_eq!(default_gas_table.static_gas(op::SELFDESTRUCT), 0);
    }

    #[test]
    fn gas_table_applies_spec_static_costs() {
        let tangerine = opcode_config(SpecId::TANGERINE);
        assert_eq!(tangerine.static_gas(op::SLOAD), 200);
        assert_eq!(tangerine.static_gas(op::BALANCE), 400);
        assert_eq!(tangerine.static_gas(op::SELFDESTRUCT), 5000);

        let istanbul = opcode_config(SpecId::ISTANBUL);
        assert_eq!(istanbul.static_gas(op::SLOAD), 800);
        assert_eq!(istanbul.static_gas(op::EXTCODEHASH), 700);

        let berlin = opcode_config(SpecId::BERLIN);
        assert_eq!(berlin.static_gas(op::SLOAD), 100);
        assert_eq!(berlin.static_gas(op::BALANCE), 100);
        assert_eq!(berlin.static_gas(op::CALL), 100);
    }

    #[test]
    fn amsterdam_eip8038_ext_family_second_read() {
        let amsterdam = opcode_config(SpecId::AMSTERDAM);
        // EIP-8038 §"EXT* family update": EXTCODESIZE / EXTCODECOPY make a second
        // database read, charged an extra WARM_ACCESS (100) on the static base.
        assert_eq!(amsterdam.static_gas(op::EXTCODESIZE), 200);
        assert_eq!(amsterdam.static_gas(op::EXTCODECOPY), 200);
        // WARM_ACCESS is unchanged by EIP-8038, so other access opcodes keep a
        // single warm base.
        assert_eq!(amsterdam.static_gas(op::EXTCODEHASH), 100);
        assert_eq!(amsterdam.static_gas(op::BALANCE), 100);
        assert_eq!(amsterdam.static_gas(op::SLOAD), 100);
        assert_eq!(amsterdam.static_gas(op::CALL), 100);
        // The surcharge is Amsterdam-only: pre-Amsterdam EXTCODESIZE == EXTCODEHASH.
        let prague = opcode_config(SpecId::PRAGUE);
        assert_eq!(prague.static_gas(op::EXTCODESIZE), prague.static_gas(op::EXTCODEHASH));
    }
}

/// EIP-8037 cost per state byte (CPSB) for Glamsterdam.
const AMSTERDAM_CPSB: u32 = 1530;

evm_versions! {
    FRONTIER {
        features: [
            TX_CHAIN_ID_CHECK,
            NONCE_CHECK,
            BALANCE_CHECK,
            BALANCE_TOP_UP,
            BLOCK_GAS_LIMIT_CHECK,
            EIP3607,
            PRIORITY_FEE_CHECK,
            FEE_CHARGE,
        ],
        ops: [
            STOP: ZERO,
            ADD: VERYLOW,
            MUL: LOW,
            SUB: VERYLOW,
            DIV: LOW,
            SDIV: LOW,
            MOD: LOW,
            SMOD: LOW,
            ADDMOD: MID,
            MULMOD: MID,
            EXP: EXP,
            SIGNEXTEND: LOW,
            LT: VERYLOW,
            GT: VERYLOW,
            SLT: VERYLOW,
            SGT: VERYLOW,
            EQ: VERYLOW,
            ISZERO: VERYLOW,
            AND: VERYLOW,
            OR: VERYLOW,
            XOR: VERYLOW,
            NOT: VERYLOW,
            BYTE: VERYLOW,
            KECCAK256: KECCAK256,
            ADDRESS: BASE,
            BALANCE: 20,
            ORIGIN: BASE,
            CALLER: BASE,
            CALLVALUE: BASE,
            CALLDATALOAD: VERYLOW,
            CALLDATASIZE: BASE,
            CALLDATACOPY: VERYLOW,
            CODESIZE: BASE,
            CODECOPY: VERYLOW,
            GASPRICE: BASE,
            EXTCODESIZE: 20,
            EXTCODECOPY: 20,
            BLOCKHASH: BLOCKHASH,
            COINBASE: BASE,
            TIMESTAMP: BASE,
            NUMBER: BASE,
            DIFFICULTY: BASE,
            GASLIMIT: BASE,
            POP: BASE,
            MLOAD: VERYLOW,
            MSTORE: VERYLOW,
            MSTORE8: VERYLOW,
            SLOAD: 50,
            SSTORE: ZERO,
            JUMP: MID,
            JUMPI: HIGH,
            PC: BASE,
            MSIZE: BASE,
            GAS: BASE,
            JUMPDEST: JUMPDEST,
            PUSH1: VERYLOW,
            PUSH2: VERYLOW,
            PUSH3: VERYLOW,
            PUSH4: VERYLOW,
            PUSH5: VERYLOW,
            PUSH6: VERYLOW,
            PUSH7: VERYLOW,
            PUSH8: VERYLOW,
            PUSH9: VERYLOW,
            PUSH10: VERYLOW,
            PUSH11: VERYLOW,
            PUSH12: VERYLOW,
            PUSH13: VERYLOW,
            PUSH14: VERYLOW,
            PUSH15: VERYLOW,
            PUSH16: VERYLOW,
            PUSH17: VERYLOW,
            PUSH18: VERYLOW,
            PUSH19: VERYLOW,
            PUSH20: VERYLOW,
            PUSH21: VERYLOW,
            PUSH22: VERYLOW,
            PUSH23: VERYLOW,
            PUSH24: VERYLOW,
            PUSH25: VERYLOW,
            PUSH26: VERYLOW,
            PUSH27: VERYLOW,
            PUSH28: VERYLOW,
            PUSH29: VERYLOW,
            PUSH30: VERYLOW,
            PUSH31: VERYLOW,
            PUSH32: VERYLOW,
            DUP1: VERYLOW,
            DUP2: VERYLOW,
            DUP3: VERYLOW,
            DUP4: VERYLOW,
            DUP5: VERYLOW,
            DUP6: VERYLOW,
            DUP7: VERYLOW,
            DUP8: VERYLOW,
            DUP9: VERYLOW,
            DUP10: VERYLOW,
            DUP11: VERYLOW,
            DUP12: VERYLOW,
            DUP13: VERYLOW,
            DUP14: VERYLOW,
            DUP15: VERYLOW,
            DUP16: VERYLOW,
            SWAP1: VERYLOW,
            SWAP2: VERYLOW,
            SWAP3: VERYLOW,
            SWAP4: VERYLOW,
            SWAP5: VERYLOW,
            SWAP6: VERYLOW,
            SWAP7: VERYLOW,
            SWAP8: VERYLOW,
            SWAP9: VERYLOW,
            SWAP10: VERYLOW,
            SWAP11: VERYLOW,
            SWAP12: VERYLOW,
            SWAP13: VERYLOW,
            SWAP14: VERYLOW,
            SWAP15: VERYLOW,
            SWAP16: VERYLOW,
            LOG0: LOG,
            LOG1: LOG,
            LOG2: LOG,
            LOG3: LOG,
            LOG4: LOG,
            CREATE: ZERO,
            CALL: 40,
            CALLCODE: 40,
            RETURN: ZERO,
            SELFDESTRUCT: ZERO,
        ],
        dynamic_gas: [
            ExpByte: 10,
            Logdata: LOGDATA,
            Logtopic: LOGTOPIC,
            CopyPerWord: COPY,
            ExtcodecopyPerWord: COPY,
            McopyPerWord: COPY,
            Keccak256PerWord: KECCAK256WORD,
            MemoryLinearCost: MEMORY,
            MemoryQuadraticReduction: 512,
            InitcodePerWord: INITCODE_WORD_COST,
            Create: CREATE,
            CallStipendReduction: 64,
            TransferValueCost: CALLVALUE,
            NewAccountCost: NEWACCOUNT,
            SstoreStatic: SSTORE_RESET,
            SstoreSetWithoutLoadCost: SSTORE_SET - SSTORE_RESET,
            SstoreSetRefund: SSTORE_SET - SSTORE_RESET,
            SstoreClearingSlotRefund: REFUND_SSTORE_CLEARS,
            SelfdestructRefund: SELFDESTRUCT_REFUND,
            CallStipend: CALL_STIPEND,
            MaxRefundQuotient: 2,
            CodeDepositCost: CODEDEPOSIT,
            TxTokenNonZeroByteMultiplier: NON_ZERO_BYTE_MULTIPLIER,
            TxTokenCost: STANDARD_TOKEN_COST,
            TxBaseStipend: 21000,
        ],
    }

    HOMESTEAD {
        features: [
            EIP2,
        ],
        ops: [
            DELEGATECALL: 40,
        ],
        dynamic_gas: [
            TxCreateCost: CREATE,
        ],
    }

    TANGERINE {
        features: [
            EIP150,
        ],
        static_gas: [
            SLOAD: 200,
            BALANCE: 400,
            EXTCODESIZE: 700,
            EXTCODECOPY: 700,
            CREATE: ZERO,
            CALL: 700,
            CALLCODE: 700,
            DELEGATECALL: 700,
            SELFDESTRUCT: 5000,
        ],
        dynamic_gas: [
            NewAccountCostForSelfdestruct: NEWACCOUNT,
        ],
    }

    SPURIOUS_DRAGON {
        features: [
            EIP161,
            CODE_SIZE_CHECK,
        ],
        static_gas: [
            EXP: EXP,
        ],
        dynamic_gas: [
            ExpByte: 50,
        ],
    }

    BYZANTIUM {
        ops: [
            RETURNDATASIZE: BASE,
            RETURNDATACOPY: VERYLOW,
            STATICCALL: 700,
            REVERT: ZERO,
        ],
    }

    PETERSBURG {
        ops: [
            SHL: VERYLOW,
            SHR: VERYLOW,
            SAR: VERYLOW,
            EXTCODEHASH: 400,
            CREATE2: ZERO,
        ],
    }

    ISTANBUL {
        features: [
            EIP2028,
            EIP2200,
        ],
        ops: [
            CHAINID: BASE,
            SELFBALANCE: LOW,
        ],
        static_gas: [
            SLOAD: ISTANBUL_SLOAD_GAS,
            BALANCE: 700,
            EXTCODEHASH: 700,
            SSTORE: ZERO,
        ],
        dynamic_gas: [
            SstoreStatic: ISTANBUL_SLOAD_GAS,
            SstoreSetWithoutLoadCost: SSTORE_SET - ISTANBUL_SLOAD_GAS,
            SstoreResetWithoutColdLoadCost: SSTORE_RESET - ISTANBUL_SLOAD_GAS,
            SstoreSetRefund: SSTORE_SET - ISTANBUL_SLOAD_GAS,
            SstoreResetRefund: SSTORE_RESET - ISTANBUL_SLOAD_GAS,
            TxTokenNonZeroByteMultiplier: NON_ZERO_BYTE_MULTIPLIER_ISTANBUL,
        ],
    }

    BERLIN {
        features: [
            EIP2929,
        ],
        static_gas: [
            SLOAD: WARM_STORAGE_READ_COST,
            BALANCE: WARM_STORAGE_READ_COST,
            EXTCODESIZE: WARM_STORAGE_READ_COST,
            EXTCODEHASH: WARM_STORAGE_READ_COST,
            EXTCODECOPY: WARM_STORAGE_READ_COST,
            SSTORE: ZERO,
            CALL: WARM_STORAGE_READ_COST,
            CALLCODE: WARM_STORAGE_READ_COST,
            DELEGATECALL: WARM_STORAGE_READ_COST,
            STATICCALL: WARM_STORAGE_READ_COST,
            SELFDESTRUCT: 5000,
        ],
        dynamic_gas: [
            SstoreStatic: WARM_STORAGE_READ_COST,
            ColdAccountAdditionalCost: COLD_ACCOUNT_ACCESS_COST_ADDITIONAL,
            ColdStorageAdditionalCost: COLD_SLOAD_COST - WARM_STORAGE_READ_COST,
            ColdStorageCost: COLD_SLOAD_COST,
            WarmStorageReadCost: WARM_STORAGE_READ_COST,
            SstoreResetWithoutColdLoadCost: WARM_SSTORE_RESET - WARM_STORAGE_READ_COST,
            SstoreSetWithoutLoadCost: SSTORE_SET - WARM_STORAGE_READ_COST,
            SstoreSetRefund: SSTORE_SET - WARM_STORAGE_READ_COST,
            SstoreResetRefund: WARM_SSTORE_RESET - WARM_STORAGE_READ_COST,
            TxAccessListAddressCost: EIP2930_ACCESS_LIST_ADDRESS,
            TxAccessListStorageKeyCost: EIP2930_ACCESS_LIST_STORAGE_KEY,
        ],
    }

    LONDON {
        features: [
            EIP3529,
            EIP3541,
            BASE_FEE_CHECK,
        ],
        ops: [
            BASEFEE: BASE,
        ],
        static_gas: [
            SSTORE: ZERO,
            SELFDESTRUCT: 5000,
        ],
        dynamic_gas: [
            SstoreClearingSlotRefund: WARM_SSTORE_RESET + EIP2930_ACCESS_LIST_STORAGE_KEY,
            SelfdestructRefund: 0,
            MaxRefundQuotient: 5,
        ],
    }

    MERGE {
        features: [
            EIP4399,
        ],
    }

    SHANGHAI {
        features: [
            EIP3651,
            EIP3860,
        ],
        ops: [
            PUSH0: BASE,
        ],
        static_gas: [
            CREATE: ZERO,
            CREATE2: ZERO,
        ],
        dynamic_gas: [
            TxInitcodeCost: INITCODE_WORD_COST,
        ],
    }

    CANCUN {
        features: [
            EIP6780,
        ],
        ops: [
            BLOBHASH: VERYLOW,
            BLOBBASEFEE: BASE,
            TLOAD: WARM_STORAGE_READ_COST,
            TSTORE: WARM_STORAGE_READ_COST,
            MCOPY: VERYLOW,
        ],
    }

    PRAGUE {
        features: [
            EIP7623,
            EIP7702,
        ],
        dynamic_gas: [
            TxEip7702PerEmptyAccountCost: EIP7702_PER_EMPTY_ACCOUNT_COST,
            TxEip7702AuthRefund: EIP7702_PER_EMPTY_ACCOUNT_COST - EIP7702_PER_AUTH_BASE_COST,
            TxFloorCostPerToken: TOTAL_COST_FLOOR_PER_TOKEN,
            TxFloorCostBase: 21000,
            // EIP-7623: zero calldata bytes count as one floor token.
            TxFloorZeroByteMultiplier: 1,
        ],
    }

    OSAKA {
        ops: [
            CLZ: LOW,
        ],
    }

    AMSTERDAM {
        features: [
            EIP8037,
            EIP7708,
            EIP8246,
            EIP2780,
        ],
        ops: [
            DUPN: VERYLOW,
            SWAPN: VERYLOW,
            EXCHANGE: VERYLOW,
            SLOTNUM: BASE,
        ],
        static_gas: [
            CREATE: ZERO,
            CREATE2: ZERO,
            SSTORE: ZERO,
            CALL: WARM_STORAGE_READ_COST,
            CALLCODE: WARM_STORAGE_READ_COST,
            DELEGATECALL: WARM_STORAGE_READ_COST,
            STATICCALL: WARM_STORAGE_READ_COST,
            // EIP-8038 §"EXT* family update": EXTCODESIZE and EXTCODECOPY perform
            // two database reads (load the account, then read its code), so their
            // per-access base is charged an extra warm access above the normal
            // account access. The warm access cost itself is unchanged by EIP-8038
            // (100 = `WARM_STORAGE_READ_COST`), so every other access opcode keeps
            // its Berlin warm base; only these two change. The dynamic cold premium
            // is still added by `load_account`.
            EXTCODESIZE: WARM_STORAGE_READ_COST + WARM_STORAGE_READ_COST,
            EXTCODECOPY: WARM_STORAGE_READ_COST + WARM_STORAGE_READ_COST,
            SELFDESTRUCT: 5000,
        ],
        dynamic_gas: [
            CodeDepositCost: 0,
            // EIP-8037 state-gas values: state bytes × CPSB (Glamsterdam).
            SstoreSetState: 64 * AMSTERDAM_CPSB,
            NewAccountState: 120 * AMSTERDAM_CPSB,
            CodeDepositState: AMSTERDAM_CPSB,
            CreateState: 120 * AMSTERDAM_CPSB,
            TxFloorCostPerToken: TOTAL_COST_FLOOR_PER_TOKEN_AMSTERDAM,
            // EIP-2780: the EIP-7623 calldata floor uses the reduced `TX_BASE`
            // (12,000) as its base instead of the legacy 21,000.
            TxFloorCostBase: EIP2780_TX_BASE_COST,
            // EIP-7976: the calldata floor charges every byte uniformly (64 gas
            // each), so zero bytes weigh the same as non-zero bytes in the
            // floor-tokens count.
            TxFloorZeroByteMultiplier: NON_ZERO_BYTE_MULTIPLIER_ISTANBUL,
            TxAccessListFloorByteMultiplier: EIP7981_ACCESS_LIST_FLOOR_BYTE_MULTIPLIER,
            // EIP-7702 under EIP-8037: per-authorization execution-gas refund. The
            // intrinsic per-auth execution charge bundles a worst-case
            // `ACCOUNT_WRITE`; when the authority leaf already exists (or the auth
            // EIP-2780 (ethereum/EIPs#11844): the per-auth `ACCOUNT_WRITE` and
            // state gas are charged conditionally at the runtime gas phase, per
            // authority that incurs them, so the pessimistic per-auth intrinsic
            // refund no longer applies. `TxEip7702PerAuthState` (per-bytecode,
            // AUTH_BASE_BYTES × CPSB) is the runtime net-new delegation-bytes
            // charge; the new-account state gas comes from `NewAccountState`.
            TxEip7702AuthRefund: 0,
            TxEip7702PerAuthState: 23 * AMSTERDAM_CPSB,

            // EIP-8038: State-access gas cost update (glamsterdam devnet-8
            // values). Constants in `interpreter::gas`.
            //   WARM_ACCESS                    100 ->    100  (unchanged)
            //   COLD_ACCOUNT_ACCESS          2,600 ->  3,000
            //   ACCOUNT_WRITE                6,700 ->  9,000
            //   COLD_STORAGE_ACCESS          2,100 ->  2,100  (unchanged)
            //   STORAGE_WRITE                2,800 -> 10,000
            //   STORAGE_CLEAR_REFUND         4,800 -> 11,616
            //   CREATE_ACCESS                7,000 -> 12,000  (ACCOUNT_WRITE + COLD_ACCOUNT_ACCESS)
            //   ACCESS_LIST_ADDRESS_COST     2,400 ->  2,900  (COLD_ACCOUNT_ACCESS - WARM_ACCESS)
            //   ACCESS_LIST_STORAGE_KEY_COST 1,900 ->  2,000  (COLD_STORAGE_ACCESS - WARM_ACCESS)
            //
            // WARM_ACCESS and the SSTORE warm base (`SstoreStatic`) are unchanged
            // (100), so they keep their inherited Berlin values.
            //
            // Account/storage access table values.
            ColdAccountAdditionalCost: EIP8038_COLD_ACCOUNT_ACCESS_ADDITIONAL,
            ColdStorageAdditionalCost: EIP8038_COLD_STORAGE_ACCESS_ADDITIONAL,
            // EIP-8038 folds the warm base into the cold cost: a cold SSTORE pays
            // `COLD_STORAGE_ACCESS` (2100) total, not warm(100)+cold. Since
            // `SstoreStatic` (warm, 100) is always charged, the cold add-on here
            // is the premium above warm (2000), unlike pre-8038 forks which add
            // the full `COLD_SLOAD_COST` on top of the warm base.
            ColdStorageCost: EIP8038_COLD_STORAGE_ACCESS_ADDITIONAL,
            // CALL_VALUE = ACCOUNT_WRITE + CALL_STIPEND. A value-bearing CALL already
            // pays the ACCOUNT_WRITE surcharge here, so creating the target charges
            // no extra execution gas — only the NEW_ACCOUNT state gas (hence
            // `NewAccountCost` is zero). SELFDESTRUCT has no such bundled charge, so
            // it still pays a separate ACCOUNT_WRITE when sending balance to an empty
            // account (execution-specs `selfdestruct`).
            TransferValueCost: EIP8038_CALL_VALUE,
            NewAccountCost: 0,
            NewAccountCostForSelfdestruct: EIP8038_ACCOUNT_WRITE,
            // SSTORE write surcharge / refunds = STORAGE_WRITE.
            SstoreSetWithoutLoadCost: EIP8038_STORAGE_WRITE,
            SstoreResetWithoutColdLoadCost: EIP8038_STORAGE_WRITE,
            // SSTORE 0→x→0 execution refund only; the state-gas portion is restored
            // directly to the reservoir via `sstore_state_gas_refill`.
            SstoreSetRefund: EIP8038_STORAGE_WRITE,
            SstoreResetRefund: EIP8038_STORAGE_WRITE,
            SstoreClearingSlotRefund: EIP8038_STORAGE_CLEAR_REFUND,
            // CREATE / CREATE2 execution-gas access cost (CREATE opcodes and create txns).
            Create: EIP8038_CREATE_ACCESS,
            TxCreateCost: EIP8038_CREATE_ACCESS,
            // EIP-7981: charge access-list data at 64 gas per byte (20 bytes per
            // address, 32 per storage key), baked into the per-item cost; EIP-8038
            // sets each per-item base to the cold-minus-warm premium (2,900 per
            // address, 2,000 per storage key).
            TxAccessListAddressCost: EIP8038_ACCESS_LIST_ADDRESS_COST + 20 * EIP7981_ACCESS_LIST_DATA_COST_PER_BYTE,
            TxAccessListStorageKeyCost: EIP8038_ACCESS_LIST_STORAGE_KEY_COST + 32 * EIP7981_ACCESS_LIST_DATA_COST_PER_BYTE,
            // EIP-2780 (ethereum/EIPs#11844): the intrinsic per-auth charge is
            // the state-independent `REGULAR_PER_AUTH_BASE_COST` (7,816) only;
            // the ACCOUNT_WRITE and state-gas remainder is charged at the runtime
            // gas phase per authority.
            TxEip7702PerEmptyAccountCost: EIP8038_EIP7702_PER_AUTH_BASE_REGULAR,

            // EIP-2780: Reduce intrinsic transaction gas. The `to`- and
            // `value`-based intrinsic charges. ACCOUNT_WRITE / CREATE_ACCESS
            // source from EIP-8038 so a single change propagates everywhere.
            TxValueCost: EIP2780_TX_VALUE_COST,
            TxCreateAccessCost: EIP8038_CREATE_ACCESS,
        ],
    }

}

macro_rules! op_instr {
    ($ty:ident, $name:ident) => {
        op_instr!(@path $ty, $name)
    };

    (@path $ty:ident, STOP) => { instr::stop<$ty> };
    (@path $ty:ident, ADD) => { instr::add<$ty> };
    (@path $ty:ident, MUL) => { instr::mul<$ty> };
    (@path $ty:ident, SUB) => { instr::sub<$ty> };
    (@path $ty:ident, DIV) => { instr::div<$ty> };
    (@path $ty:ident, SDIV) => { instr::sdiv<$ty> };
    (@path $ty:ident, MOD) => { instr::rem<$ty> };
    (@path $ty:ident, SMOD) => { instr::smod<$ty> };
    (@path $ty:ident, ADDMOD) => { instr::addmod<$ty> };
    (@path $ty:ident, MULMOD) => { instr::mulmod<$ty> };
    (@path $ty:ident, EXP) => { instr::exp<$ty> };
    (@path $ty:ident, SIGNEXTEND) => { instr::signextend<$ty> };
    (@path $ty:ident, LT) => { instr::lt<$ty> };
    (@path $ty:ident, GT) => { instr::gt<$ty> };
    (@path $ty:ident, SLT) => { instr::slt<$ty> };
    (@path $ty:ident, SGT) => { instr::sgt<$ty> };
    (@path $ty:ident, EQ) => { instr::eq<$ty> };
    (@path $ty:ident, ISZERO) => { instr::iszero<$ty> };
    (@path $ty:ident, AND) => { instr::bitand<$ty> };
    (@path $ty:ident, OR) => { instr::bitor<$ty> };
    (@path $ty:ident, XOR) => { instr::bitxor<$ty> };
    (@path $ty:ident, NOT) => { instr::not<$ty> };
    (@path $ty:ident, BYTE) => { instr::byte<$ty> };
    (@path $ty:ident, SHL) => { instr::shl<$ty> };
    (@path $ty:ident, SHR) => { instr::shr<$ty> };
    (@path $ty:ident, SAR) => { instr::sar<$ty> };
    (@path $ty:ident, CLZ) => { instr::clz<$ty> };
    (@path $ty:ident, KECCAK256) => { instr::keccak256<$ty> };
    (@path $ty:ident, ADDRESS) => { instr::address<$ty> };
    (@path $ty:ident, BALANCE) => { instr::balance<$ty> };
    (@path $ty:ident, ORIGIN) => { instr::origin<$ty> };
    (@path $ty:ident, CALLER) => { instr::caller<$ty> };
    (@path $ty:ident, CALLVALUE) => { instr::callvalue<$ty> };
    (@path $ty:ident, CALLDATALOAD) => { instr::calldataload<$ty> };
    (@path $ty:ident, CALLDATASIZE) => { instr::calldatasize<$ty> };
    (@path $ty:ident, CALLDATACOPY) => { instr::calldatacopy<$ty> };
    (@path $ty:ident, CODESIZE) => { instr::codesize<$ty> };
    (@path $ty:ident, CODECOPY) => { instr::codecopy<$ty> };
    (@path $ty:ident, GASPRICE) => { instr::gasprice<$ty> };
    (@path $ty:ident, EXTCODESIZE) => { instr::extcodesize<$ty> };
    (@path $ty:ident, EXTCODECOPY) => { instr::extcodecopy<$ty> };
    (@path $ty:ident, RETURNDATASIZE) => { instr::returndatasize<$ty> };
    (@path $ty:ident, RETURNDATACOPY) => { instr::returndatacopy<$ty> };
    (@path $ty:ident, EXTCODEHASH) => { instr::extcodehash<$ty> };
    (@path $ty:ident, BLOCKHASH) => { instr::blockhash<$ty> };
    (@path $ty:ident, COINBASE) => { instr::coinbase<$ty> };
    (@path $ty:ident, TIMESTAMP) => { instr::timestamp<$ty> };
    (@path $ty:ident, NUMBER) => { instr::block_number<$ty> };
    (@path $ty:ident, DIFFICULTY) => { instr::difficulty<$ty> };
    (@path $ty:ident, GASLIMIT) => { instr::gaslimit<$ty> };
    (@path $ty:ident, CHAINID) => { instr::chainid<$ty> };
    (@path $ty:ident, SELFBALANCE) => { instr::selfbalance<$ty> };
    (@path $ty:ident, BASEFEE) => { instr::basefee<$ty> };
    (@path $ty:ident, BLOBHASH) => { instr::blobhash<$ty> };
    (@path $ty:ident, BLOBBASEFEE) => { instr::blobbasefee<$ty> };
    (@path $ty:ident, SLOTNUM) => { instr::slotnum<$ty> };
    (@path $ty:ident, POP) => { instr::pop<$ty> };
    (@path $ty:ident, MLOAD) => { instr::mload<$ty> };
    (@path $ty:ident, MSTORE) => { instr::mstore<$ty> };
    (@path $ty:ident, MSTORE8) => { instr::mstore8<$ty> };
    (@path $ty:ident, SLOAD) => { instr::sload<$ty> };
    (@path $ty:ident, SSTORE) => { instr::sstore<$ty> };
    (@path $ty:ident, JUMP) => { instr::jump<$ty> };
    (@path $ty:ident, JUMPI) => { instr::jumpi<$ty> };
    (@path $ty:ident, PC) => { instr::pc<$ty> };
    (@path $ty:ident, MSIZE) => { instr::msize<$ty> };
    (@path $ty:ident, GAS) => { instr::gas<$ty> };
    (@path $ty:ident, JUMPDEST) => { instr::jumpdest<$ty> };
    (@path $ty:ident, TLOAD) => { instr::tload<$ty> };
    (@path $ty:ident, TSTORE) => { instr::tstore<$ty> };
    (@path $ty:ident, MCOPY) => { instr::mcopy<$ty> };
    (@path $ty:ident, PUSH0) => { instr::push<$ty, 0> };
    (@path $ty:ident, PUSH1) => { instr::push<$ty, 1> };
    (@path $ty:ident, PUSH2) => { instr::push<$ty, 2> };
    (@path $ty:ident, PUSH3) => { instr::push<$ty, 3> };
    (@path $ty:ident, PUSH4) => { instr::push<$ty, 4> };
    (@path $ty:ident, PUSH5) => { instr::push<$ty, 5> };
    (@path $ty:ident, PUSH6) => { instr::push<$ty, 6> };
    (@path $ty:ident, PUSH7) => { instr::push<$ty, 7> };
    (@path $ty:ident, PUSH8) => { instr::push<$ty, 8> };
    (@path $ty:ident, PUSH9) => { instr::push<$ty, 9> };
    (@path $ty:ident, PUSH10) => { instr::push<$ty, 10> };
    (@path $ty:ident, PUSH11) => { instr::push<$ty, 11> };
    (@path $ty:ident, PUSH12) => { instr::push<$ty, 12> };
    (@path $ty:ident, PUSH13) => { instr::push<$ty, 13> };
    (@path $ty:ident, PUSH14) => { instr::push<$ty, 14> };
    (@path $ty:ident, PUSH15) => { instr::push<$ty, 15> };
    (@path $ty:ident, PUSH16) => { instr::push<$ty, 16> };
    (@path $ty:ident, PUSH17) => { instr::push<$ty, 17> };
    (@path $ty:ident, PUSH18) => { instr::push<$ty, 18> };
    (@path $ty:ident, PUSH19) => { instr::push<$ty, 19> };
    (@path $ty:ident, PUSH20) => { instr::push<$ty, 20> };
    (@path $ty:ident, PUSH21) => { instr::push<$ty, 21> };
    (@path $ty:ident, PUSH22) => { instr::push<$ty, 22> };
    (@path $ty:ident, PUSH23) => { instr::push<$ty, 23> };
    (@path $ty:ident, PUSH24) => { instr::push<$ty, 24> };
    (@path $ty:ident, PUSH25) => { instr::push<$ty, 25> };
    (@path $ty:ident, PUSH26) => { instr::push<$ty, 26> };
    (@path $ty:ident, PUSH27) => { instr::push<$ty, 27> };
    (@path $ty:ident, PUSH28) => { instr::push<$ty, 28> };
    (@path $ty:ident, PUSH29) => { instr::push<$ty, 29> };
    (@path $ty:ident, PUSH30) => { instr::push<$ty, 30> };
    (@path $ty:ident, PUSH31) => { instr::push<$ty, 31> };
    (@path $ty:ident, PUSH32) => { instr::push<$ty, 32> };
    (@path $ty:ident, DUP1) => { instr::dup<$ty, 1> };
    (@path $ty:ident, DUP2) => { instr::dup<$ty, 2> };
    (@path $ty:ident, DUP3) => { instr::dup<$ty, 3> };
    (@path $ty:ident, DUP4) => { instr::dup<$ty, 4> };
    (@path $ty:ident, DUP5) => { instr::dup<$ty, 5> };
    (@path $ty:ident, DUP6) => { instr::dup<$ty, 6> };
    (@path $ty:ident, DUP7) => { instr::dup<$ty, 7> };
    (@path $ty:ident, DUP8) => { instr::dup<$ty, 8> };
    (@path $ty:ident, DUP9) => { instr::dup<$ty, 9> };
    (@path $ty:ident, DUP10) => { instr::dup<$ty, 10> };
    (@path $ty:ident, DUP11) => { instr::dup<$ty, 11> };
    (@path $ty:ident, DUP12) => { instr::dup<$ty, 12> };
    (@path $ty:ident, DUP13) => { instr::dup<$ty, 13> };
    (@path $ty:ident, DUP14) => { instr::dup<$ty, 14> };
    (@path $ty:ident, DUP15) => { instr::dup<$ty, 15> };
    (@path $ty:ident, DUP16) => { instr::dup<$ty, 16> };
    (@path $ty:ident, SWAP1) => { instr::swap<$ty, 1> };
    (@path $ty:ident, SWAP2) => { instr::swap<$ty, 2> };
    (@path $ty:ident, SWAP3) => { instr::swap<$ty, 3> };
    (@path $ty:ident, SWAP4) => { instr::swap<$ty, 4> };
    (@path $ty:ident, SWAP5) => { instr::swap<$ty, 5> };
    (@path $ty:ident, SWAP6) => { instr::swap<$ty, 6> };
    (@path $ty:ident, SWAP7) => { instr::swap<$ty, 7> };
    (@path $ty:ident, SWAP8) => { instr::swap<$ty, 8> };
    (@path $ty:ident, SWAP9) => { instr::swap<$ty, 9> };
    (@path $ty:ident, SWAP10) => { instr::swap<$ty, 10> };
    (@path $ty:ident, SWAP11) => { instr::swap<$ty, 11> };
    (@path $ty:ident, SWAP12) => { instr::swap<$ty, 12> };
    (@path $ty:ident, SWAP13) => { instr::swap<$ty, 13> };
    (@path $ty:ident, SWAP14) => { instr::swap<$ty, 14> };
    (@path $ty:ident, SWAP15) => { instr::swap<$ty, 15> };
    (@path $ty:ident, SWAP16) => { instr::swap<$ty, 16> };
    (@path $ty:ident, LOG0) => { instr::log<$ty, 0> };
    (@path $ty:ident, LOG1) => { instr::log<$ty, 1> };
    (@path $ty:ident, LOG2) => { instr::log<$ty, 2> };
    (@path $ty:ident, LOG3) => { instr::log<$ty, 3> };
    (@path $ty:ident, LOG4) => { instr::log<$ty, 4> };
    (@path $ty:ident, DUPN) => { instr::dupn<$ty> };
    (@path $ty:ident, SWAPN) => { instr::swapn<$ty> };
    (@path $ty:ident, EXCHANGE) => { instr::exchange<$ty> };
    (@path $ty:ident, CREATE) => { instr::create<$ty, false> };
    (@path $ty:ident, CALL) => { instr::call<$ty> };
    (@path $ty:ident, CALLCODE) => { instr::callcode<$ty> };
    (@path $ty:ident, RETURN) => { instr::r#return<$ty> };
    (@path $ty:ident, DELEGATECALL) => { instr::delegatecall<$ty> };
    (@path $ty:ident, CREATE2) => { instr::create<$ty, true> };
    (@path $ty:ident, STATICCALL) => { instr::staticcall<$ty> };
    (@path $ty:ident, REVERT) => { instr::revert<$ty> };
    (@path $ty:ident, INVALID) => { instr::invalid<$ty> };
    (@path $ty:ident, SELFDESTRUCT) => { instr::selfdestruct<$ty> };
}
use op_instr;
