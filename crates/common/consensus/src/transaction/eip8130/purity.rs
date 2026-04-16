//! Bytecode purity analysis for EIP-8130 custom verifiers.
//!
//! A verifier contract is **pure** when its `verify(bytes32, bytes)` call is
//! a deterministic function of its inputs alone — no state reads, no
//! environment-dependent opcodes, and external calls only to known precompiles.
//!
//! Pure verifiers can be evaluated in a minimal sandbox without any chain state,
//! making them safe for mempool validation without tracing.  The analysis result
//! is keyed by `keccak256(deployed_bytecode)` and can be cached permanently
//! (the code hash is immutable), only needing invalidation on a hard fork that
//! introduces new precompiles.
//!
//! # Design Philosophy: No False Positives
//!
//! This scanner prioritises **soundness** — it will never mark unsafe bytecode
//! as "pure". False negatives (safe code rejected) are acceptable; false
//! positives (unsafe code accepted) are not. Every "skip" path has been
//! designed so that the worst case is a spurious rejection.
//!
//! # Algorithm
//!
//! 1. Linear disassembly: walk opcodes, skipping PUSH operands.
//! 2. **Unreachable code tracking** (the sole skip mechanism): after
//!    RETURN/STOP/REVERT/INVALID, all bytes are unreachable until the next
//!    JUMPDEST. Since unreachable code cannot execute, skipping it cannot
//!    produce false positives.
//! 3. **Opcode allowlist** (not banlist): only explicitly-approved opcodes
//!    pass. Any unknown or future opcode is rejected by default.
//! 4. For every `STATICCALL`, verify the Solidity-standard pattern
//!    `PUSH<n> <addr>, GAS, STATICCALL` and check that the target is a
//!    known precompile. Any deviation is rejected.
//! 5. `GAS` (0x5A) is only allowed as the immediate predecessor of
//!    `STATICCALL`.
//! 6. **Hidden JUMPDEST scan**: detect `0x5B` bytes inside PUSH operands
//!    that an attacker could reach via JUMP. The alternate instruction stream
//!    from each hidden JUMPDEST is checked for forbidden opcodes.

extern crate alloc;

use alloc::collections::BTreeSet;
use alloc::vec;
use alloc::vec::Vec;

/// Maximum precompile address in the standard range (ecrecover..BLS_MAP_FP_TO_G1).
const MAX_STANDARD_PRECOMPILE: u16 = 0x12;

/// P256VERIFY precompile address (RIP-7212).
const P256VERIFY_ADDR: u16 = 0x100;

/// EIP-8130 TxContext precompile address.
///
/// Pure read-only precompile that exposes transaction metadata (hash, sender,
/// payer, etc.) to verifier contracts. Safe because the values are fixed for
/// a given transaction and don't depend on chain state or block context.
const TX_CONTEXT_ADDR: u64 = 0xaa03;

/// Result of bytecode purity analysis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PurityVerdict {
    /// The bytecode is pure: deterministic, no state access, calls only known
    /// precompiles.
    Pure {
        /// Precompile addresses called via `STATICCALL`.
        precompile_calls: Vec<u16>,
    },
    /// The bytecode is not pure.
    Impure {
        /// Human-readable reasons for rejection.
        reasons: Vec<PurityViolation>,
    },
}

impl PurityVerdict {
    /// Returns `true` if the bytecode was determined to be pure.
    pub fn is_pure(&self) -> bool {
        matches!(self, Self::Pure { .. })
    }
}

/// A single reason why a verifier's bytecode is not pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PurityViolation {
    /// A forbidden opcode was found in executable code.
    BannedOpcode {
        /// Byte offset in the bytecode.
        offset: usize,
        /// Opcode byte value.
        opcode: u8,
        /// Category of the violation.
        category: ViolationCategory,
    },
    /// A `STATICCALL` whose target address could not be statically determined.
    DynamicStaticCallTarget {
        /// Byte offset of the `STATICCALL` instruction.
        offset: usize,
    },
    /// A `STATICCALL` to an address that is not a known precompile.
    NonPrecompileStaticCall {
        /// Byte offset of the `STATICCALL` instruction.
        offset: usize,
        /// The resolved target address.
        target: u64,
    },
    /// `GAS` opcode used outside the `STATICCALL` calling convention.
    StandaloneGas {
        /// Byte offset of the `GAS` instruction.
        offset: usize,
    },
    /// Bytecode is empty.
    EmptyBytecode,
    /// A forbidden opcode was found in a hidden instruction stream reachable
    /// via a JUMPDEST byte embedded inside a PUSH operand.
    HiddenJumpdestViolation {
        /// Byte offset of the `0x5B` byte inside the PUSH operand.
        hidden_jumpdest_offset: usize,
        /// Byte offset of the forbidden opcode in the alternate stream.
        opcode_offset: usize,
        /// The forbidden opcode byte.
        opcode: u8,
    },
}

