use core::fmt;
use std::collections::VecDeque;

use bitvec::vec::BitVec;
use oxc_index::{Idx, IndexVec, index_vec};

use crate::bytecode::{Block, Bytecode, Inst, InstFlags};

/// Memory-size facts before a memory access.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MemorySection {
    /// A lower bound on memory size before the instruction executes.
    pub(crate) known_size: u64,
    /// The exact memory size required by this instruction, if known.
    pub(crate) required_size: u64,
    /// The exact size to pass directly to `mresize`.
    ///
    /// This is non-zero only when analysis proves that memory is smaller than this instruction's
    /// exact required size before the instruction executes, so codegen can call `mresize` without
    /// first loading and comparing `ecx.mem_len`.
    pub(crate) direct_resize_size: u64,
}

impl fmt::Debug for MemorySection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            f.write_str("MemorySection::EMPTY")
        } else {
            f.debug_struct("MemorySection")
                .field("known_size", &self.known_size)
                .field("required_size", &self.required_size)
                .field("direct_resize_size", &self.direct_resize_size)
                .finish()
        }
    }
}

impl MemorySection {
    /// Returns `true` if the section is empty.
    #[inline]
    pub(crate) fn is_empty(self) -> bool {
        self == Self::default()
    }
}

#[derive(Clone)]
struct IndexBitSet<I: Idx> {
    bits: BitVec,
    _marker: std::marker::PhantomData<fn() -> I>,
}

impl<I: Idx> IndexBitSet<I> {
    fn new(len: usize) -> Self {
        Self { bits: BitVec::repeat(false, len), _marker: std::marker::PhantomData }
    }

    fn insert(&mut self, index: I) {
        self.bits.set(index.index(), true);
    }

    fn remove(&mut self, index: I) {
        self.bits.set(index.index(), false);
    }

    fn contains(&self, index: I) -> bool {
        self.bits[index.index()]
    }

    fn iter(&self) -> impl Iterator<Item = I> + '_ {
        self.bits.iter_ones().map(I::from_usize)
    }
}

/// FIFO worklist with deduplication.
struct Worklist {
    queue: VecDeque<Block>,
    in_queue: IndexBitSet<Block>,
}

impl Worklist {
    fn new(size: usize) -> Self {
        Self { queue: VecDeque::new(), in_queue: IndexBitSet::new(size) }
    }

    fn push(&mut self, id: Block) {
        if !self.in_queue.contains(id) {
            self.in_queue.insert(id);
            self.queue.push_back(id);
        }
    }

