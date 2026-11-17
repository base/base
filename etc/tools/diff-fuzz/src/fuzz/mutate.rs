//! Structure-aware mutations of fuzz case descriptors.

use alloy_primitives::{Bytes, U256};
use serde::Serialize;

use crate::{
    ByteEdit, CodeEdit, FeeTarget, FuzzCase, GasTarget, L1Tweak, Rng, Slot, SlotKind, Tweak,
};

/// The mutation operators, named in statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Mutation {
    /// Re-rolls one slot's random stream.
    Reroll,
    /// Changes one slot's kind.
    Rekind,
    /// Inserts a new slot.
    Insert,
    /// Removes a slot.
    Remove,
    /// Duplicates a slot.
    Duplicate,
    /// Swaps two slots.
    Swap,
    /// Splices slots from another corpus case.
    Splice,
    /// Adds a gas, value, calldata, fee, or mint tweak.
    Tweak,
    /// Turns a slot into a mutated program or edits its bytecode further.
    Code,
    /// Edits a field of the L1 attributes deposit.
    L1,
    /// Moves the slots onto another pre-state.
    Rebase,
    /// Re-rolls a native precompile call's function, sender, ABI mutation, or arguments, or turns
    /// a slot into one.
    Native,
}

impl Mutation {
    /// Every operator.
    pub const ALL: [Self; 12] = [
        Self::Reroll,
        Self::Rekind,
        Self::Insert,
        Self::Remove,
        Self::Duplicate,
        Self::Swap,
        Self::Splice,
        Self::Tweak,
        Self::Code,
        Self::L1,
        Self::Rebase,
        Self::Native,
    ];

    /// Relative selection weights, in [`Self::ALL`] order.
    pub const WEIGHTS: [u64; 12] = [16, 10, 8, 4, 3, 3, 8, 18, 12, 6, 6, 12];
}

/// Applies random mutations to fuzz cases.
#[derive(Debug)]
pub struct Mutator {
    /// Random source.
    pub rng: Rng,
    /// Most slots a case may have.
    pub max_slots: usize,
    /// Generator indices `Rebase` may move a case to are below this.
    pub index_space: u64,
}

impl Mutator {
    /// Mutates `parent` one to four times. `donor` is another corpus case for splicing. Returns
    /// the child and the operators applied.
    pub fn mutate(
        &mut self,
        parent: &FuzzCase,
        donor: Option<&FuzzCase>,
    ) -> (FuzzCase, Vec<Mutation>) {
        let mut child = parent.clone();
        let mut applied = Vec::new();
        let rounds = 1 + self.rng.below(4) as usize;
        for _ in 0..rounds {
            let mutation = Mutation::ALL[self.rng.weighted(&Mutation::WEIGHTS)];
            if self.apply(mutation, &mut child, donor) {
                applied.push(mutation);
            }
        }
        if applied.is_empty() || child == *parent {
            self.apply(Mutation::Reroll, &mut child, None);
            applied.push(Mutation::Reroll);
        }
        (child, applied)
    }

    fn slot(&mut self, case: &FuzzCase) -> Option<usize> {
        (!case.slots.is_empty()).then(|| self.rng.index(case.slots.len()))
    }

