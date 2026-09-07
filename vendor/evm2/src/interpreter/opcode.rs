//! Opcode byte constants and metadata.

use core::{fmt, ptr::NonNull};

macro_rules! opcodes {
    ($($val:literal => $name:ident => $instr:path => $($modifier:ident $(( $($modifier_arg:expr),* ))?),*;)*) => {
        /// Opcode byte constants.
        pub mod op {
            $(
                #[doc = concat!("Opcode byte for `", stringify!($name), "`.")]
                pub const $name: u8 = $val;
            )*
        }

        use op::*;

        impl OpCode {
            $(
                #[doc = concat!("Opcode metadata for `", stringify!($name), "`.")]
                pub const $name: Self = Self(op::$name);
            )*
        }

        /// Maps each opcode byte to metadata.
        pub static OPCODE_INFO: [Option<OpInfo>; 256] = {
            let mut map = [None; 256];
            let mut prev: u8 = 0;
            $(
                let val: u8 = $val;
                assert!(val == 0 || val > prev, "opcodes must be sorted in ascending order");
                prev = val;
                let info = OpInfo::new(stringify!($name));
                $(
                    let info = $modifier(info, $($($modifier_arg),*)?);
                )*
                map[op::$name as usize] = Some(info);
            )*
            let _ = prev;
            map
        };

        /// Maps each opcode name to metadata.
        #[cfg(feature = "parse")]
        pub(crate) static NAME_TO_OPCODE: phf::Map<&'static str, OpCode> =
            stringify_with_cb! { phf_map_cb; $($name)* };
    };
}

/// Callback for creating a [`phf`] map with `stringify_with_cb`.
#[cfg(feature = "parse")]
macro_rules! phf_map_cb {
    ($(#[doc = $s:literal] $id:ident)*) => {
        phf::phf_map! {
            $($s => OpCode::$id),*
        }
    };
}

/// Stringifies identifiers with `paste` so that they are available as literals.
///
/// This doesn't work with [`stringify!`] because it cannot be expanded inside another macro.
#[cfg(feature = "parse")]
macro_rules! stringify_with_cb {
    ($callback:ident; $($id:ident)*) => { paste::paste! {
        $callback! { $(#[doc = "" $id ""] $id)* }
    }};
}

/// Opcode metadata.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct OpCode(u8);

impl OpCode {
    /// Creates opcode metadata for a raw opcode byte.
    #[inline]
    pub const fn new(opcode: u8) -> Option<Self> {
        match OPCODE_INFO[opcode as usize] {
            Some(_) => Some(Self(opcode)),
            None => None,
        }
    }

    /// Creates opcode metadata for a raw opcode byte without validating it.
    #[inline]
    pub const fn new_or_unknown(opcode: u8) -> Self {
        Self(opcode)
    }

    /// Returns true if the opcode is a jump destination.
    #[inline]
    pub const fn is_jumpdest(&self) -> bool {
        self.0 == JUMPDEST
    }

    /// Returns true if the opcode is a legacy jump instruction.
    #[inline]
    pub const fn is_jump(self) -> bool {
        self.0 == JUMP
    }

    /// Returns true if the opcode is a `PUSH1..=PUSH32` instruction.
    #[inline]
    pub const fn is_push(self) -> bool {
        self.0 >= PUSH1 && self.0 <= PUSH32
    }

    /// Returns the raw opcode byte.
    #[inline]
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Returns whether this opcode is defined by evm2.
    #[inline]
    pub const fn is_valid(&self) -> bool {
        OPCODE_INFO[self.0 as usize].is_some()
    }

    /// Returns opcode stack and immediate metadata.
    #[inline]
    pub const fn info(&self) -> OpInfo {
        if let Some(info) = OPCODE_INFO[self.0 as usize] { info } else { OpInfo::unknown() }
    }

    /// Returns the opcode name.
    #[inline]
    pub const fn as_str(self) -> &'static str {
        self.info().name()
    }

    /// Returns the number of input stack elements.
    #[inline]
    pub const fn inputs(&self) -> u8 {
        self.info().inputs()
    }

    /// Returns the immediate byte count.
    #[inline]
    pub const fn immediate_size(self) -> u8 {
        self.info().immediate_size()
    }

    /// Returns the number of stack outputs.
    #[inline]
    pub const fn outputs(&self) -> u8 {
        self.info().outputs()
    }

    /// Calculates the difference between the number of input and output stack elements.
    #[inline]
    pub const fn io_diff(&self) -> i16 {
        self.info().io_diff()
    }

    /// Returns the opcode as a usize.
    #[inline]
    pub const fn as_usize(&self) -> usize {
        self.0 as usize
    }

    /// Returns the number of both input and output stack elements.
    pub const fn input_output(&self) -> (u8, u8) {
        let info = self.info();
        (info.inputs, info.outputs)
    }

    /// Returns whether the opcode can modify linear memory.
    #[inline]
    pub const fn modifies_memory(&self) -> bool {
        matches!(
            *self,
            Self::EXTCODECOPY
                | Self::MLOAD
                | Self::MSTORE
                | Self::MSTORE8
                | Self::MCOPY
                | Self::KECCAK256
                | Self::CODECOPY
                | Self::CALLDATACOPY
                | Self::RETURNDATACOPY
                | Self::CALL
                | Self::CALLCODE
                | Self::DELEGATECALL
                | Self::STATICCALL
                | Self::LOG0
                | Self::LOG1
                | Self::LOG2
                | Self::LOG3
                | Self::LOG4
                | Self::CREATE
                | Self::CREATE2
        )
    }
}

impl fmt::Display for OpCode {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.get();
        if let Some(val) = OPCODE_INFO[n as usize] {
            f.write_str(val.name())
        } else {
            write!(f, "UNKNOWN(0x{n:02X})")
        }
    }
}

impl PartialEq<u8> for OpCode {
    #[inline]
    fn eq(&self, other: &u8) -> bool {
        self.get().eq(other)
    }
}

/// Opcode stack and immediate metadata.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpInfo {
    /// Invariant: `(name_ptr, name_len)` is a [`&'static str`][str].
    ///
    /// It is a shorted variant of [`str`] as the name length is always less than 256 characters.
    name_ptr: NonNull<u8>,
    name_len: u8,
    /// Stack inputs.
    inputs: u8,
    /// Stack outputs.
    outputs: u8,
    /// Number of intermediate bytes.
    immediate_size: u8,
    /// If the opcode stops execution. aka STOP, RETURN, ..
    terminating: bool,
}