/// Category of a forbidden opcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViolationCategory {
    /// Reads or writes persistent/transient storage, or inspects other accounts.
    StateAccess,
    /// Reads block or transaction context that varies across executions.
    NonDeterministicEnv,
    /// Produces side effects (logs, contract creation, value transfer, etc.).
    SideEffect,
    /// Opcode is not on the explicit allowlist (unknown or future opcode).
    ForbiddenOpcode,
}

/// A bytecode purity scanner for EIP-8130 verifier contracts.
#[derive(Debug)]
pub struct PurityScanner;

impl PurityScanner {
    /// Analyze deployed bytecode and return a purity verdict.
    ///
    /// `bytecode` should be the **runtime** (deployed) bytecode, not initcode.
    pub fn analyze(bytecode: &[u8]) -> PurityVerdict {
        if bytecode.is_empty() {
            return PurityVerdict::Impure { reasons: vec![PurityViolation::EmptyBytecode] };
        }

        let insns = disassemble(bytecode);
        let skip_offsets = build_unreachable_set(&insns);

        let mut violations = Vec::new();
        let mut precompile_calls = Vec::new();

        for (idx, insn) in insns.iter().enumerate() {
            if skip_offsets.contains(&insn.offset) {
                continue;
            }

            if insn.opcode == op::STATICCALL {
                match resolve_staticcall_target(&insns, idx) {
                    StaticCallTarget::Precompile(addr) => {
                        precompile_calls.push(addr);
                    }
                    StaticCallTarget::NonPrecompile(addr) => {
                        violations.push(PurityViolation::NonPrecompileStaticCall {
                            offset: insn.offset,
                            target: addr,
                        });
                    }
                    StaticCallTarget::Dynamic => {
                        violations
                            .push(PurityViolation::DynamicStaticCallTarget { offset: insn.offset });
                    }
                }
            } else if insn.opcode == op::GAS {
                let is_before_staticcall = idx + 1 < insns.len()
                    && insns[idx + 1].opcode == op::STATICCALL
                    && !skip_offsets.contains(&insns[idx + 1].offset);
                if !is_before_staticcall {
                    violations.push(PurityViolation::StandaloneGas { offset: insn.offset });
                }
            } else if !is_allowed_opcode(insn.opcode) {
                violations.push(PurityViolation::BannedOpcode {
                    offset: insn.offset,
                    opcode: insn.opcode,
                    category: violation_category(insn.opcode),
                });
            }
        }

        violations.extend(scan_hidden_jumpdests(bytecode, &insns));

        if violations.is_empty() {
            PurityVerdict::Pure { precompile_calls }
        } else {
            PurityVerdict::Impure { reasons: violations }
        }
    }
}

// ── Opcode constants ────────────────────────────────────────────────────

mod op {
    pub(super) const STOP: u8 = 0x00;
    pub(super) const PUSH0: u8 = 0x5F;
    pub(super) const PUSH1: u8 = 0x60;
    pub(super) const PUSH32: u8 = 0x7F;
    pub(super) const GAS: u8 = 0x5A;
    pub(super) const JUMPDEST: u8 = 0x5B;
    pub(super) const STATICCALL: u8 = 0xFA;
    pub(super) const RETURN: u8 = 0xF3;
    pub(super) const REVERT: u8 = 0xFD;
    pub(super) const INVALID: u8 = 0xFE;
}

// ── Opcode allowlist ─────────────────────────────────────────────────────
//
// Only explicitly-approved opcodes pass. Any opcode not listed here is
// rejected, including future EVM additions. This prevents false positives
// from opcodes we haven't reviewed.

/// Returns `true` if the opcode is on the explicit allowlist.
///
/// GAS (0x5A) and STATICCALL (0xFA) are intentionally excluded — they have
/// their own validation paths in the main analysis loop.
fn is_allowed_opcode(opcode: u8) -> bool {
    matches!(
        opcode,
        0x00        |       // STOP
        0x01..=0x0B |       // ADD, MUL, SUB, DIV, SDIV, MOD, SMOD, ADDMOD, MULMOD, EXP, SIGNEXTEND
        0x10..=0x1D |       // LT, GT, SLT, SGT, EQ, ISZERO, AND, OR, XOR, NOT, BYTE, SHL, SHR, SAR
        0x20        |       // SHA3 (KECCAK256)
        0x30        |       // ADDRESS
        0x33..=0x39 |       // CALLER, CALLVALUE, CALLDATALOAD, CALLDATASIZE, CALLDATACOPY, CODESIZE, CODECOPY
        0x3D..=0x3E |       // RETURNDATASIZE, RETURNDATACOPY
        0x46        |       // CHAINID (constant per chain)
        0x50..=0x53 |       // POP, MLOAD, MSTORE, MSTORE8
        0x56..=0x59 |       // JUMP, JUMPI, PC, MSIZE
        0x5B        |       // JUMPDEST
        0x5E..=0x7F |       // MCOPY, PUSH0, PUSH1..PUSH32
        0x80..=0x9F |       // DUP1..DUP16, SWAP1..SWAP16
        0xF3        |       // RETURN
        0xFD..=0xFE         // REVERT, INVALID
    )
}