    fn pop(&mut self) -> Option<Block> {
        let id = self.queue.pop_front()?;
        self.in_queue.remove(id);
        Some(id)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct BlockMemoryState {
    /// Lower-bound memory size known at block entry.
    known_size: Option<u64>,
    /// Exact memory size known at block entry.
    ///
    /// This is dropped to `None` at joins with different predecessor sizes, loops, and unknown
    /// accesses. It is used only to prove direct `mresize` calls.
    exact_known_size: Option<u64>,
}

impl BlockMemoryState {
    fn join(&mut self, incoming: Self) -> bool {
        let old = *self;
        *self = join_state(*self, incoming);
        *self != old
    }
}

/// Memory section analysis state.
pub(crate) struct MemorySectionAnalysis {
    /// Minimum memory size required by all constant-size accesses in each block.
    block_min_required_sizes: IndexVec<Block, u64>,
    /// Exact memory size required by all accesses in each block, if all are known.
    block_exact_required_sizes: IndexVec<Block, Option<u64>>,
    /// Memory facts known at each block entry.
    block_entry_states: IndexVec<Block, BlockMemoryState>,
    dynamic_jump_blocks: IndexBitSet<Block>,
    dynamic_jump_targets: IndexBitSet<Block>,
    sections: IndexVec<Inst, MemorySection>,
}

impl MemorySectionAnalysis {
    pub(crate) fn new(bytecode: &Bytecode<'_>) -> Self {
        let mut dynamic_jump_blocks = IndexBitSet::new(bytecode.cfg.blocks.len());
        let mut dynamic_jump_targets = IndexBitSet::new(bytecode.cfg.blocks.len());
        if bytecode.has_dynamic_jumps() {
            for bid in bytecode.cfg.blocks.indices() {
                let block = &bytecode.cfg.blocks[bid];
                let head = bytecode.inst(block.insts.start);
                if head.is_reachable_jumpdest(true) {
                    dynamic_jump_targets.insert(bid);
                }

                let term = bytecode.inst(block.terminator());
                if term.is_jump()
                    && !term.is_static_jump()
                    && !term.flags.contains(InstFlags::INVALID_JUMP)
                {
                    dynamic_jump_blocks.insert(bid);
                }
            }
        }

        Self {
            block_min_required_sizes: index_vec![0; bytecode.cfg.blocks.len()],
            block_exact_required_sizes: index_vec![Some(0); bytecode.cfg.blocks.len()],
            block_entry_states: index_vec![BlockMemoryState::default(); bytecode.cfg.blocks.len()],
            dynamic_jump_blocks,
            dynamic_jump_targets,
            sections: index_vec![MemorySection::default(); bytecode.insts.len()],
        }
    }

    /// Runs memory section analysis.
    pub(crate) fn run(mut self, bytecode: &Bytecode<'_>) -> IndexVec<Inst, MemorySection> {
        self.compute_block_memory_summaries(bytecode);
        self.compute_block_entry_states(bytecode);
        self.save_sections(bytecode);
        self.sections
    }

    fn compute_block_memory_summaries(&mut self, bytecode: &Bytecode<'_>) {
        for bid in bytecode.cfg.blocks.indices() {
            let block = &bytecode.cfg.blocks[bid];
            let mut min_required_size = 0;
            let mut exact_required_size = Some(0);
            for inst in block.insts() {
                for (offset, len) in bytecode.const_memory_accesses(inst).into_iter().flatten() {
                    min_required_size =
                        min_required_size.max(min_memory_size_for_access(offset, len));
                    exact_required_size = exact_required_size
                        .zip(exact_memory_size_for_access(offset, len))
                        .map(|(block_size, access_size)| block_size.max(access_size));
                }
            }
            self.block_min_required_sizes[bid] = min_required_size;
            self.block_exact_required_sizes[bid] = exact_required_size;
        }
    }

    fn compute_block_entry_states(&mut self, bytecode: &Bytecode<'_>) {
        let entry = Block::from_usize(0);
        self.block_entry_states[entry] =
            BlockMemoryState { known_size: Some(0), exact_known_size: Some(0) };

        let dynamic_jump_targets = self.dynamic_jump_targets.clone();
        let mut dynamic_exit_state = BlockMemoryState::default();

        let mut worklist = Worklist::new(bytecode.cfg.blocks.len());
        worklist.push(entry);

        while let Some(bid) = worklist.pop() {
            let exit_state = self.block_exit_state(bid);
            if exit_state.known_size.is_none() {
                continue;
            }

            for &succ in &bytecode.cfg.blocks[bid].succs {
                self.update_entry_state(succ, exit_state, &mut worklist);
            }
            if self.dynamic_jump_blocks.contains(bid) && dynamic_exit_state.join(exit_state) {
                for succ in dynamic_jump_targets.iter() {
                    self.update_entry_state(succ, dynamic_exit_state, &mut worklist);
                }
            }
        }
    }

    fn update_entry_state(
        &mut self,
        bid: Block,
        incoming: BlockMemoryState,
        worklist: &mut Worklist,
    ) {
        if self.block_entry_states[bid].join(incoming) {
            worklist.push(bid);
        }
    }

    fn block_exit_state(&self, bid: Block) -> BlockMemoryState {
        let state = self.block_entry_states[bid];
        let known_size =
            state.known_size.map(|known_size| known_size.max(self.block_min_required_sizes[bid]));
        let exact_known_size = state
            .exact_known_size
            .zip(self.block_exact_required_sizes[bid])
            .map(|(exact_known_size, required_size)| exact_known_size.max(required_size));
        BlockMemoryState { known_size, exact_known_size }
    }

    fn save_sections(&mut self, bytecode: &Bytecode<'_>) {
        for bid in bytecode.cfg.blocks.indices() {
            let state = self.block_entry_states[bid];
            let Some(mut known_size) = state.known_size else { continue };
            let mut exact_known_size = state.exact_known_size;
            let block = &bytecode.cfg.blocks[bid];

            for inst in block.insts() {
                for (offset, len) in bytecode.const_memory_accesses(inst).into_iter().flatten() {
                    let exact_required_size = exact_memory_size_for_access(offset, len);
                    let min_required_size = min_memory_size_for_access(offset, len);
                    trace!(
                        %bid,
                        %inst,
                        pc = bytecode.pc(inst),
                        opcode = %bytecode.inst(inst).to_op(),
                        ?offset,
                        ?len,
                        known_size,
                        min_required_size,
                        ?exact_required_size,
                        "memory access"
                    );
                    if known_size != 0 || exact_required_size.is_some_and(|size| size != 0) {
                        let section = &mut self.sections[inst];
                        if section.known_size == 0 && section.required_size == 0 {
                            section.known_size = known_size;
                        }
                        if let Some(exact_required_size) = exact_required_size {
                            section.required_size = section.required_size.max(exact_required_size);
                            if exact_known_size.is_some_and(|size| size < exact_required_size) {
                                section.direct_resize_size =
                                    section.direct_resize_size.max(exact_required_size);
                            }
                        }
                    }
                    known_size = known_size.max(min_required_size);
                    exact_known_size = exact_known_size.zip(exact_required_size).map(
                        |(exact_known_size, exact_required_size)| {
                            exact_known_size.max(exact_required_size)
                        },
                    );
                }
            }
        }
    }
}

fn join_state(
    entry_state: BlockMemoryState,
    pred_exit_state: BlockMemoryState,
) -> BlockMemoryState {
    let Some(pred_known_size) = pred_exit_state.known_size else { return entry_state };
    let Some(entry_known_size) = entry_state.known_size else { return pred_exit_state };

    let known_size = Some(entry_known_size.min(pred_known_size));
    let exact_known_size = match (entry_state.exact_known_size, pred_exit_state.exact_known_size) {
        (Some(entry_size), Some(pred_size)) if entry_size == pred_size => Some(entry_size),
        _ => None,
    };
    BlockMemoryState { known_size, exact_known_size }
}

#[inline]
fn exact_memory_size_for_access(offset: Option<u64>, len: Option<u64>) -> Option<u64> {
    if matches!(len, Some(0)) {
        return Some(0);
    }
    let size = offset?.saturating_add(len?);
    Some(round_memory_size(size))
}

fn min_memory_size_for_access(offset: Option<u64>, len: Option<u64>) -> u64 {
    match (offset, len) {
        (_, Some(0) | None) => 0,
        (Some(offset), Some(len)) => round_memory_size(offset.saturating_add(len)),
        (None, Some(len)) => round_memory_size(len),
    }
}

#[inline]
fn round_memory_size(size: u64) -> u64 {
    size.saturating_add(31) / 32 * 32
}

#[cfg(test)]
mod tests {
    use revm_bytecode::opcode as op;
    use revm_primitives::hardfork::SpecId;

    use super::{MemorySection, exact_memory_size_for_access, min_memory_size_for_access};
    use crate::bytecode::{
        Bytecode, Inst,
        passes::block_analysis::tests::{analyze_asm, analyze_asm_spec},
    };

    fn nth_inst(bytecode: &Bytecode<'_>, opcode: u8, n: usize) -> Inst {
        bytecode
            .iter_all_insts()
            .filter_map(|(inst, data)| (data.opcode == opcode).then_some(inst))
            .nth(n)
            .unwrap()
    }

    fn section(bytecode: &Bytecode<'_>, inst: Inst) -> MemorySection {
        bytecode.memory_section(inst)
    }

    fn assert_section(bytecode: &Bytecode<'_>, inst: Inst, known_size: u64, required_size: u64) {
        let direct_resize_size = if known_size < required_size { required_size } else { 0 };
        assert_eq!(
            section(bytecode, inst),
            MemorySection { known_size, required_size, direct_resize_size }
        );
    }

    #[test]
    fn exact_memory_size_rounds_up_to_words() {
        assert_eq!(exact_memory_size_for_access(Some(0), Some(0)), Some(0));
        assert_eq!(exact_memory_size_for_access(Some(0), Some(1)), Some(32));
        assert_eq!(exact_memory_size_for_access(Some(1), Some(32)), Some(64));
        assert_eq!(exact_memory_size_for_access(Some(64), Some(32)), Some(96));
    }

    #[test]
    fn unknown_offset_or_len_has_no_exact_required_size() {
        assert_eq!(exact_memory_size_for_access(None, Some(32)), None);
        assert_eq!(exact_memory_size_for_access(Some(32), None), None);
        assert_eq!(exact_memory_size_for_access(None, None), None);
        assert_eq!(exact_memory_size_for_access(None, Some(0)), Some(0));
    }

    #[test]
    fn unknown_accesses_have_conservative_min_required_size() {
        assert_eq!(min_memory_size_for_access(None, Some(32)), 32);
        assert_eq!(min_memory_size_for_access(Some(32), None), 0);
        assert_eq!(min_memory_size_for_access(None, None), 0);
        assert_eq!(min_memory_size_for_access(None, Some(0)), 0);
    }

    #[test]
    fn same_block_accesses_use_previous_known_size() {
        let bytecode = analyze_asm("PUSH0 PUSH 64 MSTORE PUSH0 MLOAD STOP");
        let mstore = nth_inst(&bytecode, op::MSTORE, 0);
        let mload = nth_inst(&bytecode, op::MLOAD, 0);

        assert_section(&bytecode, mstore, 0, 96);
        assert_section(&bytecode, mload, 96, 32);
    }

    #[test]
    fn known_size_propagates_across_blocks() {
        let bytecode = analyze_asm(
            "
            PUSH0
            PUSH 64
            MSTORE
            PUSH %target
            JUMP
        target:
            JUMPDEST
            PUSH0
            MLOAD
            STOP
        ",
        );
        let mload = nth_inst(&bytecode, op::MLOAD, 0);
        assert_section(&bytecode, mload, 96, 32);
    }

    #[test]
    fn exact_size_propagates_across_blocks() {
        let bytecode = analyze_asm(
            "
            PUSH0
            PUSH 64
            MSTORE
            PUSH %target
            JUMP
        target:
            JUMPDEST
            PUSH0
            PUSH 128
            MSTORE
            STOP
        ",
        );
        let second_mstore = nth_inst(&bytecode, op::MSTORE, 1);
        assert_section(&bytecode, second_mstore, 96, 160);
    }

    #[test]
    fn branch_join_uses_minimum_predecessor_size() {
        let bytecode = analyze_asm(
            "
            PUSH0
            PUSH 64
            MSTORE
            CALLDATASIZE
            PUSH %large
            JUMPI
            PUSH %join
            JUMP
        large:
            JUMPDEST
            PUSH0
            PUSH 128
            MSTORE
        join:
            JUMPDEST
            PUSH0
            MLOAD
            STOP
        ",
        );
        let mload = nth_inst(&bytecode, op::MLOAD, 0);
        assert_section(&bytecode, mload, 96, 32);
    }

    #[test]
    fn loop_backedge_does_not_inflate_entry_known_size() {
        let bytecode = analyze_asm(
            "
        loop:
            JUMPDEST
            PUSH 128
            MLOAD
            PUSH0
            PUSH 49984
            MSTORE
            PUSH %loop
            JUMP
        ",
        );
        let mload = nth_inst(&bytecode, op::MLOAD, 0);
        let mstore = nth_inst(&bytecode, op::MSTORE, 0);

        assert_eq!(
            section(&bytecode, mload),
            MemorySection { known_size: 0, required_size: 160, direct_resize_size: 0 }
        );
        assert_eq!(
            section(&bytecode, mstore),
            MemorySection { known_size: 160, required_size: 50016, direct_resize_size: 0 }
        );
    }

    #[test]
    fn unknown_nonzero_access_does_not_create_exact_section() {
        let bytecode = analyze_asm(
            "
            PUSH0
            PUSH 64
            MSTORE
            PUSH0
            CALLDATALOAD
            PUSH0
            SWAP1
            MSTORE
            STOP
        ",
        );
        let dynamic_mstore = nth_inst(&bytecode, op::MSTORE, 1);

        assert_eq!(bytecode.const_memory_access(dynamic_mstore), Some((None, Some(32))));
        assert_section(&bytecode, dynamic_mstore, 96, 0);
    }

    #[test]
    fn unknown_offset_known_len_propagates_minimum_size() {
        let bytecode = analyze_asm(
            "
            PUSH0
            CALLDATALOAD
            PUSH0
            SWAP1
            MSTORE
            PUSH0
            MLOAD
            STOP
        ",
        );
        let mload = nth_inst(&bytecode, op::MLOAD, 0);

        assert_section(&bytecode, mload, 32, 32);
    }

    #[test]
    fn zero_len_access_is_exact_even_with_unknown_offset() {
        let bytecode = analyze_asm("PUSH0 PUSH0 PUSH0 CALLDATALOAD CALLDATACOPY STOP");
        let copy = nth_inst(&bytecode, op::CALLDATACOPY, 0);

        assert_eq!(bytecode.const_memory_access(copy), Some((None, Some(0))));
        assert_eq!(section(&bytecode, copy), MemorySection::default());
    }

    #[test]
    fn log0_uses_memory_offset_and_len() {
        let bytecode = analyze_asm("PUSH 32 PUSH 64 LOG0 STOP");
        let log0 = nth_inst(&bytecode, op::LOG0, 0);

        assert_eq!(bytecode.const_memory_access(log0), Some((Some(64), Some(32))));
        assert_section(&bytecode, log0, 0, 96);
    }

    #[test]
    fn log_topics_do_not_affect_memory_range() {
        let bytecode = analyze_asm("PUSH 2048 PUSH0 PUSH0 PUSH0 PUSH0 LOG3 STOP");
        let log3 = nth_inst(&bytecode, op::LOG3, 0);

        assert_eq!(bytecode.const_memory_access(log3), Some((Some(0), Some(0))));
        assert_eq!(section(&bytecode, log3), MemorySection::default());
    }

    #[test]
    fn call_tracks_input_and_output_memory_ranges() {
        let bytecode = analyze_asm(
            "
            PUSH 64     ; out len
            PUSH 96     ; out offset
            PUSH 32     ; in len
            PUSH 0      ; in offset
            PUSH 0      ; value
            PUSH 1      ; to
            PUSH 50000  ; gas
            CALL
            STOP
        ",
        );
        let call = nth_inst(&bytecode, op::CALL, 0);

        assert_eq!(
            bytecode.const_memory_accesses(call),
            [Some((Some(0), Some(32))), Some((Some(96), Some(64)))]
        );
        assert_section(&bytecode, call, 0, 160);
    }

    #[test]
    fn staticcall_tracks_input_and_output_memory_ranges() {
        let bytecode = analyze_asm(
            "
            PUSH 32     ; out len
            PUSH 128    ; out offset
            PUSH 64     ; in len
            PUSH 0      ; in offset
            PUSH 1      ; to
            PUSH 50000  ; gas
            STATICCALL
            STOP
        ",
        );
        let call = nth_inst(&bytecode, op::STATICCALL, 0);

        assert_eq!(
            bytecode.const_memory_accesses(call),
            [Some((Some(0), Some(64))), Some((Some(128), Some(32)))]
        );
        assert_section(&bytecode, call, 0, 160);
    }

    #[test]
    fn create_tracks_initcode_memory_range() {
        let bytecode = analyze_asm("PUSH 32 PUSH 64 PUSH0 CREATE STOP");
        let create = nth_inst(&bytecode, op::CREATE, 0);

        assert_eq!(bytecode.const_memory_access(create), Some((Some(64), Some(32))));
        assert_section(&bytecode, create, 0, 96);
    }

    #[test]
    fn disabled_memory_opcode_is_ignored() {
        let bytecode = analyze_asm_spec("PUSH 32 PUSH0 PUSH0 MCOPY STOP", SpecId::SHANGHAI);
        let mcopy = nth_inst(&bytecode, op::MCOPY, 0);

        assert_eq!(bytecode.const_memory_access(mcopy), None);
        assert_eq!(section(&bytecode, mcopy), MemorySection::default());
    }
}