// SAFETY: The `NonNull` is just a `&'static str`.
unsafe impl Send for OpInfo {}
unsafe impl Sync for OpInfo {}

impl fmt::Debug for OpInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpInfo")
            .field("name", &self.name())
            .field("inputs", &self.inputs())
            .field("outputs", &self.outputs())
            .field("terminating", &self.is_terminating())
            .field("immediate_size", &self.immediate_size())
            .finish()
    }
}

impl OpInfo {
    /// Creates a new opcode info with the given name and default values.
    pub const fn new(name: &'static str) -> Self {
        assert!(name.len() < 256, "opcode name is too long");
        Self {
            name_ptr: unsafe { NonNull::new_unchecked(name.as_ptr().cast_mut()) },
            name_len: name.len() as u8,
            inputs: 0,
            outputs: 0,
            terminating: false,
            immediate_size: 0,
        }
    }

    const fn unknown() -> Self {
        terminating(Self::new("UNKNOWN"))
    }

    /// Returns the opcode name.
    #[inline]
    pub const fn name(&self) -> &'static str {
        // SAFETY: `self.name_*` can only be initialized with a valid `&'static str`.
        unsafe {
            let slice = core::slice::from_raw_parts(self.name_ptr.as_ptr(), self.name_len as usize);
            core::str::from_utf8_unchecked(slice)
        }
    }

    /// Calculates the difference between the number of input and output stack elements.
    #[inline]
    pub const fn io_diff(&self) -> i16 {
        self.outputs as i16 - self.inputs as i16
    }

    /// Returns the number of input stack elements.
    #[inline]
    pub const fn inputs(&self) -> u8 {
        self.inputs
    }

    /// Returns the number of output stack elements.
    #[inline]
    pub const fn outputs(&self) -> u8 {
        self.outputs
    }

    /// Returns whether this opcode terminates execution, e.g. `STOP`, `RETURN`, etc.
    #[inline]
    pub const fn is_terminating(&self) -> bool {
        self.terminating
    }

    /// Returns the immediate byte count.
    #[inline]
    pub const fn immediate_size(&self) -> u8 {
        self.immediate_size
    }
}

