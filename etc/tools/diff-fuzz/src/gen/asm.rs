//! A small EVM bytecode assembler.

use alloy_primitives::{Bytes, U256};

/// EVM opcodes used by the generator.
#[derive(Debug)]
pub struct Op;

#[allow(missing_docs)]
impl Op {
    pub const STOP: u8 = 0x00;
    pub const ADD: u8 = 0x01;
    pub const MUL: u8 = 0x02;
    pub const SUB: u8 = 0x03;
    pub const DIV: u8 = 0x04;
    pub const SDIV: u8 = 0x05;
    pub const MOD: u8 = 0x06;
    pub const SMOD: u8 = 0x07;
    pub const ADDMOD: u8 = 0x08;
    pub const MULMOD: u8 = 0x09;
    pub const EXP: u8 = 0x0a;
    pub const SIGNEXTEND: u8 = 0x0b;
    pub const LT: u8 = 0x10;
    pub const GT: u8 = 0x11;
    pub const SLT: u8 = 0x12;
    pub const SGT: u8 = 0x13;
    pub const EQ: u8 = 0x14;
    pub const ISZERO: u8 = 0x15;
    pub const AND: u8 = 0x16;
    pub const OR: u8 = 0x17;
    pub const XOR: u8 = 0x18;
    pub const NOT: u8 = 0x19;
    pub const BYTE: u8 = 0x1a;
    pub const SHL: u8 = 0x1b;
    pub const SHR: u8 = 0x1c;
    pub const SAR: u8 = 0x1d;
    pub const CLZ: u8 = 0x1e;
    pub const KECCAK256: u8 = 0x20;
    pub const ADDRESS: u8 = 0x30;
    pub const BALANCE: u8 = 0x31;
    pub const ORIGIN: u8 = 0x32;
    pub const CALLER: u8 = 0x33;
    pub const CALLVALUE: u8 = 0x34;
    pub const CALLDATALOAD: u8 = 0x35;
    pub const CALLDATASIZE: u8 = 0x36;
    pub const CALLDATACOPY: u8 = 0x37;
    pub const CODESIZE: u8 = 0x38;
    pub const CODECOPY: u8 = 0x39;
    pub const GASPRICE: u8 = 0x3a;
    pub const EXTCODESIZE: u8 = 0x3b;
    pub const EXTCODECOPY: u8 = 0x3c;
    pub const RETURNDATASIZE: u8 = 0x3d;
    pub const RETURNDATACOPY: u8 = 0x3e;
    pub const EXTCODEHASH: u8 = 0x3f;
    pub const BLOCKHASH: u8 = 0x40;
    pub const COINBASE: u8 = 0x41;
    pub const TIMESTAMP: u8 = 0x42;
    pub const NUMBER: u8 = 0x43;
    pub const PREVRANDAO: u8 = 0x44;
    pub const GASLIMIT: u8 = 0x45;
    pub const CHAINID: u8 = 0x46;
    pub const SELFBALANCE: u8 = 0x47;
    pub const BASEFEE: u8 = 0x48;
    pub const BLOBHASH: u8 = 0x49;
    pub const BLOBBASEFEE: u8 = 0x4a;
    pub const POP: u8 = 0x50;
    pub const MLOAD: u8 = 0x51;
    pub const MSTORE: u8 = 0x52;
    pub const MSTORE8: u8 = 0x53;
    pub const SLOAD: u8 = 0x54;
    pub const SSTORE: u8 = 0x55;
    pub const JUMP: u8 = 0x56;
    pub const JUMPI: u8 = 0x57;
    pub const PC: u8 = 0x58;
    pub const MSIZE: u8 = 0x59;
    pub const GAS: u8 = 0x5a;
    pub const JUMPDEST: u8 = 0x5b;
    pub const TLOAD: u8 = 0x5c;
    pub const TSTORE: u8 = 0x5d;
    pub const MCOPY: u8 = 0x5e;
    pub const PUSH0: u8 = 0x5f;
    pub const PUSH1: u8 = 0x60;
    pub const PUSH2: u8 = 0x61;
    pub const DUP1: u8 = 0x80;
    pub const DUP2: u8 = 0x81;
    pub const DUP3: u8 = 0x82;
    pub const DUP4: u8 = 0x83;
    pub const DUP5: u8 = 0x84;
    pub const DUP6: u8 = 0x85;
    pub const DUP7: u8 = 0x86;
    pub const DUP8: u8 = 0x87;
    pub const SWAP1: u8 = 0x90;
    pub const SWAP2: u8 = 0x91;
    pub const LOG0: u8 = 0xa0;
    pub const CREATE: u8 = 0xf0;
    pub const CALL: u8 = 0xf1;
    pub const CALLCODE: u8 = 0xf2;
    pub const RETURN: u8 = 0xf3;
    pub const DELEGATECALL: u8 = 0xf4;
    pub const CREATE2: u8 = 0xf5;
    pub const STATICCALL: u8 = 0xfa;
    pub const REVERT: u8 = 0xfd;
    pub const INVALID: u8 = 0xfe;
    pub const SELFDESTRUCT: u8 = 0xff;
}