/// Returns a descriptive category for a forbidden opcode.
fn violation_category(opcode: u8) -> ViolationCategory {
    match opcode {
        0x31 | 0x3B | 0x3C | 0x3F | 0x47 | 0x54 | 0x55 | 0x5C | 0x5D => {
            ViolationCategory::StateAccess
        }
        0x32 | 0x3A | 0x40..=0x45 | 0x48..=0x4A => ViolationCategory::NonDeterministicEnv,
        0xA0..=0xA4 | 0xF0..=0xF2 | 0xF4..=0xF5 | 0xFF => ViolationCategory::SideEffect,
        _ => ViolationCategory::ForbiddenOpcode,
    }
}

/// Returns `true` if the given address is a known safe precompile.
fn is_known_precompile(addr: u64) -> bool {
    (1..=MAX_STANDARD_PRECOMPILE as u64).contains(&addr)
        || addr == P256VERIFY_ADDR as u64
        || addr == TX_CONTEXT_ADDR
}

// ── Disassembler ────────────────────────────────────────────────────────

struct Instruction {
    offset: usize,
    opcode: u8,
    /// For PUSH instructions, the pushed value (up to 32 bytes as u64 for
    /// precompile address comparison; values > u64::MAX are stored as
    /// `u64::MAX` since no precompile has an address that large).
    push_value: Option<u64>,
    /// Number of operand bytes following this PUSH instruction (0 for
    /// non-PUSH opcodes). Used by the hidden-JUMPDEST scanner.
    push_size: usize,
}

fn disassemble(code: &[u8]) -> Vec<Instruction> {
    let mut insns = Vec::new();
    let mut i = 0;
    while i < code.len() {
        let opcode = code[i];
        if (op::PUSH1..=op::PUSH32).contains(&opcode) {
            let push_size = (opcode - op::PUSH0) as usize;
            let end = core::cmp::min(i + 1 + push_size, code.len());
            let bytes = &code[i + 1..end];
            let value = if bytes.len() <= 8 {
                let mut buf = [0u8; 8];
                buf[8 - bytes.len()..].copy_from_slice(bytes);
                u64::from_be_bytes(buf)
            } else {
                let high_bytes = &bytes[..bytes.len() - 8];
                if high_bytes.iter().all(|&b| b == 0) {
                    let mut buf = [0u8; 8];
                    buf.copy_from_slice(&bytes[bytes.len() - 8..]);
                    u64::from_be_bytes(buf)
                } else {
                    u64::MAX
                }
            };
            insns.push(Instruction { offset: i, opcode, push_value: Some(value), push_size });
            i = end;
        } else if opcode == op::PUSH0 {
            insns.push(Instruction { offset: i, opcode, push_value: Some(0), push_size: 0 });
            i += 1;
        } else {
            insns.push(Instruction { offset: i, opcode, push_value: None, push_size: 0 });
            i += 1;
        }
    }
    insns
}

/// Build the set of byte offsets that should be skipped during analysis.
///
/// The **only** skip mechanism is unreachable code tracking: after
/// RETURN/STOP/REVERT/INVALID, all bytes are unreachable until the next
/// JUMPDEST. This is provably sound — unreachable code cannot execute, so
/// skipping it cannot produce false positives.
///
/// Previous versions also had CODECOPY-based data region tracking and CBOR
/// metadata stripping. Both were removed because they opened potential
/// false-positive attack vectors (see audit notes). The cost is a small
/// increase in false negatives when metadata or immutable data bytes happen
/// to alias banned opcodes after a spurious JUMPDEST byte.
fn build_unreachable_set(insns: &[Instruction]) -> BTreeSet<usize> {
    let mut skip = BTreeSet::new();
    let mut unreachable = false;
    for insn in insns {
        if unreachable {
            if insn.opcode == op::JUMPDEST {
                unreachable = false;
            } else {
                skip.insert(insn.offset);
                continue;
            }
        }
        if matches!(insn.opcode, op::STOP | op::RETURN | op::REVERT | op::INVALID) {
            unreachable = true;
        }
    }
    skip
}