/// Used for [`OPCODE_INFO`] to set the immediate bytes number in the [`OpInfo`].
#[inline]
pub const fn immediate_size(mut op: OpInfo, n: u8) -> OpInfo {
    op.immediate_size = n;
    op
}

/// Used for [`OPCODE_INFO`] to set the terminating flag to true in the [`OpInfo`].
#[inline]
pub const fn terminating(mut op: OpInfo) -> OpInfo {
    op.terminating = true;
    op
}

/// Use for [`OPCODE_INFO`] to sets the number of stack inputs and outputs in the [`OpInfo`].
#[inline]
pub const fn stack_io(mut op: OpInfo, inputs: u8, outputs: u8) -> OpInfo {
    op.inputs = inputs;
    op.outputs = outputs;
    op
}

#[cfg(feature = "parse")]
mod parse {
    use core::fmt;

    use super::{NAME_TO_OPCODE, OpCode};

    /// An error indicating that an opcode is invalid.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct OpCodeError(());

    impl fmt::Display for OpCodeError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("invalid opcode")
        }
    }

    impl core::error::Error for OpCodeError {}

    impl core::str::FromStr for OpCode {
        type Err = OpCodeError;

        #[inline]
        fn from_str(s: &str) -> Result<Self, Self::Err> {
            Self::parse(s).ok_or(OpCodeError(()))
        }
    }

    impl OpCode {
        /// Parses an opcode from a string.
        #[inline]
        pub fn parse(s: &str) -> Option<Self> {
            NAME_TO_OPCODE.get(s).copied()
        }
    }
}

#[cfg(feature = "parse")]
pub use parse::OpCodeError;

