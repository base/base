use revm_bytecode::opcode as op;
use revm_interpreter::instructions::gas_table_spec;
use revm_primitives::hardfork::SpecId;

/// Opcode information.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpcodeInfo(u32);

impl OpcodeInfo {
    /// The unknown flag.
    pub const UNKNOWN: u32 = 1 << 31;
    /// The dynamic flag.
    pub const DYNAMIC: u32 = 1 << 30;
    /// The disabled flag.
    pub const DISABLED: u32 = 1 << 29;
    /// The mask for the gas cost (16 bits, max 65535).
    pub const MASK: u32 = 0xFFFF;

    /// Creates a new gas info with the given gas cost.
    #[inline]
    pub const fn new(gas: u16) -> Self {
        Self(gas as u32)
    }

    /// Returns `true` if the opcode is unknown.
    #[inline]
    pub const fn is_unknown(self) -> bool {
        self.0 == Self::UNKNOWN
    }

    /// Returns `true` if the gas cost is dynamic.
    #[inline]
    pub const fn is_dynamic(self) -> bool {
        self.0 & Self::DYNAMIC != 0
    }

    /// Returns `true` if the opcode is known, but disabled in the current EVM
    /// version.
    #[inline]
    pub const fn is_disabled(self) -> bool {
        self.0 & Self::DISABLED != 0
    }

    /// Returns the base gas cost of the opcode.
    ///
    /// This may not be the final/full gas cost of the opcode as it may also have a dynamic cost.
    #[inline]
    pub const fn base_gas(self) -> u16 {
        (self.0 & Self::MASK) as u16
    }

    /// Sets the unknown flag.
    #[inline]
    pub const fn set_unknown(&mut self) {
        self.0 = Self::UNKNOWN;
    }

    /// Sets the dynamic flag.
    #[inline]
    pub const fn set_dynamic(&mut self) {
        self.0 |= Self::DYNAMIC;
    }

    /// Sets the disabled flag.
    #[inline]
    pub const fn set_disabled(&mut self) {
        self.0 |= Self::DISABLED;
    }

    /// Sets the gas cost.
    #[inline]
    pub const fn set_gas(&mut self, gas: u16) {
        self.0 = (self.0 & !Self::MASK) | (gas as u32);
    }
}

/// Returns the static info map for the given `SpecId`.
pub const fn op_info_map(spec_id: SpecId) -> &'static [OpcodeInfo; 256] {
    const SPEC_COUNT: usize = SpecId::AMSTERDAM as usize + 1;
    static MAPS: [[OpcodeInfo; 256]; SPEC_COUNT] = {
        let mut maps = [[OpcodeInfo(OpcodeInfo::UNKNOWN); 256]; SPEC_COUNT];
        let mut i = 0;
        while i < SPEC_COUNT {
            maps[i] = make_map(unsafe { core::mem::transmute::<u8, SpecId>(i as u8) });
            i += 1;
        }
        maps
    };
    &MAPS[spec_id as usize]
}

/// Opcodes with a dynamic gas component that also have a base (static) cost deducted upfront.
/// Only the dynamic part is paid in builtins.
const DYNAMIC_WITH_BASE_GAS: &[u8] = &[
    op::EXP,
    op::KECCAK256,
    op::BALANCE,
    op::CALLDATACOPY,
    op::CODECOPY,
    op::EXTCODESIZE,
    op::EXTCODECOPY,
    op::RETURNDATACOPY,
    op::EXTCODEHASH,
    op::MLOAD,
    op::MSTORE,
    op::MSTORE8,
    op::MCOPY,
    op::LOG0,
    op::LOG1,
    op::LOG2,
    op::LOG3,
    op::LOG4,
    op::SLOAD,
    op::CALL,
    op::CALLCODE,
    op::DELEGATECALL,
    op::STATICCALL,
    op::SELFDESTRUCT,
];

/// Opcodes whose gas cost is entirely dynamic — computed fully in builtins at runtime.
const FULLY_DYNAMIC: &[u8] = &[op::SSTORE, op::CREATE, op::CREATE2];