// ── Hidden JUMPDEST scanner ─────────────────────────────────────────────
//
// The EVM allows a JUMP to target any byte whose value is 0x5B, even if
// our linear disassembler treats that byte as a PUSH operand. An attacker
// can hide `JUMPDEST + SLOAD` inside `PUSH2 0x5B54` and then JUMP to the
// operand. We scan all PUSH operands for 0x5B bytes, re-disassemble the
// alternate instruction stream, and check for forbidden opcodes.

fn scan_hidden_jumpdests(code: &[u8], insns: &[Instruction]) -> Vec<PurityViolation> {
    let mut violations = Vec::new();

    for insn in insns {
        if insn.push_size == 0 {
            continue;
        }
        for j in 1..=insn.push_size {
            let hidden_off = insn.offset + j;
            if hidden_off >= code.len() || code[hidden_off] != op::JUMPDEST {
                continue;
            }
            let alt_insns = disassemble(&code[hidden_off..]);
            for alt in alt_insns.iter().skip(1) {
                let abs = hidden_off + alt.offset;
                if alt.opcode == op::STATICCALL || alt.opcode == op::GAS {
                    violations.push(PurityViolation::HiddenJumpdestViolation {
                        hidden_jumpdest_offset: hidden_off,
                        opcode_offset: abs,
                        opcode: alt.opcode,
                    });
                    break;
                }
                if !is_allowed_opcode(alt.opcode) {
                    violations.push(PurityViolation::HiddenJumpdestViolation {
                        hidden_jumpdest_offset: hidden_off,
                        opcode_offset: abs,
                        opcode: alt.opcode,
                    });
                    break;
                }
                if matches!(alt.opcode, op::STOP | op::RETURN | op::REVERT | op::INVALID) {
                    break;
                }
            }
        }
    }

    violations
}

// ── STATICCALL target resolution ────────────────────────────────────────

enum StaticCallTarget {
    /// Target is a known precompile at this address.
    Precompile(u16),
    /// Target is a hardcoded address that isn't a known precompile.
    NonPrecompile(u64),
    /// Target address could not be determined statically.
    Dynamic,
}