    fn apply(&mut self, mutation: Mutation, case: &mut FuzzCase, donor: Option<&FuzzCase>) -> bool {
        match mutation {
            Mutation::Reroll => {
                let Some(i) = self.slot(case) else {
                    return self.apply(Mutation::Insert, case, None);
                };
                case.slots[i].salt = self.rng.next_u64();
                case.slots[i].tweaks.clear();
            }
            Mutation::Rekind => {
                let Some(i) = self.slot(case) else { return false };
                case.slots[i] =
                    Slot { kind: self.kind(), salt: self.rng.next_u64(), tweaks: Vec::new() };
            }
            Mutation::Insert => {
                if case.slots.len() >= self.max_slots {
                    return false;
                }
                let at = self.rng.index(case.slots.len() + 1);
                let slot =
                    Slot { kind: self.kind(), salt: self.rng.next_u64(), tweaks: Vec::new() };
                case.slots.insert(at, slot);
            }
            Mutation::Remove => {
                if case.slots.len() < 2 {
                    return false;
                }
                let i = self.rng.index(case.slots.len());
                case.slots.remove(i);
            }
            Mutation::Duplicate => {
                let Some(i) = self.slot(case) else { return false };
                if case.slots.len() >= self.max_slots {
                    return false;
                }
                let slot = case.slots[i].clone();
                case.slots.insert(i + 1, slot);
            }
            Mutation::Swap => {
                if case.slots.len() < 2 {
                    return false;
                }
                let (a, b) = (self.rng.index(case.slots.len()), self.rng.index(case.slots.len()));
                case.slots.swap(a, b);
            }
            Mutation::Splice => {
                let Some(donor) = donor.filter(|d| !d.slots.is_empty()) else { return false };
                if self.rng.chance(50) || case.slots.is_empty() {
                    // Crossover: a prefix of this case and a suffix of the donor.
                    let cut = self.rng.index(case.slots.len() + 1);
                    let from = self.rng.index(donor.slots.len());
                    case.slots.truncate(cut);
                    case.slots.extend(donor.slots[from..].iter().cloned());
                } else {
                    // A run of donor slots inserted here.
                    let from = self.rng.index(donor.slots.len());
                    let len = 1 + self.rng.index(3.min(donor.slots.len() - from));
                    let at = self.rng.index(case.slots.len() + 1);
                    case.slots.splice(at..at, donor.slots[from..from + len].iter().cloned());
                }
                case.slots.truncate(self.max_slots);
            }
            Mutation::Tweak => {
                let Some(i) = self.slot(case) else { return false };
                let tweak = self.tweak();
                let tweaks = &mut case.slots[i].tweaks;
                if tweaks.len() >= 4 {
                    tweaks.remove(0);
                }
                tweaks.push(tweak);
            }
            Mutation::Code => {
                let Some(i) = self.slot(case) else { return false };
                let edit = self.code_edit();
                match &mut case.slots[i].kind {
                    SlotKind::Mutant { edits, calls, .. } => {
                        if edits.len() >= 16 || self.rng.chance(15) {
                            *calls = 1 + self.rng.below(3) as u8;
                        } else {
                            edits.push(edit);
                        }
                    }
                    kind => {
                        *kind = SlotKind::Mutant {
                            source: if self.rng.chance(50) {
                                0
                            } else {
                                self.rng.range(1, 255) as u8
                            },
                            edits: vec![edit],
                            calls: 1 + self.rng.below(2) as u8,
                        };
                    }
                }
            }
            Mutation::L1 => {
                if case.l1.len() >= 4 {
                    case.l1.remove(0);
                }
                let value = self.edge();
                case.l1.push(L1Tweak { field: self.rng.below(12) as u8, value });
            }
            Mutation::Rebase => case.index = self.rng.below(self.index_space),
            Mutation::Native => {
                let Some(i) = self.slot(case) else { return false };
                let slot = &mut case.slots[i];
                match &mut slot.kind {
                    SlotKind::NativeAbi { target, selector, caller, mutation } => {
                        match self.rng.below(6) {
                            0 => *selector = self.rng.below(1 << 16) as u16,
                            1 => *caller = self.rng.below(256) as u8,
                            2 => *mutation = self.rng.below(256) as u8,
                            3 => *target = self.rng.below(256) as u8,
                            // Neighbouring functions of the same family.
                            4 => *selector = selector.wrapping_add(1),
                            _ => slot.salt = self.rng.next_u64(),
                        }
                    }
                    kind => *kind = self.native_abi(),
                }
            }
        }
        true
    }

    const fn native_abi(&mut self) -> SlotKind {
        SlotKind::NativeAbi {
            target: self.rng.below(256) as u8,
            selector: self.rng.below(1 << 16) as u16,
            caller: self.rng.below(256) as u8,
            mutation: self.rng.below(256) as u8,
        }
    }