/// Bytecode under construction, with forward labels and a trailing data section.
///
/// Labels are always referenced through `PUSH2`, so code size is known when a jump is emitted.
/// Data blobs are appended after the code; references to them are patched with their offsets.
#[derive(Debug, Default, Clone)]
pub struct Asm {
    code: Vec<u8>,
    labels: Vec<Option<usize>>,
    label_refs: Vec<(usize, usize)>,
    data: Vec<Vec<u8>>,
    data_refs: Vec<(usize, usize)>,
    /// Whether `PUSH0` may be emitted (Shanghai, from Canyon).
    pub push0: bool,
}

impl Asm {
    /// Creates an assembler; `push0` enables the `PUSH0` opcode.
    pub fn new(push0: bool) -> Self {
        Self { push0, ..Default::default() }
    }

    /// Emits one opcode.
    pub fn op(&mut self, op: u8) -> &mut Self {
        self.code.push(op);
        self
    }

    /// Emits several opcodes.
    pub fn ops(&mut self, ops: &[u8]) -> &mut Self {
        self.code.extend_from_slice(ops);
        self
    }

    /// Pushes a value with the shortest encoding.
    pub fn push(&mut self, value: U256) -> &mut Self {
        if value.is_zero() {
            if self.push0 {
                return self.op(Op::PUSH0);
            }
            return self.ops(&[Op::PUSH1, 0]);
        }
        let bytes = value.to_be_bytes::<32>();
        let skip = bytes.iter().take_while(|b| **b == 0).count();
        let len = 32 - skip;
        self.code.push(Op::PUSH1 + len as u8 - 1);
        self.code.extend_from_slice(&bytes[skip..]);
        self
    }

    /// Pushes a small value.
    pub fn push_u64(&mut self, value: u64) -> &mut Self {
        self.push(U256::from(value))
    }

    /// Pushes up to 32 raw bytes as a single word (left-aligned bytes are right-aligned on stack).
    pub fn push_bytes(&mut self, bytes: &[u8]) -> &mut Self {
        assert!(!bytes.is_empty() && bytes.len() <= 32);
        self.code.push(Op::PUSH1 + bytes.len() as u8 - 1);
        self.code.extend_from_slice(bytes);
        self
    }

    /// Allocates a new label.
    pub fn label(&mut self) -> usize {
        self.labels.push(None);
        self.labels.len() - 1
    }

    /// Places a label (emits `JUMPDEST`).
    pub fn place(&mut self, label: usize) -> &mut Self {
        self.labels[label] = Some(self.code.len());
        self.op(Op::JUMPDEST)
    }

    /// Pushes a label's offset.
    pub fn push_label(&mut self, label: usize) -> &mut Self {
        self.code.push(Op::PUSH2);
        self.label_refs.push((self.code.len(), label));
        self.code.extend_from_slice(&[0, 0]);
        self
    }

    /// Unconditional jump to a label.
    pub fn jump(&mut self, label: usize) -> &mut Self {
        self.push_label(label).op(Op::JUMP)
    }