opcodes! {
    0x00 => STOP => stop => stack_io(0, 0), terminating;
    0x01 => ADD => add => stack_io(2, 1);
    0x02 => MUL => mul => stack_io(2, 1);
    0x03 => SUB => sub => stack_io(2, 1);
    0x04 => DIV => div => stack_io(2, 1);
    0x05 => SDIV => sdiv => stack_io(2, 1);
    0x06 => MOD => rem => stack_io(2, 1);
    0x07 => SMOD => smod => stack_io(2, 1);
    0x08 => ADDMOD => addmod => stack_io(3, 1);
    0x09 => MULMOD => mulmod => stack_io(3, 1);
    0x0A => EXP => exp => stack_io(2, 1);
    0x0B => SIGNEXTEND => signextend => stack_io(2, 1);
    // 0x0C
    // 0x0D
    // 0x0E
    // 0x0F

    0x10 => LT => lt => stack_io(2, 1);
    0x11 => GT => gt => stack_io(2, 1);
    0x12 => SLT => slt => stack_io(2, 1);
    0x13 => SGT => sgt => stack_io(2, 1);
    0x14 => EQ => eq => stack_io(2, 1);
    0x15 => ISZERO => iszero => stack_io(1, 1);
    0x16 => AND => bitand => stack_io(2, 1);
    0x17 => OR => bitor => stack_io(2, 1);
    0x18 => XOR => bitxor => stack_io(2, 1);
    0x19 => NOT => not => stack_io(1, 1);
    0x1A => BYTE => byte => stack_io(2, 1);
    0x1B => SHL => shl => stack_io(2, 1);
    0x1C => SHR => shr => stack_io(2, 1);
    0x1D => SAR => sar => stack_io(2, 1);
    0x1E => CLZ => clz => stack_io(1, 1);
    // 0x1F

    0x20 => KECCAK256 => keccak256 => stack_io(2, 1);
    // 0x21
    // 0x22
    // 0x23
    // 0x24
    // 0x25
    // 0x26
    // 0x27
    // 0x28
    // 0x29
    // 0x2A
    // 0x2B
    // 0x2C
    // 0x2D
    // 0x2E
    // 0x2F

    0x30 => ADDRESS => address => stack_io(0, 1);
    0x31 => BALANCE => balance => stack_io(1, 1);
    0x32 => ORIGIN => origin => stack_io(0, 1);
    0x33 => CALLER => caller => stack_io(0, 1);
    0x34 => CALLVALUE => callvalue => stack_io(0, 1);
    0x35 => CALLDATALOAD => calldataload => stack_io(1, 1);
    0x36 => CALLDATASIZE => calldatasize => stack_io(0, 1);
    0x37 => CALLDATACOPY => calldatacopy => stack_io(3, 0);
    0x38 => CODESIZE => codesize => stack_io(0, 1);
    0x39 => CODECOPY => codecopy => stack_io(3, 0);
    0x3A => GASPRICE => gasprice => stack_io(0, 1);
    0x3B => EXTCODESIZE => extcodesize => stack_io(1, 1);
    0x3C => EXTCODECOPY => extcodecopy => stack_io(4, 0);
    0x3D => RETURNDATASIZE => returndatasize => stack_io(0, 1);
    0x3E => RETURNDATACOPY => returndatacopy => stack_io(3, 0);
    0x3F => EXTCODEHASH => extcodehash => stack_io(1, 1);

    0x40 => BLOCKHASH => blockhash => stack_io(1, 1);
    0x41 => COINBASE => coinbase => stack_io(0, 1);
    0x42 => TIMESTAMP => timestamp => stack_io(0, 1);
    0x43 => NUMBER => block_number => stack_io(0, 1);
    0x44 => DIFFICULTY => difficulty => stack_io(0, 1);
    0x45 => GASLIMIT => gaslimit => stack_io(0, 1);
    0x46 => CHAINID => chainid => stack_io(0, 1);
    0x47 => SELFBALANCE => selfbalance => stack_io(0, 1);
    0x48 => BASEFEE => basefee => stack_io(0, 1);
    0x49 => BLOBHASH => blobhash => stack_io(1, 1);
    0x4A => BLOBBASEFEE => blobbasefee => stack_io(0, 1);
    0x4B => SLOTNUM => slotnum => stack_io(0, 1);
    // 0x4C
    // 0x4D
    // 0x4E
    // 0x4F

    0x50 => POP => pop => stack_io(1, 0);
    0x51 => MLOAD => mload => stack_io(1, 1);
    0x52 => MSTORE => mstore => stack_io(2, 0);
    0x53 => MSTORE8 => mstore8 => stack_io(2, 0);
    0x54 => SLOAD => sload => stack_io(1, 1);
    0x55 => SSTORE => sstore => stack_io(2, 0);
    0x56 => JUMP => jump => stack_io(1, 0);
    0x57 => JUMPI => jumpi => stack_io(2, 0);
    0x58 => PC => pc => stack_io(0, 1);
    0x59 => MSIZE => msize => stack_io(0, 1);
    0x5A => GAS => gas => stack_io(0, 1);
    0x5B => JUMPDEST => jumpdest => stack_io(0, 0);
    0x5C => TLOAD => tload => stack_io(1, 1);
    0x5D => TSTORE => tstore => stack_io(2, 0);
    0x5E => MCOPY => mcopy => stack_io(3, 0);

    0x5F => PUSH0 => push<0> => stack_io(0, 1);
    0x60 => PUSH1 => push<1> => stack_io(0, 1), immediate_size(1);
    0x61 => PUSH2 => push<2> => stack_io(0, 1), immediate_size(2);
    0x62 => PUSH3 => push<3> => stack_io(0, 1), immediate_size(3);
    0x63 => PUSH4 => push<4> => stack_io(0, 1), immediate_size(4);
    0x64 => PUSH5 => push<5> => stack_io(0, 1), immediate_size(5);
    0x65 => PUSH6 => push<6> => stack_io(0, 1), immediate_size(6);
    0x66 => PUSH7 => push<7> => stack_io(0, 1), immediate_size(7);
    0x67 => PUSH8 => push<8> => stack_io(0, 1), immediate_size(8);
    0x68 => PUSH9 => push<9> => stack_io(0, 1), immediate_size(9);
    0x69 => PUSH10 => push<10> => stack_io(0, 1), immediate_size(10);
    0x6A => PUSH11 => push<11> => stack_io(0, 1), immediate_size(11);
    0x6B => PUSH12 => push<12> => stack_io(0, 1), immediate_size(12);
    0x6C => PUSH13 => push<13> => stack_io(0, 1), immediate_size(13);
    0x6D => PUSH14 => push<14> => stack_io(0, 1), immediate_size(14);
    0x6E => PUSH15 => push<15> => stack_io(0, 1), immediate_size(15);
    0x6F => PUSH16 => push<16> => stack_io(0, 1), immediate_size(16);
    0x70 => PUSH17 => push<17> => stack_io(0, 1), immediate_size(17);
    0x71 => PUSH18 => push<18> => stack_io(0, 1), immediate_size(18);
    0x72 => PUSH19 => push<19> => stack_io(0, 1), immediate_size(19);
    0x73 => PUSH20 => push<20> => stack_io(0, 1), immediate_size(20);
    0x74 => PUSH21 => push<21> => stack_io(0, 1), immediate_size(21);
    0x75 => PUSH22 => push<22> => stack_io(0, 1), immediate_size(22);
    0x76 => PUSH23 => push<23> => stack_io(0, 1), immediate_size(23);
    0x77 => PUSH24 => push<24> => stack_io(0, 1), immediate_size(24);
    0x78 => PUSH25 => push<25> => stack_io(0, 1), immediate_size(25);
    0x79 => PUSH26 => push<26> => stack_io(0, 1), immediate_size(26);
    0x7A => PUSH27 => push<27> => stack_io(0, 1), immediate_size(27);
    0x7B => PUSH28 => push<28> => stack_io(0, 1), immediate_size(28);
    0x7C => PUSH29 => push<29> => stack_io(0, 1), immediate_size(29);
    0x7D => PUSH30 => push<30> => stack_io(0, 1), immediate_size(30);
    0x7E => PUSH31 => push<31> => stack_io(0, 1), immediate_size(31);
    0x7F => PUSH32 => push<32> => stack_io(0, 1), immediate_size(32);

    0x80 => DUP1 => dup<1> => stack_io(1, 2);
    0x81 => DUP2 => dup<2> => stack_io(2, 3);
    0x82 => DUP3 => dup<3> => stack_io(3, 4);
    0x83 => DUP4 => dup<4> => stack_io(4, 5);
    0x84 => DUP5 => dup<5> => stack_io(5, 6);
    0x85 => DUP6 => dup<6> => stack_io(6, 7);
    0x86 => DUP7 => dup<7> => stack_io(7, 8);
    0x87 => DUP8 => dup<8> => stack_io(8, 9);
    0x88 => DUP9 => dup<9> => stack_io(9, 10);
    0x89 => DUP10 => dup<10> => stack_io(10, 11);
    0x8A => DUP11 => dup<11> => stack_io(11, 12);
    0x8B => DUP12 => dup<12> => stack_io(12, 13);
    0x8C => DUP13 => dup<13> => stack_io(13, 14);
    0x8D => DUP14 => dup<14> => stack_io(14, 15);
    0x8E => DUP15 => dup<15> => stack_io(15, 16);
    0x8F => DUP16 => dup<16> => stack_io(16, 17);

    0x90 => SWAP1 => swap<1> => stack_io(2, 2);
    0x91 => SWAP2 => swap<2> => stack_io(3, 3);
    0x92 => SWAP3 => swap<3> => stack_io(4, 4);
    0x93 => SWAP4 => swap<4> => stack_io(5, 5);
    0x94 => SWAP5 => swap<5> => stack_io(6, 6);
    0x95 => SWAP6 => swap<6> => stack_io(7, 7);
    0x96 => SWAP7 => swap<7> => stack_io(8, 8);
    0x97 => SWAP8 => swap<8> => stack_io(9, 9);
    0x98 => SWAP9 => swap<9> => stack_io(10, 10);
    0x99 => SWAP10 => swap<10> => stack_io(11, 11);
    0x9A => SWAP11 => swap<11> => stack_io(12, 12);
    0x9B => SWAP12 => swap<12> => stack_io(13, 13);
    0x9C => SWAP13 => swap<13> => stack_io(14, 14);
    0x9D => SWAP14 => swap<14> => stack_io(15, 15);
    0x9E => SWAP15 => swap<15> => stack_io(16, 16);
    0x9F => SWAP16 => swap<16> => stack_io(17, 17);

    0xA0 => LOG0 => log<0> => stack_io(2, 0);
    0xA1 => LOG1 => log<1> => stack_io(3, 0);
    0xA2 => LOG2 => log<2> => stack_io(4, 0);
    0xA3 => LOG3 => log<3> => stack_io(5, 0);
    0xA4 => LOG4 => log<4> => stack_io(6, 0);
    // 0xA5
    // 0xA6
    // 0xA7
    // 0xA8
    // 0xA9
    // 0xAA
    // 0xAB
    // 0xAC
    // 0xAD
    // 0xAE
    // 0xAF
    // 0xB0
    // 0xB1
    // 0xB2
    // 0xB3
    // 0xB4
    // 0xB5
    // 0xB6
    // 0xB7
    // 0xB8
    // 0xB9
    // 0xBA
    // 0xBB
    // 0xBC
    // 0xBD
    // 0xBE
    // 0xBF
    // 0xC0
    // 0xC1
    // 0xC2
    // 0xC3
    // 0xC4
    // 0xC5
    // 0xC6
    // 0xC7
    // 0xC8
    // 0xC9
    // 0xCA
    // 0xCB
    // 0xCC
    // 0xCD
    // 0xCE
    // 0xCF
    // 0xD0
    // 0xD1
    // 0xD2
    // 0xD3
    // 0xD4
    // 0xD5
    // 0xD6
    // 0xD7
    // 0xD8
    // 0xD9
    // 0xDA
    // 0xDB
    // 0xDC
    // 0xDD
    // 0xDE
    // 0xDF
    // 0xE0
    // 0xE1
    // 0xE2
    // 0xE3
    // 0xE4
    // 0xE5

    0xE6 => DUPN => dupn => stack_io(0, 1), immediate_size(1);
    0xE7 => SWAPN => swapn => stack_io(0, 0), immediate_size(1);
    0xE8 => EXCHANGE => exchange => stack_io(0, 0), immediate_size(1);
    // 0xE9
    // 0xEA
    // 0xEB
    // 0xEC
    // 0xED
    // 0xEE
    // 0xEF

    0xF0 => CREATE => create<false> => stack_io(3, 1);
    0xF1 => CALL => call => stack_io(7, 1);
    0xF2 => CALLCODE => callcode => stack_io(7, 1);
    0xF3 => RETURN => r#return => stack_io(2, 0), terminating;
    0xF4 => DELEGATECALL => delegatecall => stack_io(6, 1);
    0xF5 => CREATE2 => create<true> => stack_io(4, 1);
    // 0xF6
    // 0xF7
    // 0xF8
    // 0xF9
    0xFA => STATICCALL => staticcall => stack_io(6, 1);
    // 0xFB
    // 0xFC
    0xFD => REVERT => revert => stack_io(2, 0), terminating;
    0xFE => INVALID => invalid => stack_io(0, 0), terminating;
    0xFF => SELFDESTRUCT => selfdestruct => stack_io(1, 0), terminating;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcode_info_matches_table_metadata() {
        assert_eq!(OpCode::ADD.input_output(), (2, 1));
        assert_eq!(OpCode::PUSH32.immediate_size(), 32);
        assert_eq!(OpCode::DUP16.input_output(), (16, 17));
        assert_eq!(OpCode::SWAP16.input_output(), (17, 17));
        assert!(OpCode::RETURN.info().is_terminating());
        assert!(!OpCode::ADD.info().is_terminating());
        assert_eq!(OpCode::new_or_unknown(0x0C).info().name(), "UNKNOWN");
    }
}