    fn kind(&mut self) -> SlotKind {
        match self.rng.weighted(&[10, 4, 8, 3, 6, 6, 3, 4, 2, 2, 8, 2, 2, 2, 2]) {
            0 => SlotKind::Random,
            1 => SlotKind::Transfer,
            2 => SlotKind::ProgramCall,
            3 => SlotKind::DeployProgram,
            4 => SlotKind::Precompile,
            5 => SlotKind::Native,
            6 => SlotKind::System,
            7 => SlotKind::Delegation,
            8 => SlotKind::Deposit,
            9 => SlotKind::Invalid,
            10 => self.native_abi(),
            11 => SlotKind::Deep,
            12 => SlotKind::Create,
            13 => SlotKind::Destruct,
            _ => SlotKind::Refund,
        }
    }

    /// A value biased towards boundaries, with small and large magnitudes.
    fn edge(&mut self) -> U256 {
        match self.rng.below(4) {
            0 => U256::from(self.rng.below(4)),
            1 => U256::from(u32::MAX) - U256::from(self.rng.below(2)),
            2 => U256::from(u64::MAX) - U256::from(self.rng.below(2)),
            _ => self.rng.edge_u256(),
        }
    }

    fn tweak(&mut self) -> Tweak {
        let tx = self.rng.below(4) as u8;
        match self.rng.weighted(&[10, 3, 8, 3, 1]) {
            0 => Tweak::Gas {
                tx,
                to: match self.rng.below(6) {
                    0 => GasTarget::IntrinsicPlus(*self.rng.pick(&[0, 1, 2_300, 5_000, 21_000])),
                    1 => GasTarget::IntrinsicPlus(self.rng.below(60_000)),
                    2 => GasTarget::Minus(self.rng.range(1, 3_000)),
                    3 => GasTarget::PerMille(self.rng.range(300, 999) as u16),
                    4 => GasTarget::PerMille(self.rng.range(1001, 4000) as u16),
                    _ => GasTarget::Exact(*self.rng.pick(&[1 << 24, (1 << 24) + 1, 30_000_000])),
                },
            },
            1 => Tweak::Value {
                tx,
                value: match self.rng.below(3) {
                    0 => U256::ZERO,
                    1 => U256::from(1),
                    _ => self.edge(),
                },
            },
            2 => Tweak::Calldata {
                tx,
                edit: match self.rng.below(5) {
                    0 | 1 => self.word_edit(),
                    2 => ByteEdit::Flip {
                        pos: self.rng.next_u64() as u32,
                        bit: self.rng.below(8) as u8,
                    },
                    3 => ByteEdit::Truncate { len: self.rng.next_u64() as u32 },
                    _ => ByteEdit::Append { bytes: Bytes::from(self.rng.bytes_between(1, 64)) },
                },
            },
            3 => Tweak::Fees {
                tx,
                to: *self.rng.pick(&[
                    FeeTarget::AtBaseFee,
                    FeeTarget::BelowBaseFee,
                    FeeTarget::TipIsMax,
                    FeeTarget::TipAboveMax,
                    FeeTarget::Huge,
                ]),
            },
            _ => Tweak::Mint { tx, mint: *self.rng.pick(&[0, 1, u128::from(u64::MAX), u128::MAX]) },
        }
    }

    /// A word write at a word boundary of program calldata (offset `32k`) or of ABI calldata
    /// (offset `4 + 32k`).
    fn word_edit(&mut self) -> ByteEdit {
        let word = self.rng.below(18) as u32;
        let at = if self.rng.chance(50) { 32 * word } else { 4 + 32 * word };
        ByteEdit::Word { at, word: self.rng.edge_u256() }
    }

    fn code_edit(&mut self) -> CodeEdit {
        let pos = self.rng.next_u64() as u32;
        match self.rng.below(6) {
            0..=2 => CodeEdit::Set { pos, byte: self.rng.below(256) as u8 },
            3 => {
                // A PUSH of a boundary value or a short run of random opcodes.
                let bytes = if self.rng.chance(50) {
                    let mut bytes = vec![0x7f];
                    bytes.extend(self.edge().to_be_bytes::<32>());
                    bytes
                } else {
                    self.rng.bytes_between(1, 6)
                };
                CodeEdit::Insert { pos, bytes: bytes.into() }
            }
            _ => CodeEdit::Delete { pos, len: self.rng.range(1, 4) as u8 },
        }
    }
}