/// Resolve the target address of a `STATICCALL` instruction.
///
/// The Solidity compiler always emits `PUSH<n> addr, GAS, STATICCALL` for
/// precompile calls. Any deviation is rejected.
fn resolve_staticcall_target(insns: &[Instruction], sc_idx: usize) -> StaticCallTarget {
    if sc_idx < 2 {
        return StaticCallTarget::Dynamic;
    }

    let gas_insn = &insns[sc_idx - 1];
    let addr_insn = &insns[sc_idx - 2];

    if gas_insn.opcode != op::GAS {
        return StaticCallTarget::Dynamic;
    }

    match addr_insn.push_value {
        Some(addr) if is_known_precompile(addr) => StaticCallTarget::Precompile(addr as u16),
        Some(addr) => StaticCallTarget::NonPrecompile(addr),
        None => StaticCallTarget::Dynamic,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_decode(s: &str) -> Vec<u8> {
        alloy_primitives::hex::decode(s.trim()).expect("invalid hex in test fixture")
    }

    fn load_fixture_hex(name: &str) -> Option<Vec<u8>> {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/fixtures").join(name);
        let contents = std::fs::read_to_string(path).ok()?;
        Some(hex_decode(&contents))
    }

    #[test]
    fn empty_bytecode_is_impure() {
        let verdict = PurityScanner::analyze(&[]);
        assert!(!verdict.is_pure());
        match verdict {
            PurityVerdict::Impure { reasons } => {
                assert!(reasons.iter().any(|r| matches!(r, PurityViolation::EmptyBytecode)));
            }
            _ => panic!("expected impure"),
        }
    }

    #[test]
    fn minimal_pure_bytecode() {
        let code = [0x60, 0x00, 0x60, 0x00, 0xF3];
        let verdict = PurityScanner::analyze(&code);
        assert!(verdict.is_pure());
    }

    #[test]
    fn sload_is_impure() {
        let code = [0x60, 0x00, 0x54, 0x50, 0x60, 0x00, 0x60, 0x00, 0xF3];
        let verdict = PurityScanner::analyze(&code);
        assert!(!verdict.is_pure());
        match verdict {
            PurityVerdict::Impure { reasons } => {
                assert!(reasons.iter().any(|r| matches!(
                    r,
                    PurityViolation::BannedOpcode {
                        opcode: 0x54,
                        category: ViolationCategory::StateAccess,
                        ..
                    }
                )));
            }
            _ => panic!("expected impure"),
        }
    }

    #[test]
    fn timestamp_is_impure() {
        let code = [0x42, 0x50, 0x60, 0x00, 0x60, 0x00, 0xF3];
        let verdict = PurityScanner::analyze(&code);
        assert!(!verdict.is_pure());
        match verdict {
            PurityVerdict::Impure { reasons } => {
                assert!(reasons.iter().any(|r| matches!(
                    r,
                    PurityViolation::BannedOpcode {
                        opcode: 0x42,
                        category: ViolationCategory::NonDeterministicEnv,
                        ..
                    }
                )));
            }
            _ => panic!("expected impure"),
        }
    }

    #[test]
    fn all_env_opcodes_rejected() {
        for opcode in [0x32u8, 0x3A, 0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x48, 0x49, 0x4A] {
            let code = [opcode, 0x50, 0x60, 0x00, 0x60, 0x00, 0xF3];
            let verdict = PurityScanner::analyze(&code);
            assert!(!verdict.is_pure(), "opcode 0x{opcode:02x} should be rejected");
        }
    }

    #[test]
    fn all_state_opcodes_rejected() {
        for opcode in [0x31u8, 0x3B, 0x3C, 0x3F, 0x47, 0x54, 0x55, 0x5C, 0x5D] {
            let code = [opcode, 0x50, 0x60, 0x00, 0x60, 0x00, 0xF3];
            let verdict = PurityScanner::analyze(&code);
            assert!(!verdict.is_pure(), "opcode 0x{opcode:02x} should be rejected");
        }
    }

    #[test]
    fn all_side_effect_opcodes_rejected() {
        for opcode in [0xA0u8, 0xA4, 0xF0, 0xF1, 0xF2, 0xF4, 0xF5, 0xFF] {
            let code = [opcode, 0x50, 0x60, 0x00, 0x60, 0x00, 0xF3];
            let verdict = PurityScanner::analyze(&code);
            assert!(!verdict.is_pure(), "opcode 0x{opcode:02x} should be rejected");
        }
    }

    #[test]
    fn unknown_opcodes_rejected() {
        for opcode in [0x0Cu8, 0x0D, 0x21, 0x4B, 0xA5, 0xB0, 0xC0, 0xEF] {
            let code = [opcode, 0x50, 0x60, 0x00, 0x60, 0x00, 0xF3];
            let verdict = PurityScanner::analyze(&code);
            assert!(!verdict.is_pure(), "unknown opcode 0x{opcode:02x} must be rejected");
            match verdict {
                PurityVerdict::Impure { reasons } => {
                    assert!(reasons.iter().any(|r| matches!(
                        r,
                        PurityViolation::BannedOpcode {
                            category: ViolationCategory::ForbiddenOpcode,
                            ..
                        }
                    )));
                }
                _ => panic!("expected impure"),
            }
        }
    }

    #[test]
    fn staticcall_to_ecrecover_is_pure() {
        #[rustfmt::skip]
        let code = [
            0x60, 0x20,  // PUSH1 retLen
            0x60, 0x00,  // PUSH1 retOffset
            0x60, 0x80,  // PUSH1 argsLen
            0x60, 0x00,  // PUSH1 argsOffset
            0x60, 0x01,  // PUSH1 addr = ecrecover
            0x5A,        // GAS
            0xFA,        // STATICCALL
            0x50,        // POP
            0x60, 0x20,  // PUSH1 0x20
            0x60, 0x00,  // PUSH1 0x00
            0xF3,        // RETURN
        ];
        let verdict = PurityScanner::analyze(&code);
        assert!(verdict.is_pure());
        match verdict {
            PurityVerdict::Pure { precompile_calls } => {
                assert_eq!(precompile_calls, vec![1]);
            }
            _ => panic!("expected pure"),
        }
    }

    #[test]
    fn staticcall_to_p256verify_is_pure() {
        #[rustfmt::skip]
        let code = [
            0x60, 0x20, 0x60, 0x00, 0x60, 0x80, 0x60, 0x00,
            0x61, 0x01, 0x00, // PUSH2 addr = P256VERIFY (0x100)
            0x5A,           // GAS
            0xFA,           // STATICCALL
            0x50,
            0x60, 0x20, 0x60, 0x00, 0xF3,
        ];
        let verdict = PurityScanner::analyze(&code);
        assert!(verdict.is_pure());
        match verdict {
            PurityVerdict::Pure { precompile_calls } => {
                assert_eq!(precompile_calls, vec![0x100]);
            }
            _ => panic!("expected pure"),
        }
    }

    #[test]
    fn staticcall_to_non_precompile_is_impure() {
        #[rustfmt::skip]
        let code = [
            0x60, 0x20, 0x60, 0x00, 0x60, 0x80, 0x60, 0x00,
            0x61, 0x02, 0x00,
            0x5A, 0xFA,
            0x50, 0x60, 0x20, 0x60, 0x00, 0xF3,
        ];
        let verdict = PurityScanner::analyze(&code);
        assert!(!verdict.is_pure());
    }

    #[test]
    fn dynamic_staticcall_target_is_impure() {
        #[rustfmt::skip]
        let code = [
            0x60, 0x20, 0x60, 0x00, 0x60, 0x80, 0x60, 0x00,
            0x60, 0x04, 0x35,  // CALLDATALOAD (dynamic)
            0x5A, 0xFA,
            0x50, 0x60, 0x20, 0x60, 0x00, 0xF3,
        ];
        let verdict = PurityScanner::analyze(&code);
        assert!(!verdict.is_pure());
    }

    #[test]
    fn standalone_gas_is_impure() {
        let code = [0x5A, 0x50, 0x60, 0x00, 0x60, 0x00, 0xF3];
        let verdict = PurityScanner::analyze(&code);
        assert!(!verdict.is_pure());
        match verdict {
            PurityVerdict::Impure { reasons } => {
                assert!(reasons.iter().any(|r| matches!(r, PurityViolation::StandaloneGas { .. })));
            }
            _ => panic!("expected impure"),
        }
    }

    #[test]
    fn metadata_after_return_is_unreachable() {
        // No longer stripped — handled by unreachable code tracking. The
        // metadata sits after RETURN so it's unreachable and skipped.
        let mut code = vec![0x60, 0x00, 0x60, 0x00, 0xF3]; // PUSH, PUSH, RETURN
        let meta = vec![0xa2, 0x64, 0x54, 0x54, 0x54, 0x54]; // fake CBOR with SLOAD bytes
        let meta_len = meta.len() as u16;
        code.extend_from_slice(&meta);
        code.extend_from_slice(&meta_len.to_be_bytes());
        let verdict = PurityScanner::analyze(&code);
        assert!(verdict.is_pure(), "metadata after RETURN is unreachable");
    }

    #[test]
    fn bls_precompiles_are_safe() {
        for addr in 0x0Bu8..=0x12 {
            #[rustfmt::skip]
            let code = [
                0x60, 0x20, 0x60, 0x00, 0x60, 0x80, 0x60, 0x00,
                0x60, addr,
                0x5A, 0xFA,
                0x50, 0x60, 0x20, 0x60, 0x00, 0xF3,
            ];
            let verdict = PurityScanner::analyze(&code);
            assert!(verdict.is_pure(), "BLS precompile 0x{addr:02x} should be safe");
        }
    }

    #[test]
    fn staticcall_to_tx_context_is_pure() {
        #[rustfmt::skip]
        let code = [
            0x60, 0x20, 0x60, 0x00, 0x60, 0x80, 0x60, 0x00,
            0x61, 0xAA, 0x03,
            0x5A, 0xFA,
            0x50, 0x60, 0x20, 0x60, 0x00, 0xF3,
        ];
        let verdict = PurityScanner::analyze(&code);
        assert!(verdict.is_pure(), "TxContext precompile should be safe, got: {verdict:?}");
        match verdict {
            PurityVerdict::Pure { precompile_calls } => {
                assert_eq!(precompile_calls, vec![0xaa03]);
            }
            _ => panic!("expected pure"),
        }
    }

    #[test]
    fn staticcall_to_nonce_manager_is_impure() {
        #[rustfmt::skip]
        let code = [
            0x60, 0x20, 0x60, 0x00, 0x60, 0x80, 0x60, 0x00,
            0x61, 0xAA, 0x02,
            0x5A, 0xFA,
            0x50, 0x60, 0x20, 0x60, 0x00, 0xF3,
        ];
        let verdict = PurityScanner::analyze(&code);
        assert!(!verdict.is_pure(), "NonceManager precompile does storage reads");
    }

    #[test]
    fn k1_verifier_bytecode_is_pure() {
        let Some(bytecode) = load_fixture_hex("k1_verifier_runtime.hex") else {
            return;
        };
        let verdict = PurityScanner::analyze(&bytecode);
        assert!(verdict.is_pure(), "K1Verifier should be pure, got: {verdict:?}");
        match verdict {
            PurityVerdict::Pure { precompile_calls } => {
                assert!(precompile_calls.contains(&1), "should call ecrecover");
            }
            _ => panic!("expected pure"),
        }
    }

    #[test]
    fn p256_verifier_bytecode_is_pure() {
        let Some(bytecode) = load_fixture_hex("p256_verifier_runtime.hex") else {
            return;
        };
        let verdict = PurityScanner::analyze(&bytecode);
        assert!(verdict.is_pure(), "P256Verifier should be pure, got: {verdict:?}");
        match verdict {
            PurityVerdict::Pure { precompile_calls } => {
                assert!(precompile_calls.contains(&P256VERIFY_ADDR), "should call P256VERIFY");
            }
            _ => panic!("expected pure"),
        }
    }

    #[test]
    fn always_valid_verifier_is_pure() {
        let Some(bytecode) = load_fixture_hex("always_valid_verifier_runtime.hex") else {
            return;
        };
        let verdict = PurityScanner::analyze(&bytecode);
        assert!(verdict.is_pure(), "AlwaysValidVerifier should be pure, got: {verdict:?}");
    }

    #[test]
    fn multiple_violations_collected() {
        let code = [0x54, 0x42, 0xA0, 0x60, 0x00, 0x60, 0x00, 0xF3];
        let verdict = PurityScanner::analyze(&code);
        match verdict {
            PurityVerdict::Impure { reasons } => {
                assert!(reasons.len() >= 3, "should collect all violations, got: {reasons:?}");
            }
            _ => panic!("expected impure"),
        }
    }

    // ── Adversarial bypass tests ────────────────────────────────────────

    #[test]
    fn attack_codecopy_masks_sload() {
        #[rustfmt::skip]
        let code = [
            0x60, 0x00, 0x54, 0x50, 0x60, 0x00, 0x60, 0x00, 0xF3,
            0x60, 0x00, 0x60, 0x01, 0x60, 0x04, 0x39,
        ];
        let verdict = PurityScanner::analyze(&code);
        assert!(!verdict.is_pure(), "backward CODECOPY must not mask SLOAD");
    }

    #[test]
    fn attack_codecopy_masks_timestamp() {
        #[rustfmt::skip]
        let code = [
            0x42, 0x50, 0x60, 0x00, 0x60, 0x00, 0xF3,
            0x60, 0x00, 0x60, 0x00, 0x60, 0x02, 0x39,
        ];
        let verdict = PurityScanner::analyze(&code);
        assert!(!verdict.is_pure(), "backward CODECOPY must not mask TIMESTAMP");
    }

    #[test]
    fn attack_staticcall_explicit_gas_bypasses_check() {
        #[rustfmt::skip]
        let code = [
            0x60, 0x20, 0x60, 0x00, 0x60, 0x80, 0x60, 0x00,
            0x73, 0xDE, 0xAD, 0xBE, 0xEF,
                  0x00, 0x00, 0x00, 0x00,
                  0x00, 0x00, 0x00, 0x00,
                  0x00, 0x00, 0x00, 0x00,
                  0x00, 0x00, 0x00, 0x00,
            0x62, 0x01, 0x00, 0x00, // PUSH3 (explicit gas)
            0xFA,                    // STATICCALL
            0x50, 0x60, 0x00, 0x60, 0x00, 0xF3,
        ];
        let verdict = PurityScanner::analyze(&code);
        assert!(!verdict.is_pure(), "explicit-gas STATICCALL must not be silently skipped");
    }

    #[test]
    fn legitimate_data_after_return_is_ignored() {
        #[rustfmt::skip]
        let code = [
            0x60, 0x00, 0x60, 0x00, 0xF3,
            0xFF, 0xFF, 0x54, 0x42, 0xFA,
        ];
        let verdict = PurityScanner::analyze(&code);
        assert!(verdict.is_pure(), "unreachable data after RETURN should be ignored");
    }

    #[test]
    fn jumpdest_after_return_resumes_checking() {
        #[rustfmt::skip]
        let code = [
            0x60, 0x00, 0x60, 0x00, 0xF3,
            0xFF,
            0x5B,   // JUMPDEST
            0x54,   // SLOAD
            0x50, 0x60, 0x00, 0x60, 0x00, 0xF3,
        ];
        let verdict = PurityScanner::analyze(&code);
        assert!(!verdict.is_pure(), "SLOAD after JUMPDEST must be caught");
    }

    #[test]
    fn forward_codecopy_data_after_return_is_unreachable() {
        // Data referenced by forward CODECOPY sits after RETURN, so layer-1
        // unreachable tracking handles it. This is a deliberate false-negative
        // trade-off: CODECOPY data region tracking was removed to eliminate
        // a false-positive attack vector.
        #[rustfmt::skip]
        let code = [
            0x60, 0x00,  // PUSH1 destOffset
            0x60, 0x10,  // PUSH1 codeOffset = 0x10 (forward)
            0x60, 0x04,  // PUSH1 size = 4
            0x39,        // CODECOPY
            0x60, 0x00, 0x51, 0x50,
            0x60, 0x00, 0x60, 0x00,
            0xF3,        // RETURN at offset 0x0F
            // Data at 0x10 — after RETURN, unreachable
            0xFF, 0xFF, 0x54, 0x42,
        ];
        let verdict = PurityScanner::analyze(&code);
        assert!(verdict.is_pure(), "data after RETURN is unreachable");
    }

    // ── Hidden JUMPDEST attack tests ────────────────────────────────────

    #[test]
    fn attack_hidden_jumpdest_sload_in_push() {
        // PUSH2 0x5B54 hides JUMPDEST(5B) + SLOAD(54) at offsets 1-2.
        // An attacker JUMPs to offset 1 at runtime.
        #[rustfmt::skip]
        let code = [
            0x61, 0x5B, 0x54,  // PUSH2 0x5B54 (hides JUMPDEST+SLOAD)
            0x50,               // POP
            0x60, 0x01,         // PUSH1 0x01 (hidden JUMPDEST offset)
            0x56,               // JUMP
            0x00,               // STOP
        ];
        let verdict = PurityScanner::analyze(&code);
        assert!(!verdict.is_pure(), "hidden JUMPDEST + SLOAD in PUSH must be caught");
        match verdict {
            PurityVerdict::Impure { reasons } => {
                assert!(
                    reasons
                        .iter()
                        .any(|r| matches!(r, PurityViolation::HiddenJumpdestViolation { .. }))
                );
            }
            _ => panic!("expected impure"),
        }
    }

    #[test]
    fn attack_hidden_jumpdest_timestamp_in_push() {
        // PUSH2 0x5B42 hides JUMPDEST(5B) + TIMESTAMP(42)
        #[rustfmt::skip]
        let code = [
            0x61, 0x5B, 0x42,  // PUSH2 0x5B42
            0x50, 0x60, 0x00, 0x60, 0x00, 0xF3,
        ];
        let verdict = PurityScanner::analyze(&code);
        assert!(!verdict.is_pure(), "hidden JUMPDEST + TIMESTAMP in PUSH must be caught");
    }

    #[test]
    fn attack_hidden_jumpdest_call_in_push32() {
        // Large PUSH hiding JUMPDEST + CALL deep in its operand
        #[rustfmt::skip]
        let code = [
            0x7F,  // PUSH32
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x5B,  // JUMPDEST at operand byte 15
            0xF1,  // CALL at operand byte 16
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x50, 0x60, 0x00, 0x60, 0x00, 0xF3,
        ];
        let verdict = PurityScanner::analyze(&code);
        assert!(!verdict.is_pure(), "hidden JUMPDEST + CALL in PUSH32 must be caught");
    }

    #[test]
    fn benign_push_with_5b_byte_no_violation() {
        // PUSH2 0x5B00 — the 0x5B is a JUMPDEST, but the following byte is
        // STOP (0x00) which is allowed. The hidden path scan should not find
        // any forbidden opcodes and should not cause a false negative.
        #[rustfmt::skip]
        let code = [
            0x61, 0x5B, 0x00,  // PUSH2 0x5B00 — hidden JUMPDEST then STOP
            0x50,               // POP
            0x60, 0x00,
            0x60, 0x00,
            0xF3,               // RETURN
        ];
        let verdict = PurityScanner::analyze(&code);
        assert!(verdict.is_pure(), "hidden JUMPDEST followed by only safe opcodes is OK");
    }

    #[test]
    fn attack_fake_metadata_hides_sload() {
        // Metadata stripping removed — this attack no longer works.
        // The attacker arranges a fake 0xa2 suffix to strip real code.
        #[rustfmt::skip]
        let mut code = vec![
            0x60, 0x00, 0x60, 0x00, 0xF3,  // PUSH, PUSH, RETURN
            0x5B,                            // JUMPDEST (reachable!)
            0x54,                            // SLOAD
            0x50, 0x60, 0x00, 0x60, 0x00, 0xF3,
        ];
        // Craft a fake metadata suffix: 0xa2 at offset 5 (the JUMPDEST)
        // then last 2 bytes encode the length from 0xa2 to end-2.
        let _len_from_a2 = code.len() - 5;
        // But wait — 0xa2 != 0x5B. The attacker must place 0xa2 at the right
        // spot. For this test, we just append a suffix that the old stripper
        // would have matched.
        code.clear();
        #[rustfmt::skip]
        code.extend_from_slice(&[
            0x60, 0x06,  // PUSH1 0x06 (jump target)
            0x56,        // JUMP
            0x00,        // padding
            0x00,        // padding
            0xa2,        // old stripper would start stripping here (LOG2)
            0x5B,        // JUMPDEST
            0x54,        // SLOAD
            0x50, 0x60, 0x00, 0x60, 0x00, 0xF3,
        ]);
        let meta_len: u16 = (code.len() - 5) as u16; // from 0xa2 onward
        code.extend_from_slice(&meta_len.to_be_bytes());
        let verdict = PurityScanner::analyze(&code);
        assert!(!verdict.is_pure(), "fake metadata suffix must not hide SLOAD");
    }

    #[test]
    fn allowlist_rejects_future_opcodes() {
        // Hypothetical future opcode 0xEF (currently undefined in pre-EOF EVM)
        // must be rejected even though it's not in any banlist.
        let code = [0xEF, 0x50, 0x60, 0x00, 0x60, 0x00, 0xF3];
        let verdict = PurityScanner::analyze(&code);
        assert!(!verdict.is_pure(), "undefined opcode 0xEF must be rejected by allowlist");
    }
}