/// Opcodes that are gated behind a specific `SpecId`, paired with the spec they were introduced in.
const SPEC_GATED_OPCODES: &[(u8, SpecId)] = &[
    (op::SHL, SpecId::PETERSBURG),
    (op::SHR, SpecId::PETERSBURG),
    (op::SAR, SpecId::PETERSBURG),
    (op::CLZ, SpecId::OSAKA),
    (op::RETURNDATASIZE, SpecId::BYZANTIUM),
    (op::RETURNDATACOPY, SpecId::BYZANTIUM),
    (op::EXTCODEHASH, SpecId::PETERSBURG),
    (op::CHAINID, SpecId::ISTANBUL),
    (op::SELFBALANCE, SpecId::ISTANBUL),
    (op::BASEFEE, SpecId::LONDON),
    (op::BLOBHASH, SpecId::CANCUN),
    (op::BLOBBASEFEE, SpecId::CANCUN),
    (op::TLOAD, SpecId::CANCUN),
    (op::TSTORE, SpecId::CANCUN),
    (op::MCOPY, SpecId::CANCUN),
    (op::PUSH0, SpecId::SHANGHAI),
    (op::DELEGATECALL, SpecId::HOMESTEAD),
    (op::CREATE2, SpecId::PETERSBURG),
    (op::STATICCALL, SpecId::BYZANTIUM),
    (op::REVERT, SpecId::BYZANTIUM),
    (op::DUPN, SpecId::AMSTERDAM),
    (op::SWAPN, SpecId::AMSTERDAM),
    (op::EXCHANGE, SpecId::AMSTERDAM),
    (op::SLOTNUM, SpecId::AMSTERDAM),
];

/// Opcodes present in the upstream instruction table but not supported by revmc (e.g. EOF-only).
const UNSUPPORTED_OPCODES: &[u8] = &[];

const fn contains(haystack: &[u8], needle: u8) -> bool {
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i] == needle {
            return true;
        }
        i += 1;
    }
    false
}