    /// Jump to a label when the top of the stack is non-zero.
    pub fn jumpi(&mut self, label: usize) -> &mut Self {
        self.push_label(label).op(Op::JUMPI)
    }

    /// Appends a data blob after the code and pushes `(len, offset)` so that `CODECOPY` with a
    /// destination pushed afterwards copies it: callers push `dest` and emit `CODECOPY`.
    pub fn push_data(&mut self, blob: Vec<u8>) -> &mut Self {
        let len = blob.len();
        self.data.push(blob);
        self.push_u64(len as u64);
        self.code.push(Op::PUSH2);
        self.data_refs.push((self.code.len(), self.data.len() - 1));
        self.code.extend_from_slice(&[0, 0]);
        self
    }

    /// Copies a data blob into memory at `dest`.
    pub fn copy_data(&mut self, blob: Vec<u8>, dest: u64) -> &mut Self {
        self.push_data(blob).push_u64(dest).op(Op::CODECOPY)
    }

    /// `mstore(offset, value)`.
    pub fn mstore(&mut self, offset: u64, value: U256) -> &mut Self {
        self.push(value).push_u64(offset).op(Op::MSTORE)
    }

    /// Writes arbitrary bytes to memory at `offset` with `MSTORE`s (pads the last word with zeros
    /// beyond the data, which callers must account for).
    pub fn mstore_bytes(&mut self, offset: u64, bytes: &[u8]) -> &mut Self {
        if bytes.len() > 96 {
            return self.copy_data(bytes.to_vec(), offset);
        }
        for (i, chunk) in bytes.chunks(32).enumerate() {
            let mut word = [0u8; 32];
            word[..chunk.len()].copy_from_slice(chunk);
            self.mstore(offset + 32 * i as u64, U256::from_be_bytes(word));
        }
        self
    }

    /// Stores the top of the stack into storage slot `slot`.
    pub fn sstore_top(&mut self, slot: u64) -> &mut Self {
        self.push_u64(slot).op(Op::SSTORE)
    }

    /// Current code length (without the data section).
    pub const fn len(&self) -> usize {
        self.code.len()
    }

    /// Whether no code has been emitted.
    pub const fn is_empty(&self) -> bool {
        self.code.is_empty()
    }

    /// Resolves labels and data references and returns the bytecode.
    pub fn finish(mut self) -> Bytes {
        for (at, label) in &self.label_refs {
            let target = self.labels[*label].expect("label placed");
            self.code[*at..*at + 2].copy_from_slice(&(target as u16).to_be_bytes());
        }
        let mut offset = self.code.len();
        let mut offsets = Vec::with_capacity(self.data.len());
        for blob in &self.data {
            offsets.push(offset);
            offset += blob.len();
        }
        for (at, index) in &self.data_refs {
            self.code[*at..*at + 2].copy_from_slice(&(offsets[*index] as u16).to_be_bytes());
        }
        for blob in self.data {
            self.code.extend_from_slice(&blob);
        }
        self.code.into()
    }

    /// Init code that deploys `runtime` unchanged.
    pub fn deployer(runtime: &[u8], push0: bool) -> Bytes {
        let mut asm = Self::new(push0);
        asm.copy_data(runtime.to_vec(), 0);
        asm.push_u64(runtime.len() as u64).push_u64(0).op(Op::RETURN);
        asm.finish()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;

    use super::{Asm, Op};

    #[test]
    fn labels_and_data_resolve() {
        let mut asm = Asm::new(true);
        let end = asm.label();
        asm.push(U256::ZERO).jumpi(end).copy_data(vec![1, 2, 3], 0).place(end).op(Op::STOP);
        let code = asm.finish();
        // PUSH0 PUSH2 <end> JUMPI PUSH1 3 PUSH2 <data> PUSH0 CODECOPY JUMPDEST STOP DATA
        assert_eq!(code[0], Op::PUSH0);
        let end_offset = u16::from_be_bytes([code[2], code[3]]) as usize;
        assert_eq!(code[end_offset], Op::JUMPDEST);
        let data_offset = u16::from_be_bytes([code[8], code[9]]) as usize;
        assert_eq!(&code[data_offset..], &[1, 2, 3]);
    }
}