const fn make_map(spec_id: SpecId) -> [OpcodeInfo; 256] {
    let table = gas_table_spec(spec_id);
    let mut map = [OpcodeInfo(OpcodeInfo::UNKNOWN); 256];

    let mut i = 0u16;
    while i < 256 {
        let op = i as u8;

        // Skip opcodes not defined in revm's opcode table.
        if revm_bytecode::opcode::OpCode::new(op).is_none() {
            i += 1;
            continue;
        }

        // Mark opcodes not supported by revmc (e.g. EOF-only) as disabled rather than
        // unknown, so they return `NotActivated` instead of `OpcodeNotFound` at runtime.
        if contains(UNSUPPORTED_OPCODES, op) {
            map[op as usize].set_disabled();
            i += 1;
            continue;
        }

        let is_fully_dynamic = contains(FULLY_DYNAMIC, op);
        let is_dynamic_with_base = contains(DYNAMIC_WITH_BASE_GAS, op);

        // Fully dynamic opcodes have their entire gas cost handled in builtins.
        let gas = if is_fully_dynamic {
            0u16
        } else {
            let static_gas = table[op as usize] as u64;
            assert!(
                static_gas <= OpcodeInfo::MASK as u64,
                "static gas exceeds OpcodeInfo capacity"
            );
            static_gas as u16
        };

        let mut info = OpcodeInfo::new(gas);

        if is_fully_dynamic || is_dynamic_with_base {
            info.set_dynamic();
        }

        map[op as usize] = info;
        i += 1;
    }

    // Apply spec-gating: mark opcodes as disabled if the current spec is before their introduction.
    let mut j = 0;
    while j < SPEC_GATED_OPCODES.len() {
        let (op, required_spec) = SPEC_GATED_OPCODES[j];
        if (spec_id as u8) < (required_spec as u8) {
            map[op as usize].set_disabled();
        }
        j += 1;
    }

    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clz_flags() {
        let london = op_info_map(SpecId::LONDON);
        let cancun = op_info_map(SpecId::CANCUN);
        let osaka = op_info_map(SpecId::OSAKA);

        let clz_london = london[op::CLZ as usize];
        let clz_cancun = cancun[op::CLZ as usize];
        let clz_osaka = osaka[op::CLZ as usize];

        eprintln!(
            "CLZ on LONDON: is_unknown={}, is_disabled={}, raw={:#06x}",
            clz_london.is_unknown(),
            clz_london.is_disabled(),
            clz_london.0
        );
        eprintln!(
            "CLZ on CANCUN: is_unknown={}, is_disabled={}, raw={:#06x}",
            clz_cancun.is_unknown(),
            clz_cancun.is_disabled(),
            clz_cancun.0
        );
        eprintln!(
            "CLZ on OSAKA: is_unknown={}, is_disabled={}, raw={:#06x}",
            clz_osaka.is_unknown(),
            clz_osaka.is_disabled(),
            clz_osaka.0
        );

        assert!(!clz_london.is_unknown(), "CLZ should not be unknown on pre-OSAKA");
        assert!(clz_london.is_disabled(), "CLZ should be disabled on LONDON");
        assert!(!clz_cancun.is_unknown(), "CLZ should not be unknown on pre-OSAKA");
        assert!(clz_cancun.is_disabled(), "CLZ should be disabled on CANCUN");
        assert!(!clz_osaka.is_unknown(), "CLZ should not be unknown on OSAKA");
        assert!(!clz_osaka.is_disabled(), "CLZ should not be disabled on OSAKA");
    }

    #[test]
    fn test_gas_values() {
        let cancun = op_info_map(SpecId::CANCUN);

        // Basic arithmetic.
        assert_eq!(cancun[op::ADD as usize].base_gas(), 3);
        assert_eq!(cancun[op::MUL as usize].base_gas(), 5);
        assert_eq!(cancun[op::ADDMOD as usize].base_gas(), 8);

        // EXP: base 10, dynamic.
        assert_eq!(cancun[op::EXP as usize].base_gas(), 10);
        assert!(cancun[op::EXP as usize].is_dynamic());

        // PUSH/DUP/SWAP gas.
        assert_eq!(cancun[op::PUSH1 as usize].base_gas(), 3);
        assert_eq!(cancun[op::DUP1 as usize].base_gas(), 3);
        assert_eq!(cancun[op::SWAP1 as usize].base_gas(), 3);
        assert_eq!(cancun[op::PUSH0 as usize].base_gas(), 2);

        // LOG: base gas only (topic + data cost charged dynamically in builtin).
        assert_eq!(cancun[op::LOG0 as usize].base_gas(), 375);
        assert_eq!(cancun[op::LOG1 as usize].base_gas(), 375);
        assert_eq!(cancun[op::LOG2 as usize].base_gas(), 375);
        assert_eq!(cancun[op::LOG3 as usize].base_gas(), 375);
        assert_eq!(cancun[op::LOG4 as usize].base_gas(), 375);
        assert!(cancun[op::LOG0 as usize].is_dynamic());

        // Memory ops: dynamic with base cost 3.
        assert_eq!(cancun[op::MLOAD as usize].base_gas(), 3);
        assert!(cancun[op::MLOAD as usize].is_dynamic());
        assert_eq!(cancun[op::KECCAK256 as usize].base_gas(), 30);
        assert!(cancun[op::KECCAK256 as usize].is_dynamic());

        // Transient storage (Cancun).
        assert_eq!(cancun[op::TLOAD as usize].base_gas(), 100);
        assert!(!cancun[op::TLOAD as usize].is_disabled());

        // Unknown opcode.
        assert!(cancun[0x0C].is_unknown());

        // AMSTERDAM-gated opcodes should be disabled on CANCUN.
        assert!(cancun[op::DUPN as usize].is_disabled());
        assert!(!cancun[op::DUPN as usize].is_unknown());
        assert!(cancun[op::SWAPN as usize].is_disabled());
        assert!(cancun[op::EXCHANGE as usize].is_disabled());
        assert!(cancun[op::SLOTNUM as usize].is_disabled());

        // AMSTERDAM-gated opcodes should be enabled on AMSTERDAM.
        let amsterdam = op_info_map(SpecId::AMSTERDAM);
        assert!(!amsterdam[op::DUPN as usize].is_disabled());
        assert!(!amsterdam[op::SWAPN as usize].is_disabled());
        assert!(!amsterdam[op::EXCHANGE as usize].is_disabled());
        assert!(!amsterdam[op::SLOTNUM as usize].is_disabled());

        // Spec-gated: PUSH0 disabled before Shanghai.
        let pre_shanghai = op_info_map(SpecId::MERGE);
        assert!(pre_shanghai[op::PUSH0 as usize].is_disabled());
        assert!(!cancun[op::PUSH0 as usize].is_disabled());

        // Dynamic-with-base-gas opcodes: base gas varies by spec.
        let frontier = op_info_map(SpecId::FRONTIER);
        assert_eq!(frontier[op::SLOAD as usize].base_gas(), 50);
        assert_eq!(frontier[op::SELFDESTRUCT as usize].base_gas(), 0);
        assert!(frontier[op::SELFDESTRUCT as usize].is_dynamic());
        assert_eq!(frontier[op::CALL as usize].base_gas(), 40);
        assert_eq!(frontier[op::BALANCE as usize].base_gas(), 20);
        assert_eq!(frontier[op::EXTCODESIZE as usize].base_gas(), 20);
        let tangerine = op_info_map(SpecId::TANGERINE);
        assert_eq!(tangerine[op::SLOAD as usize].base_gas(), 200);
        assert_eq!(tangerine[op::SELFDESTRUCT as usize].base_gas(), 5000);
        assert!(tangerine[op::SELFDESTRUCT as usize].is_dynamic());
        assert_eq!(tangerine[op::CALL as usize].base_gas(), 700);
        assert_eq!(tangerine[op::BALANCE as usize].base_gas(), 400);
        assert_eq!(tangerine[op::EXTCODESIZE as usize].base_gas(), 700);
        let berlin = op_info_map(SpecId::BERLIN);
        assert_eq!(berlin[op::SLOAD as usize].base_gas(), 100);
        assert_eq!(berlin[op::CALL as usize].base_gas(), 100);
        assert_eq!(berlin[op::BALANCE as usize].base_gas(), 100);
        assert_eq!(berlin[op::EXTCODESIZE as usize].base_gas(), 100);
    }
}
