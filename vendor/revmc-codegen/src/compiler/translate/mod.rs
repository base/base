//! EVM to IR translation.

use std::mem;

use oxc_index::IndexVec;
use revm_bytecode::opcode as op;
use revm_interpreter::{InputsImpl, InstructionResult};
use revm_primitives::U256;
use revmc_backend::{Attribute, BackendTypes, FunctionAttributeLocation, Pointer, TypeMethods};
use revmc_builtins::{Builtin, Builtins, CallKind, CreateKind};

use super::default_attrs;
use crate::{
    Backend, Builder, Bytecode, EvmContext, Inst, InstData, InstFlags, IntCC, Result, StackSection,
    decode_pair, decode_single,
};

mod peephole;

mod vstack;
use vstack::{VSlot, VStack};

const STACK_CAP: usize = 1024;
// const WORD_SIZE: usize = 32;

#[derive(Clone, Copy, Debug)]
pub(super) struct FcxConfig {
    pub(super) comments: bool,
    pub(super) debug_assertions: bool,
    pub(super) frame_pointers: bool,

    pub(super) debug: bool,
    pub(super) inspect_stack: bool,
    pub(super) stack_bound_checks: bool,
    pub(super) gas_metering: bool,
    pub(super) single_error: bool,
}

impl Default for FcxConfig {
    fn default() -> Self {
        Self {
            debug_assertions: cfg!(debug_assertions),
            comments: false,
            frame_pointers: cfg!(debug_assertions) || cfg!(force_frame_pointers),
            debug: false,
            inspect_stack: false,
            stack_bound_checks: true,
            gas_metering: true,
            single_error: true,
        }
    }
}

/// A list of incoming values for a block. Represents a `phi` node.
type Incoming<B> = Vec<(<B as BackendTypes>::Value, <B as BackendTypes>::BasicBlock)>;

/// A list of `switch` targets.
#[allow(dead_code)]
type SwitchTargets<B> = Vec<(u64, <B as BackendTypes>::BasicBlock)>;

pub(super) struct FunctionCx<'a, B: Backend> {
    // Configuration.
    config: FcxConfig,

    /// The backend's function builder.
    bcx: B::Builder<'a>,

    // Common types.
    isize_type: B::Type,
    word_type: B::Type,
    address_type: B::Type,
    i8_type: B::Type,

    // Locals.
    /// The stack length. Either passed in the arguments as a pointer or allocated locally.
    stack_len: Pointer<B::Builder<'a>>,
    /// The stack value. Constant throughout the function, either passed in the arguments as a
    /// pointer or allocated locally.
    stack: Pointer<B::Builder<'a>>,
    /// The stack argument pointer. Only used when `local_stack` is enabled and the stack needs
    /// to be copied in/out at entry/exit boundaries.
    sp_arg: Option<B::Value>,
    /// The EVM context. Opaque pointer, only passed to builtins.
    ecx: B::Value,
    /// Stack length before the current instruction.
    len_before: B::Value,
    /// Stack length offset for the current instruction, used for push/pop.
    len_offset: i8,

    /// Section-local virtual stack that caches values as SSA instead of
    /// immediately storing/loading from the stack alloca.
    vstack: VStack<B::Value>,
    /// Stack length at the start of the current stack section, loaded once from the alloca.
    /// All intra-section `len_before` values are derived from this + `section_len_offset`.
    section_start_len: B::Value,
    /// Stack pointer at the start of the current stack section (`&stack[section_start_len]`).
    /// All intra-section stack pointer GEPs are derived from this base to preserve pointer
    /// provenance, which lets LLVM prove aliasing and fold redundant operations.
    section_start_sp: B::Value,
    /// Cumulative stack diff from the section start to the current instruction (compile-time).
    /// Updated after the opcode handler so that push/pop/sp helpers see the pre-diff value.
    section_len_offset: i32,
    /// The cumulative offset that `len.addr` currently holds relative to `section_start_len`.
    /// At section start this is 0 (len.addr == section_start_len). After each store it becomes
    /// `section_len_offset + diff`. Stores are skipped when the new offset matches this value.
    stored_len_offset: i32,
    cached_mem_base: Option<B::Value>,

    /// The bytecode being translated.
    bytecode: &'a Bytecode<'a>,
    /// Instruction index to 1-based line number in bytecode.txt (for debug info).
    inst_lines: IndexVec<Inst, u32>,
    /// All entry blocks for each instruction.
    inst_entries: IndexVec<Inst, B::BasicBlock>,
    /// The current instruction being translated.
    current_inst: Option<Inst>,

    // Basic blocks are `None` when outside of a main function.
    /// `dynamic_jump_table` incoming values.
    incoming_dynamic_jumps: Incoming<B>,
    /// The dynamic jump table block where all dynamic jumps branch to.
    dynamic_jump_table: B::BasicBlock,

    /// `failure_block` incoming values.
    incoming_failures: Incoming<B>,
    /// The block that all failures branch to.
    failure_block: Option<B::BasicBlock>,
    /// `return_block` incoming values.
    incoming_returns: Incoming<B>,
    /// The return block that all return instructions branch to.
    return_block: Option<B::BasicBlock>,

    /// `resume_block` switch values.
    resume_blocks: Vec<B::BasicBlock>,
    /// `suspend_block` incoming values.
    suspend_blocks: Incoming<B>,
    /// The suspend block that all suspend instructions branch to.
    suspend_block: B::BasicBlock,

    /// Builtins.
    builtins: &'a mut Builtins<B>,
}

impl<'a, B: Backend> FunctionCx<'a, B> {
    /// Translates an EVM bytecode into a native function.
    ///
    /// Example pseudo-code:
    ///
    /// ```ignore (pseudo-code)
    /// // `cfg(may_suspend) = bytecode.may_suspend()`: `true` if it contains a
    /// // `*CALL*` or `CREATE*` instruction.
    /// fn evm_bytecode(args: ...) {
    ///     setup_locals();
    ///
    ///     #[cfg(debug_assertions)]
    ///     if args.<ptr>.is_null() { panic!("...") };
    ///
    ///     load_arguments();
    ///
    ///     #[cfg(may_suspend)]
    ///     resume: {
    ///         goto match ecx.resume_at {
    ///             0 => inst0,
    ///             1 => first_call_or_create_inst + 1, // + 1 as in the block after.
    ///             2 => second_call_or_create_inst + 1,
    ///             ... => ...,
    ///             _ => unreachable, // Assumed to be valid.
    ///         };
    ///     };
    ///
    ///     op.inst0: { /* ... */ };
    ///     op.inst1: { /* ... */ };
    ///     // ...
    ///     #[cfg(may_suspend)]
    ///     first_call_or_create_inst: {
    ///          // ...
    ///          goto suspend(1);
    ///     };
    ///     // ...
    ///     // There will always be at least one diverging instruction.
    ///     op.stop: {
    ///         goto return(InstructionResult::Stop);
    ///     };
    ///
    ///     #[cfg(may_suspend)]
    ///     suspend(resume_at: u32): {
    ///         ecx.resume_at = resume_at;
    ///         goto return(Ok(())); // Caller checks next_action
    ///     };
    ///
    ///     // All paths lead to here.
    ///     return(ir: InstructionResult): {
    ///         #[cfg(inspect_stack)]
    ///         *args.stack_len = stack_len;
    ///         return ir;
    ///     }
    /// }
    /// ```
    #[allow(rustdoc::invalid_rust_codeblocks)] // Syntax highlighting.
    pub(super) fn translate(
        mut bcx: B::Builder<'a>,
        config: FcxConfig,
        builtins: &'a mut Builtins<B>,
        bytecode: &'a Bytecode<'a>,
    ) -> Result<()> {
        let entry_block = bcx.current_block().unwrap();

        // Clear debug location for prologue code.
        if config.debug {
            bcx.clear_debug_location();
        }

        // Get common types.
        let isize_type = bcx.type_ptr_sized_int();
        let i8_type = bcx.type_int(8);
        let word_type = bcx.type_int(256);
        let address_type = bcx.type_int(160);

        // Set up entry block.
        let ecx = bcx.fn_param(0);

        let sp_arg = bcx.fn_param(1);
        // Use a local alloca for the stack to allow the backend to eliminate dead stores to
        // stack slots above `stack_len` at function exit (e.g. `PUSH0 POP`).
        // Disabled when `inspect_stack` is set because the caller observes every store.
        let local_stack = !config.inspect_stack;
        let stack = if local_stack {
            let stack_type = bcx.type_array(word_type, STACK_CAP as u32);
            bcx.new_stack_slot(stack_type, "stack.addr")
        } else {
            Pointer::new_address(word_type, sp_arg)
        };

        let stack_len_arg = bcx.fn_param(2);
        // This is initialized later in `post_entry_block`.
        let stack_len = bcx.new_stack_slot(isize_type, "len.addr");

        // Create all instruction entry blocks. Dead-code instructions map to
        // `unreachable_block`; dedup redirects are resolved at control-flow callsites.
        let unreachable_block = bcx.create_block("unreachable");
        let inst_entries: IndexVec<Inst, _> = bytecode
            .iter_all_insts()
            .map(|(i, data)| {
                if data.is_dead_code() {
                    unreachable_block
                } else {
                    let name = if config.debug { &bytecode.op_block_name(Some(i), "") } else { "" };
                    bcx.create_block(name)
                }
            })
            .collect();
        assert!(!inst_entries.is_empty(), "translating empty bytecode");

        let dynamic_jump_table = bcx.create_block("dynamic_jump_table");
        let suspend_block = bcx.create_block("suspend");
        let failure_block = bcx.create_block("failure");
        let return_block = bcx.create_block("return");

        let section_start_sp = stack.addr(&mut bcx);
        let zero = bcx.iconst(isize_type, 0);
        let mut fx = FunctionCx {
            config,

            isize_type,
            word_type,
            address_type,
            i8_type,
            stack_len,
            stack,
            sp_arg: local_stack.then_some(sp_arg),
            ecx,
            len_before: zero,
            len_offset: 0,
            section_start_len: zero,
            section_start_sp,
            section_len_offset: 0,
            stored_len_offset: 0,
            cached_mem_base: None,
            bcx,

            bytecode,
            inst_lines: if config.debug { bytecode.take_inst_lines() } else { IndexVec::new() },
            inst_entries,
            current_inst: None,

            incoming_dynamic_jumps: Vec::new(),
            dynamic_jump_table,

            incoming_failures: Vec::new(),
            failure_block: Some(failure_block),
            incoming_returns: Vec::new(),
            return_block: Some(return_block),

            resume_blocks: Vec::new(),
            suspend_blocks: Vec::new(),
            suspend_block,

            builtins,

            vstack: VStack::default(),
        };

        // Add debug assertions for the parameters.
        if config.debug_assertions {
            // Assert that the runtime spec_id matches the compilation spec_id.
            let compiled_spec = fx.bcx.iconst(fx.i8_type, bytecode.spec_id as i64);
            let _ = fx.call_builtin(Builtin::AssertSpecId, &[ecx, compiled_spec]);
        }

        // The bytecode is guaranteed to have at least one instruction.
        let first_inst_block = fx.inst_entries[Inst::from_usize(0)];
        let post_entry_block = fx.bcx.create_block_after(entry_block, "entry.post");
        let resume_block = fx.bcx.create_block_after(post_entry_block, "resume");
        fx.bcx.br(post_entry_block);

        // Translate individual instructions into their respective blocks.
        for (inst, _) in bytecode.iter_insts() {
            fx.translate_inst(inst)?;
        }

        // Clear debug location for all synthetic / epilogue blocks.
        if config.debug {
            fx.bcx.clear_debug_location();
        }

        // Finalize the dynamic jump table.
        fx.bcx.switch_to_block(unreachable_block);
        fx.bcx.unreachable();
        if bytecode.has_dynamic_jumps() {
            fx.bcx.switch_to_block(fx.dynamic_jump_table);
            let jumpdests = bytecode.iter_insts().filter(|(_, data)| data.opcode == op::JUMPDEST);
            let targets = jumpdests
                .map(|(inst, data)| (data.jumpdest_pc() as u64, fx.effective_entry(inst)))
                .collect::<Vec<_>>();
            let i64_type = fx.bcx.type_int(64);
            let index = fx.bcx.phi(i64_type, &fx.incoming_dynamic_jumps);
            let invalid_jump = fx.add_invalid_jump();
            fx.bcx.switch(index, invalid_jump, &targets, true);
        } else {
            // No dynamic jumps.
            debug_assert!(fx.incoming_dynamic_jumps.is_empty());
            fx.bcx.switch_to_block(fx.dynamic_jump_table);
            fx.bcx.unreachable();
        }

        // Finalize the suspend and resume blocks. Must come before the return block.
        // Also here is where the stack length is initialized.
        let load_len_at_start = |fx: &mut Self| {
            // Loaded from args only for the config.
            if config.inspect_stack {
                let stack_len = fx.bcx.load(fx.isize_type, stack_len_arg, "stack_len");
                fx.stack_len.store(&mut fx.bcx, stack_len);
                fx.copy_stack_from_arg(stack_len);
            } else {
                fx.stack_len.store_imm(&mut fx.bcx, 0);
            }
        };
        let generate_resume = bytecode.may_suspend();
        if generate_resume {
            let get_ecx_resume_at_ptr = |fx: &mut Self| {
                fx.get_field(
                    fx.ecx,
                    mem::offset_of!(EvmContext<'_>, resume_at),
                    "ecx.resume_at.addr",
                )
            };

            let resume_ty = fx.isize_type;

            // Resume block: load the `resume_at` value and switch to the corresponding block.
            // Invalid values are treated as unreachable.
            {
                // Special-case the no resume case to load 0 into the length if possible.
                let no_resume_block = fx.bcx.create_block_after(resume_block, "no_resume");

                fx.bcx.switch_to_block(post_entry_block);
                let resume_at = get_ecx_resume_at_ptr(&mut fx);
                let resume_at = fx.bcx.load_aligned(resume_ty, resume_at, 1, "ecx.resume_at");
                let no_resume = fx.bcx.icmp_imm(IntCC::Equal, resume_at, 0);
                fx.bcx.brif(no_resume, no_resume_block, resume_block);

                fx.bcx.switch_to_block(no_resume_block);
                load_len_at_start(&mut fx);
                fx.bcx.br(first_inst_block);

                // Dispatch to the resume block.
                fx.bcx.switch_to_block(resume_block);
                let stack_len = fx.bcx.load(fx.isize_type, stack_len_arg, "stack_len");
                fx.stack_len.store(&mut fx.bcx, stack_len);
                fx.copy_stack_from_arg(stack_len);
                let default = fx.bcx.create_block_after(resume_block, "resume_invalid");
                fx.bcx.switch_to_block(default);
                fx.call_panic("invalid `resume_at` value");

                fx.bcx.switch_to_block(resume_block);
                let targets = fx
                    .resume_blocks
                    .iter()
                    .enumerate()
                    .map(|(i, b)| (i as u64 + 1, *b))
                    .collect::<Vec<_>>();
                fx.bcx.switch(resume_at, default, &targets, true);
            }

            // Suspend block: store the `resume_at` value and return `Stop`.
            {
                fx.bcx.switch_to_block(fx.suspend_block);
                let resume_value = fx.bcx.phi(resume_ty, &fx.suspend_blocks);
                let resume_at = get_ecx_resume_at_ptr(&mut fx);
                fx.bcx.store_aligned(resume_value, resume_at, 1);

                // Save stack back to caller only when suspending, or always if inspecting.
                // This matches the inverse of the condition in the return block.
                if !config.inspect_stack {
                    fx.copy_stack_to_arg();
                    fx.save_stack_len();
                }

                fx.build_return_imm(InstructionResult::Stop);
            }
        } else {
            debug_assert!(fx.resume_blocks.is_empty());
            debug_assert!(fx.suspend_blocks.is_empty());

            fx.bcx.switch_to_block(post_entry_block);
            load_len_at_start(&mut fx);
            fx.bcx.br(first_inst_block);

            fx.bcx.switch_to_block(resume_block);
            fx.bcx.unreachable();
            fx.bcx.switch_to_block(fx.suspend_block);
            fx.bcx.unreachable();
        }

        // Finalize the failure block.
        fx.bcx.switch_to_block(fx.failure_block.unwrap());
        if !fx.incoming_failures.is_empty() {
            // Semantically, the EVM has only one halt; the distinct failure
            // `InstructionResult` codes exist purely for debugging and error messages.
            // `single_error` collapses every failure path to a single placeholder
            // (OOG), letting LLVM DCE the per-site materialization and the phi.
            let failure_value = if config.single_error {
                fx.bcx.iconst(fx.i8_type, InstructionResult::OutOfGas as i64)
            } else {
                fx.bcx.phi(fx.i8_type, &fx.incoming_failures)
            };
            fx.bcx.set_current_block_cold();
            // Do not sync the stack here: this is a shared synthetic merge block for
            // failures that may occur before their instruction body has materialized
            // the current virtual stack. Syncing here can use values that do not
            // dominate all failure predecessors.
            fx.build_return_inner(failure_value);
        } else {
            fx.bcx.unreachable();
        }

        // Finalize the return block.
        fx.bcx.switch_to_block(fx.return_block.unwrap());
        if !fx.incoming_returns.is_empty() {
            let return_value = fx.bcx.phi(fx.i8_type, &fx.incoming_returns);
            if config.inspect_stack {
                fx.copy_stack_to_arg();
                fx.save_stack_len();
            }
            fx.bcx.ret(&[return_value]);
        } else {
            fx.bcx.unreachable();
        }

        fx.bcx.seal_all_blocks();

        Ok(())
    }

    /// Returns the instruction that is physically next in bytecode order.
    fn lexical_next_inst(&self, inst: Inst) -> Option<Inst> {
        let next = inst + 1;
        self.inst_entries.get(next)?;
        Some(next)
    }

    /// Resolves an instruction through dead-block redirects created by dedup.
    fn effective_inst(&self, inst: Inst) -> Inst {
        self.bytecode.redirects.get(&inst).copied().unwrap_or(inst)
    }

    /// Returns the IR block for an instruction after applying dedup redirects.
    fn effective_entry(&self, inst: Inst) -> B::BasicBlock {
        self.inst_entries[self.effective_inst(inst)]
    }

    /// Returns the runtime fallthrough target after applying dedup redirects.
    fn effective_next_inst(&self, inst: Inst) -> Option<Inst> {
        self.lexical_next_inst(inst).map(|next| self.effective_inst(next))
    }

    /// Returns the IR block for the runtime fallthrough target.
    fn effective_next_entry(&self, inst: Inst) -> Option<B::BasicBlock> {
        self.effective_next_inst(inst).map(|next| self.effective_entry(next))
    }

    #[instrument(level = "debug", skip_all, fields(inst = %self.bytecode.inst(inst).to_op()))]
    fn translate_inst(&mut self, inst: Inst) -> Result<()> {
        self.current_inst = Some(inst);
        let data = self.bytecode.inst(inst);
        let opcode = data.opcode;
        let entry_block = self.inst_entries[inst];
        self.bcx.switch_to_block(entry_block);

        if self.config.debug {
            self.bcx.set_debug_location(self.inst_lines[inst], 1);
        }

        // self.call_printf(format_printf!("{}\n", self.op_block_name("")), &[]);

        let branch_to_next_opcode = |this: &mut Self| {
            debug_assert!(
                !this.bytecode.is_instr_diverging(inst),
                "attempted to branch to next instruction in a diverging instruction: {data:?}",
            );
            if let Some(next) = this.effective_next_entry(inst) {
                this.bcx.br(next);
            }
        };

        /// Makes sure to run cleanup code and return.
        /// Use `no_branch` to skip the branch to the next opcode.
        /// Use `build` to build the return instruction and skip the branch.
        macro_rules! goto_return {
            (no_branch $($comment:expr)?) => {
                $(
                    if self.config.comments {
                        self.add_comment($comment);
                    }
                )?
                return Ok(());
            };
            (build $ret:expr) => {{
                self.build_return_imm($ret);
                goto_return!(no_branch);
            }};
            (fail $ret:expr) => {{
                self.build_fail_imm($ret);
                goto_return!(no_branch);
            }};
            ($($comment:expr)?) => {
                // Flush virtual stack before leaving the section.
                if let Some(next_inst) = self.effective_next_inst(inst) {
                    let next = self.bytecode.inst(next_inst);
                    if !next.is_dead_code() && next.is_stack_section_head() {
                        self.materialize_live_stack();
                    } else {
                        self.relieve_vstack_pressure();
                    }
                }
                branch_to_next_opcode(self);
                goto_return!(no_branch $($comment)?);
            };
        }

        // Assert that we already skipped the block.
        debug_assert!(!data.flags.contains(InstFlags::DEAD_CODE));

        #[cfg(test)]
        if opcode == crate::TEST_SUSPEND {
            self.suspend();
            goto_return!(no_branch);
        }

        // Disabled instructions don't pay gas.
        if data.flags.contains(InstFlags::DISABLED) {
            goto_return!(fail InstructionResult::NotActivated);
        }
        if data.flags.contains(InstFlags::UNKNOWN) {
            goto_return!(fail InstructionResult::OpcodeNotFound);
        }

        // Pay static gas for the current section.
        self.gas_cost_imm(data.gas_section.gas_cost as u64);

        // Compute len_before for this instruction.
        // At section heads: load from the alloca once and reset the section offset.
        // Within a section: derive from section_start_len + compile-time offset.
        let (inp, out) = data.stack_io();
        let diff = effective_stack_diff(inp, out, data);
        self.len_offset = 0;
        if data.is_stack_section_head() {
            self.section_start_len = self.stack_len.load(&mut self.bcx, "stack_len");
            self.section_start_sp = self.sp_at(self.section_start_len);
            self.section_len_offset = 0;
            self.stored_len_offset = 0;
            self.cached_mem_base = None;

            let section = data.stack_section;
            self.vstack.reset(section.inputs as usize, section.max_growth.max(0) as usize);
        }
        self.len_before = if self.section_len_offset == 0 {
            self.section_start_len
        } else {
            self.bcx.iadd_imm(self.section_start_len, self.section_len_offset as i64)
        };

        // Check stack length for the current section.
        if self.config.stack_bound_checks && data.is_stack_section_head() {
            self.check_stack_bounds(data.stack_section);
        }

        // NOOPs need no codegen beyond gas and bounds checks.
        if data.flags.contains(InstFlags::NOOP) {
            self.sync_noop_diff(inst, diff);
            self.section_len_offset += diff;
            goto_return!("noop");
        }

        // Store the updated stack length. Skip when `len.addr` already holds the
        // correct value, i.e. the offset we'd write matches what's already stored.
        let new_len_offset = self.section_len_offset + diff;
        if new_len_offset != self.stored_len_offset {
            let len_changed = self.bcx.iadd_imm(self.len_before, diff as i64);
            self.stack_len.store(&mut self.bcx, len_changed);
            self.stored_len_offset = new_len_offset;
        }

        // If the output is a known constant and the opcode has no dynamic gas or side effects,
        // skip the real logic and just write the result.
        // The inputs are not loaded; we simply adjust the stack offset to consume them and
        // push the folded constant. This turns e.g. `PUSH 3, PUSH 4, ADD` into a single
        // store of `7`.
        // Pure ops with a known-constant output: skip the opcode logic and just
        // store the folded constant. Works for single-output arithmetic (ADD, MUL,
        // ...) and DUP (which adds one new TOS without consuming its input).
        if out >= 1
            && let Some(const_out) = self.bytecode.const_output(inst)
        {
            debug_assert!(
                out == 1 || out == inp + 1,
                "const_output assumes single synthesized push: inp={inp}, out={out}",
            );
            debug_assert!(!data.may_suspend() && !data.is_branching());
            // We push exactly 1 value, so consume `inp + 1 - out` to match the
            // real stack diff. For DUP (out = inp+1) this is 0; for ADD (out = 1)
            // this equals inp.
            let drop_count = (inp + 1 - out) as usize;
            self.vstack.drop_top(drop_count);
            self.len_offset -= drop_count as i8;
            let value = self.bcx.iconst_256(const_out);
            self.push(value);
            self.section_len_offset += diff;
            goto_return!("const output");
        }

        if self.try_peephole(data) {
            self.sync_virtual_stack_diff(diff);
            self.section_len_offset += diff;
            if self.current_inst().is_diverging() {
                goto_return!(no_branch);
            } else {
                goto_return!("peephole");
            }
        }

        // Macro utils.
        macro_rules! unop {
            ($op:ident) => {{
                let mut a = self.pop();
                a = self.bcx.$op(a);
                self.push(a);
            }};
        }

        macro_rules! binop {
            ($op:ident) => {{
                let [a, b] = self.popn();
                let r = self.bcx.$op(a, b);
                self.push(r);
            }};
            (@shift $op:ident, | $value:ident, $shift:ident | $default:expr) => {{
                let [$shift, $value] = self.popn();
                let r = self.bcx.$op($value, $shift);
                let overflow = self.bcx.icmp_imm(IntCC::UnsignedGreaterThan, $shift, 255);
                let default = $default;
                let r = self.bcx.select(overflow, default, r);
                self.push(r);
            }};
        }

        /// Gets the pointer to a field via chained `offset_of!`.
        macro_rules! field {
            // Gets the pointer to a field.
            (@get $base:expr, $($paths:path),*; $($spec:tt).*) => {
                self.get_field($base, 0 $(+ mem::offset_of!($paths, $spec))*, stringify!($($spec).*.addr))
            };
            // Loads a field.
            // `@[endian]` is the endianness of the stored value. If native, omit it.
            (@load $(@[endian = $endian:tt])? $ty:expr, $base:expr, $($paths:path),*; $($spec:tt).*) => {{
                let ptr = field!(@get $base, $($paths),*; $($spec).*);
                #[allow(unused_mut)]
                let mut value = self.bcx.load_aligned($ty, ptr, 1, stringify!($($spec).*));
                $(
                    if !cfg!(target_endian = $endian) {
                        value = self.bcx.bswap(value);
                    }
                )?
                value
            }};
            // Loads, extends (if necessary), and pushes a field.
            // `@[endian]` is the endianness of the stored value. If native, omit it.
            (@push $(@[endian = $endian:tt])? $ty:expr, $base:expr, $($rest:tt)*) => {{
                let mut value = field!(@load $(@[endian = $endian])? $ty, $base, $($rest)*);
                if self.bcx.type_bit_width($ty) < 256 {
                    value = self.bcx.zext(self.word_type, value);
                }
                self.push(value);
            }};
        }

        match data.opcode {
            op::STOP => goto_return!(build InstructionResult::Stop),

            op::ADD => binop!(iadd),
            op::MUL => binop!(imul),
            op::SUB => binop!(isub),
            op::DIV => {
                let sp = self.sp_after_inputs();
                let _ = self.call_builtin(Builtin::Div, &[sp]);
            }
            op::SDIV => {
                let sp = self.sp_after_inputs();
                let _ = self.call_builtin(Builtin::SDiv, &[sp]);
            }
            op::MOD => {
                let sp = self.sp_after_inputs();
                let _ = self.call_builtin(Builtin::Mod, &[sp]);
            }
            op::SMOD => {
                let sp = self.sp_after_inputs();
                let _ = self.call_builtin(Builtin::SMod, &[sp]);
            }
            op::ADDMOD => {
                let sp = self.sp_after_inputs();
                let _ = self.call_builtin(Builtin::AddMod, &[sp]);
            }
            op::MULMOD => {
                let sp = self.sp_after_inputs();
                let _ = self.call_builtin(Builtin::MulMod, &[sp]);
            }
            op::EXP => {
                let sp = self.sp_after_inputs();
                self.call_fallible_builtin(Builtin::Exp, &[self.ecx, sp]);
            }
            op::SIGNEXTEND => {
                // let shift = 248 - 8 * ext;
                // ext < 31
                //   ? (x << shift) >>s shift
                //   : x
                let [ext, x] = self.popn();

                let might_do_something = self.bcx.icmp_imm(IntCC::UnsignedLessThan, ext, 31);

                let shift = self.bcx.imul_imm(ext, 8);
                let c248 = self.bcx.iconst_256(248);
                let shift = self.bcx.isub(c248, shift);
                let shifted = self.bcx.ishl(x, shift);
                let sext = self.bcx.sshr(shifted, shift);

                let r = self.bcx.select(might_do_something, sext, x);
                self.push(r);
            }

            op::LT | op::GT | op::SLT | op::SGT | op::EQ => {
                let cond = match opcode {
                    op::LT => IntCC::UnsignedLessThan,
                    op::GT => IntCC::UnsignedGreaterThan,
                    op::SLT => IntCC::SignedLessThan,
                    op::SGT => IntCC::SignedGreaterThan,
                    op::EQ => IntCC::Equal,
                    _ => unreachable!(),
                };

                let [a, b] = self.popn();
                let r = self.bcx.icmp(cond, a, b);
                let r = self.bcx.zext(self.word_type, r);
                self.push(r);
            }
            op::ISZERO => {
                let a = self.pop();
                let r = self.bcx.icmp_imm(IntCC::Equal, a, 0);
                let r = self.bcx.zext(self.word_type, r);
                self.push(r);
            }
            op::AND => binop!(bitand),
            op::OR => binop!(bitor),
            op::XOR => binop!(bitxor),
            op::NOT => unop!(bitnot),
            op::BYTE => {
                // index < 32
                //   ? (value >> (248 - index * 8)) & 0xFF
                //   : 0
                let [index, value] = self.popn();

                let in_range = self.bcx.icmp_imm(IntCC::UnsignedLessThan, index, 32);

                let shift = self.bcx.imul_imm(index, 8);
                let c248 = self.bcx.iconst_256(248);
                let shift = self.bcx.isub(c248, shift);
                let shifted = self.bcx.ushr(value, shift);
                let mask = self.bcx.iconst_256(0xFF);
                let byte = self.bcx.bitand(shifted, mask);

                let zero = self.bcx.iconst_256(0);

                let r = self.bcx.select(in_range, byte, zero);
                self.push(r);
            }
            op::SHL => binop!(@shift ishl, |value, shift| self.bcx.iconst_256(0)),
            op::SHR => binop!(@shift ushr, |value, shift| self.bcx.iconst_256(0)),
            op::SAR => binop!(@shift sshr, |value, shift| {
                let is_negative = self.bcx.icmp_imm(IntCC::SignedLessThan, value, 0);
                let max = self.bcx.iconst_256(U256::MAX);
                let zero = self.bcx.iconst_256(0);
                self.bcx.select(is_negative, max, zero)
            }),
            op::CLZ => unop!(clz),

            op::KECCAK256 => {
                let sp = self.sp_after_inputs();
                self.call_fallible_builtin(Builtin::Keccak256, &[self.ecx, sp]);
            }

            op::ADDRESS => {
                let input = self.load_input();
                field!(@push @[endian = "big"] self.address_type, input, InputsImpl; target_address);
            }
            op::BALANCE => {
                let sp = self.sp_after_inputs();
                self.call_fallible_builtin(Builtin::Balance, &[self.ecx, sp]);
            }
            op::ORIGIN => {
                let slot = self.sp_at_top();
                let _ = self.call_builtin(Builtin::Origin, &[self.ecx, slot]);
                self.narrow_to_address(slot);
            }
            op::CALLER => {
                let input = self.load_input();
                field!(@push @[endian = "big"] self.address_type, input, InputsImpl; caller_address);
            }
            op::CALLVALUE => {
                let input = self.load_input();
                field!(@push self.word_type, input, InputsImpl; call_value);
            }
            op::CALLDATALOAD => {
                let sp = self.sp_after_inputs();
                let _ = self.call_builtin(Builtin::CallDataLoad, &[self.ecx, sp]);
            }
            op::CALLDATASIZE => {
                field!(@push self.isize_type, self.ecx, EvmContext<'_>; calldatasize);
            }
            op::CALLDATACOPY => {
                let sp = self.sp_after_inputs();
                self.call_fallible_builtin(Builtin::CallDataCopy, &[self.ecx, sp]);
            }
            op::CODESIZE => {
                let len = self.bcx.iconst(self.word_type, self.bytecode.codesize() as i64);
                self.push(len);
            }
            op::CODECOPY => {
                let sp = self.sp_after_inputs();
                self.call_fallible_builtin(Builtin::CodeCopy, &[self.ecx, sp]);
            }

            op::GASPRICE => {
                let sp = self.sp_after_inputs();
                let _ = self.call_builtin(Builtin::GasPrice, &[self.ecx, sp]);
            }
            op::EXTCODESIZE => {
                let sp = self.sp_after_inputs();
                self.call_fallible_builtin(Builtin::ExtCodeSize, &[self.ecx, sp]);
            }
            op::EXTCODECOPY => {
                let sp = self.sp_after_inputs();
                self.call_fallible_builtin(Builtin::ExtCodeCopy, &[self.ecx, sp]);
            }
            op::RETURNDATASIZE => {
                field!(@push self.isize_type, self.ecx, EvmContext<'_>, pf::Slice; return_data.len);
            }
            op::RETURNDATACOPY => {
                let sp = self.sp_after_inputs();
                self.call_fallible_builtin(Builtin::ReturnDataCopy, &[self.ecx, sp]);
            }
            op::EXTCODEHASH => {
                let sp = self.sp_after_inputs();
                self.call_fallible_builtin(Builtin::ExtCodeHash, &[self.ecx, sp]);
            }
            op::BLOCKHASH => {
                let sp = self.sp_after_inputs();
                self.call_fallible_builtin(Builtin::BlockHash, &[self.ecx, sp]);
            }
            op::COINBASE => {
                let slot = self.sp_at_top();
                let _ = self.call_builtin(Builtin::Coinbase, &[self.ecx, slot]);
                self.narrow_to_address(slot);
            }
            op::TIMESTAMP => {
                let slot = self.sp_at_top();
                let _ = self.call_builtin(Builtin::Timestamp, &[self.ecx, slot]);
            }
            op::NUMBER => {
                let slot = self.sp_at_top();
                let _ = self.call_builtin(Builtin::Number, &[self.ecx, slot]);
            }
            op::DIFFICULTY => {
                let slot = self.sp_at_top();
                let _ = self.call_builtin(Builtin::Difficulty, &[self.ecx, slot]);
            }
            op::GASLIMIT => {
                let slot = self.sp_at_top();
                let _ = self.call_builtin(Builtin::GasLimit, &[self.ecx, slot]);
            }
            op::CHAINID => {
                let slot = self.sp_at_top();
                let _ = self.call_builtin(Builtin::ChainId, &[self.ecx, slot]);
            }
            op::SELFBALANCE => {
                let slot = self.sp_at_top();
                self.call_fallible_builtin(Builtin::SelfBalance, &[self.ecx, slot]);
            }
            op::BASEFEE => {
                let slot = self.sp_at_top();
                let _ = self.call_builtin(Builtin::Basefee, &[self.ecx, slot]);
            }
            op::BLOBHASH => {
                let sp = self.sp_after_inputs();
                let _ = self.call_builtin(Builtin::BlobHash, &[self.ecx, sp]);
            }
            op::BLOBBASEFEE => {
                let slot = self.sp_at_top();
                let _ = self.call_builtin(Builtin::BlobBaseFee, &[self.ecx, slot]);
            }
            op::SLOTNUM => {
                let slot = self.sp_at_top();
                let _ = self.call_builtin(Builtin::SlotNum, &[self.ecx, slot]);
            }

            op::POP => {
                self.pop_ignore(1);
            }
            op::MLOAD => {
                let offset = self.pop();
                let addr = self.build_ensure_memory(offset, 32);
                let value = self.bcx.load_aligned(self.word_type, addr, 1, "mload.value");
                let value = if self.little_endian() { self.bcx.bswap(value) } else { value };
                self.push(value);
            }
            op::MSTORE => {
                let [offset, value] = self.popn();
                let addr = self.build_ensure_memory(offset, 32);
                let value = if self.little_endian() { self.bcx.bswap(value) } else { value };
                self.bcx.store_aligned(value, addr, 1);
            }
            op::MSTORE8 => {
                let [offset, value] = self.popn();
                let addr = self.build_ensure_memory(offset, 1);
                let value = self.bcx.ireduce(self.i8_type, value);
                self.bcx.store_aligned(value, addr, 1);
            }
            op::SLOAD => {
                let sp = self.sp_after_inputs();
                self.call_fallible_builtin(Builtin::Sload, &[self.ecx, sp]);
            }
            op::SSTORE => {
                let sp = self.sp_after_inputs();
                self.call_fallible_builtin(Builtin::Sstore, &[self.ecx, sp]);
            }
            op::JUMP | op::JUMPI => {
                let is_invalid = data.flags.contains(InstFlags::INVALID_JUMP);
                let has_const_jumpi_condition = data.has_const_jumpi_condition();
                if is_invalid && opcode == op::JUMP {
                    // Pop and discard the target; it's always on the stack.
                    self.pop_ignore(1);
                    self.build_fail_imm(InstructionResult::InvalidJump);
                } else {
                    let target = if is_invalid {
                        debug_assert_eq!(*data, op::JUMPI);
                        // The jump target is invalid, but we still need to pop it.
                        self.pop_ignore(1);
                        self.add_invalid_jump()
                    } else if data.flags.contains(InstFlags::MULTI_JUMP) {
                        let target_value = self.pop();
                        let targets = self.bytecode.multi_jump_targets(inst).unwrap();

                        if opcode == op::JUMPI && !has_const_jumpi_condition {
                            let cond_word = self.pop();
                            self.materialize_live_stack();
                            let cond = self.bcx.icmp_imm(IntCC::NotEqual, cond_word, 0);
                            let next = self
                                .effective_next_entry(inst)
                                .expect("JUMPI must have a fallthrough target");
                            let switch_block = self.bcx.create_block("multi_jump");
                            self.bcx.brif(cond, switch_block, next);
                            self.bcx.switch_to_block(switch_block);
                        } else {
                            if opcode == op::JUMPI {
                                self.pop_ignore(1);
                            }
                            self.materialize_live_stack();
                        }

                        let switch_targets: Vec<_> = targets
                            .iter()
                            .map(|&t| {
                                let pc = self.bytecode.inst(t).jumpdest_pc() as u64;
                                (pc, self.effective_entry(t))
                            })
                            .collect();
                        let invalid_jump = self.add_invalid_jump();
                        self.bcx.switch(target_value, invalid_jump, &switch_targets, true);

                        goto_return!(no_branch);
                    } else if data.flags.contains(InstFlags::STATIC_JUMP) {
                        // Pop and discard the target; it's always on the stack.
                        self.pop_ignore(1);
                        let target_inst = data.static_jump_target();
                        debug_assert!(
                            *self.bytecode.inst(target_inst) == op::JUMPDEST
                                || (opcode == op::JUMPI && has_const_jumpi_condition),
                            "jumping to non-JUMPDEST; target_inst={target_inst}",
                        );
                        self.effective_entry(target_inst)
                    } else {
                        // Dynamic jump.
                        debug_assert!(self.bytecode.has_dynamic_jumps());
                        let target = self.pop();
                        let target = self.u256_to_u64_saturating(target, 64);
                        self.incoming_dynamic_jumps
                            .push((target, self.bcx.current_block().unwrap()));
                        self.dynamic_jump_table
                    };

                    if opcode == op::JUMPI && !has_const_jumpi_condition {
                        let cond_word = self.pop();
                        // Flush virtual values before leaving the section.
                        self.materialize_live_stack();
                        let cond = self.bcx.icmp_imm(IntCC::NotEqual, cond_word, 0);
                        let next = self
                            .effective_next_entry(inst)
                            .expect("JUMPI must have a fallthrough target");
                        self.bcx.brif(cond, target, next);
                    } else {
                        if opcode == op::JUMPI {
                            self.pop_ignore(1);
                        }
                        // Flush virtual values before leaving the section.
                        self.materialize_live_stack();
                        self.bcx.br(target);
                    }
                }

                goto_return!(no_branch);
            }
            op::PC => {
                let pc = self.bcx.iconst_256(data.pc_imm());
                self.push(pc);
            }
            op::MSIZE => {
                let mem_len_field = self.get_field(
                    self.ecx,
                    mem::offset_of!(EvmContext<'_>, mem_len),
                    "ecx.mem_len.addr",
                );
                let mem_len = self.bcx.load(self.isize_type, mem_len_field, "ecx.mem_len");
                let msize = self.bcx.zext(self.word_type, mem_len);
                self.push(msize);
            }
            op::GAS => {
                let addr = self.gas_remaining_addr();
                let i64_type = self.bcx.type_int(64);
                let remaining = self.bcx.load(i64_type, addr, "gas.remaining");
                let remaining = self.bcx.zext(self.word_type, remaining);
                self.push(remaining);
            }
            op::JUMPDEST => {
                self.bcx.nop();
            }
            op::TLOAD => {
                let sp = self.sp_after_inputs();
                let _ = self.call_builtin(Builtin::Tload, &[self.ecx, sp]);
            }
            op::TSTORE => {
                let sp = self.sp_after_inputs();
                self.call_fallible_builtin(Builtin::Tstore, &[self.ecx, sp]);
            }
            op::MCOPY => {
                let sp = self.sp_after_inputs();
                self.call_fallible_builtin(Builtin::Mcopy, &[self.ecx, sp]);
            }

            op::PUSH0..=op::PUSH32 => {
                unreachable!("handled in const_output");
            }

            op::DUP1..=op::DUP16 => self.dup((opcode - op::DUP1 + 1) as usize),
            op::DUPN => match decode_single(data.imm_byte()) {
                Some(n) => self.dup(n as usize),
                None => goto_return!(fail InstructionResult::InvalidImmediateEncoding),
            },

            op::SWAP1..=op::SWAP16 => self.swap((opcode - op::SWAP1 + 1) as usize),
            op::SWAPN => match decode_single(data.imm_byte()) {
                Some(n) => self.swap(n as usize),
                None => goto_return!(fail InstructionResult::InvalidImmediateEncoding),
            },

            op::EXCHANGE => match decode_pair(data.imm_byte()) {
                Some((n, m)) => self.exchange(n as usize, (m - n) as usize),
                None => goto_return!(fail InstructionResult::InvalidImmediateEncoding),
            },

            op::LOG0..=op::LOG4 => {
                let n = opcode - op::LOG0;
                let sp = self.sp_after_inputs();
                let n = self.bcx.iconst(self.i8_type, n as i64);
                self.call_fallible_builtin(Builtin::Log, &[self.ecx, sp, n]);
            }

            op::CREATE => {
                self.create_common(CreateKind::Create);
                goto_return!(no_branch);
            }
            op::CALL => {
                self.call_common(CallKind::Call);
                goto_return!(no_branch);
            }
            op::CALLCODE => {
                self.call_common(CallKind::CallCode);
                goto_return!(no_branch);
            }
            op::RETURN => {
                self.return_common(InstructionResult::Return);
                goto_return!(no_branch);
            }
            op::DELEGATECALL => {
                self.call_common(CallKind::DelegateCall);
                goto_return!(no_branch);
            }
            op::CREATE2 => {
                self.create_common(CreateKind::Create2);
                goto_return!(no_branch);
            }

            op::STATICCALL => {
                self.call_common(CallKind::StaticCall);
                goto_return!(no_branch);
            }

            op::REVERT => {
                self.return_common(InstructionResult::Revert);
                goto_return!(no_branch);
            }
            op::INVALID => goto_return!(fail InstructionResult::InvalidFEOpcode),
            op::SELFDESTRUCT => {
                let sp = self.sp_after_inputs();
                self.sync_diverging_stack_effect();
                let _ = self.call_builtin(Builtin::SelfDestruct, &[self.ecx, sp]);
                self.bcx.unreachable();
                goto_return!(no_branch);
            }

            _ => unreachable!("unimplemented instruction: {data:?}"),
        }

        self.sync_virtual_stack_diff(diff);
        self.section_len_offset += diff;
        goto_return!("normal exit");
    }

    /// Syncs the virtual stack for a NOOP instruction. Unlike `sync_virtual_stack_diff`,
    /// this preserves known-constant outputs as `Virtual` values so that boundary
    /// materialization (e.g. `materialize_live_stack` before branch/suspend) can flush
    /// them to physical memory. Without this, NOOP'd constants would be marked
    /// `Materialized` with no actual store, leaving garbage in memory.
    fn sync_noop_diff(&mut self, inst: Inst, diff: i32) {
        let expected_top = self.section_len_offset + diff;
        let current_top = self.vstack.top_offset();
        if current_top == expected_top {
            return;
        }
        let delta = expected_top - current_top;
        if delta < 0 {
            self.vstack.drop_top((-delta) as usize);
        } else {
            // For NOOP instructions that push values, use the known constant output
            // if available. This ensures the value can be materialized later.
            if delta == 1
                && let Some(c) = self.bytecode.const_output(inst)
            {
                let value = self.bcx.iconst_256(c);
                self.vstack.push(value);
            } else {
                for _ in 0..delta {
                    self.vstack.push_mem();
                }
            }
        }
    }

    /// Syncs the virtual stack's `top_offset` with the expected value after applying
    /// the instruction's stack diff. For inline ops (push/pop), the virtual stack is
    /// already up to date. For builtin ops (sp_after_inputs/sp_at_top), this adjusts
    /// the virtual stack to account for consumed inputs and materialized outputs.
    ///
    /// Also invalidates any virtual slots in the output area that the builtin may have
    /// overwritten in physical memory, to prevent stale virtual values from shadowing
    /// the builtin's actual output.
    fn sync_virtual_stack_diff(&mut self, diff: i32) {
        let expected_top = self.section_len_offset + diff;
        let current_top = self.vstack.top_offset();
        if current_top == expected_top {
            return;
        }
        let delta = expected_top - current_top;
        if expected_top < self.vstack.live_range().start {
            let inst = self.current_inst.unwrap();
            // Walk backward to find the section head for diagnostic context.
            let mut head = inst;
            for i in (0..inst.index()).rev() {
                let idx = crate::Inst::from_usize(i);
                let d = self.bytecode.inst(idx);
                if d.is_dead_code() {
                    continue;
                }
                if d.is_stack_section_head() {
                    head = idx;
                    break;
                }
            }
            // Dump the section from head to current inst.
            let mut section_dump = String::new();
            for i in head.index()..=inst.index() {
                let idx = crate::Inst::from_usize(i);
                let d = self.bytecode.inst(idx);
                if d.is_dead_code() {
                    continue;
                }
                use std::fmt::Write;
                let _ = write!(
                    section_dump,
                    "\n  ic{i} pc={} {:?} io={:?} flags={:?} gas={:?} stack={:?}{}{}",
                    self.bytecode.pc(idx),
                    d.to_op(),
                    d.stack_io(),
                    d.flags,
                    d.gas_section,
                    d.stack_section,
                    if d.is_stack_section_head() { " SECTION_HEAD" } else { "" },
                    if d.is_dead_code() { " DEAD" } else { "" },
                );
            }
            let head_data = self.bytecode.inst(head);
            panic!(
                "sync: expected_top={expected_top} < base={}, section_len_offset={}, \
                 diff={diff}, current_top={current_top}, inst={:?} (ic{})\n\
                 section head=ic{}, head_stack_section={:?}, section:{section_dump}",
                self.vstack.live_range().start,
                self.section_len_offset,
                self.current_inst().to_op(),
                inst.index(),
                head.index(),
                head_data.stack_section,
            );
        }
        if delta < 0 {
            self.vstack.drop_top((-delta) as usize);
        } else {
            for _ in 0..delta {
                self.vstack.push_mem();
            }
        }

        // Mark the builtin's output area as materialized. The builtin wrote to
        // physical memory at `sp[0..outputs]`, which corresponds to offsets
        // `expected_top - outputs .. expected_top`. Any pre-existing Virtual
        // entries (e.g. from NOOP'd producers) must be invalidated.
        let (_, outputs) = self.current_inst().stack_io();
        let outputs = outputs as i32;
        if outputs > 0 {
            self.vstack.mark_materialized_range(expected_top - outputs..expected_top);
        }

        debug_assert_eq!(
            self.vstack.top_offset(),
            expected_top,
            "virtual stack sync mismatch after {:?}",
            self.current_inst().to_op(),
        );
    }

    /// Pushes a 256-bit value onto the stack.
    fn push(&mut self, value: B::Value) {
        self.vstack.push(value);
        self.len_offset += 1;
    }

    /// Returns the known constant values of the topmost `N` stack operands, in the same order
    /// as [`popn`](Self::popn): index 0 is TOS, index 1 is second from top, etc.
    fn const_operands<const N: usize>(&self) -> [Option<U256>; N] {
        let inst = self.current_inst.unwrap();
        std::array::from_fn(|i| self.bytecode.const_operand(inst, i))
    }

    /// Discards `n` stack inputs and pushes a compile-time constant.
    fn fold_const(&mut self, value: impl TryInto<U256>) {
        self.pop_ignore(self.current_inst().stack_io().0 as usize);
        let v = self.bcx.iconst_256(value);
        self.push(v);
    }

    /// Consumes the topmost `n` elements from the stack without loading them.
    fn pop_ignore(&mut self, n: usize) {
        self.vstack.drop_top(n);
        self.len_offset -= n as i8;
    }

    /// Removes the topmost element from the stack and returns it.
    fn pop(&mut self) -> B::Value {
        self.popn::<1>()[0]
    }

    /// Removes the topmost `N` elements from the stack and returns them.
    fn popn<const N: usize>(&mut self) -> [B::Value; N] {
        assert_ne!(N, 0);

        let operand_depth_base = (-self.len_offset) as usize;
        let values = std::array::from_fn(|i| {
            let operand_depth = operand_depth_base + i;
            let name = b'a' + i as u8;
            self.stack_value_at_depth(operand_depth, i, std::str::from_utf8(&[name]).unwrap())
        });
        self.pop_ignore(N);
        values
    }

    /// Duplicates the `n`th value from the top of the stack.
    /// `n` cannot be `0`.
    fn dup(&mut self, n: usize) {
        assert_ne!(n, 0);
        let name = if self.config.debug { &format!("dup{n}") } else { "" };
        let value = self.stack_value_at_depth(n - 1, n - 1, name);
        self.push(value);
    }

    /// Swaps the topmost value with the `n`th value from the top.
    /// `n` cannot be `0`.
    fn swap(&mut self, n: usize) {
        self.exchange(0, n);
    }

    /// Exchange two values on the stack.
    /// `n` is the first index, and the second index is calculated as `n + m`.
    /// `m` cannot be `0`.
    fn exchange(&mut self, n: usize, m: usize) {
        assert_ne!(m, 0);
        let a = self.stack_value_at_depth(n, n, "swap.a");
        let b = self.stack_value_at_depth(n + m, n + m, "swap.b");
        self.vstack.set(n, b);
        self.vstack.set(n + m, a);
    }

    /// `RETURN` or `REVERT` instruction.
    fn return_common(&mut self, ir: InstructionResult) {
        let sp = self.sp_after_inputs();
        let ir_const = self.bcx.iconst(self.i8_type, ir as i64);
        self.sync_diverging_stack_effect();
        let _ = self.call_builtin(Builtin::DoReturn, &[self.ecx, sp, ir_const]);
        self.bcx.unreachable();
    }

    fn sync_diverging_stack_effect(&mut self) {
        let data = self.current_inst();
        let (inp, out) = data.stack_io();
        let diff = effective_stack_diff(inp, out, data);
        self.sync_virtual_stack_diff(diff);
        if self.config.inspect_stack {
            self.materialize_live_stack();
            self.copy_stack_to_arg();
            self.save_stack_len();
        }
    }

    /// Builds a `CREATE` or `CREATE2` instruction.
    fn create_common(&mut self, create_kind: CreateKind) {
        let sp = self.sp_after_inputs();
        let create_kind = self.bcx.iconst(self.i8_type, create_kind as i64);
        self.call_fallible_builtin(Builtin::Create, &[self.ecx, sp, create_kind]);
        self.suspend();
    }

    /// Builds `*CALL*` instructions.
    fn call_common(&mut self, call_kind: CallKind) {
        let sp = self.sp_after_inputs();
        let call_kind = self.bcx.iconst(self.i8_type, call_kind as i64);
        self.call_fallible_builtin(Builtin::Call, &[self.ecx, sp, call_kind]);
        self.suspend();
    }

    /// Suspend execution, storing the resume point in the context.
    fn suspend(&mut self) {
        // Flush virtual stack before entering the shared suspend block.
        self.materialize_live_stack();

        // Register the next instruction as the resume block.
        let idx = self.resume_blocks.len();
        let resume_at = self
            .effective_next_entry(self.current_inst.unwrap())
            .expect("suspending instruction must have a resume target");
        self.add_resume_at(resume_at);

        // Register the current block as the suspend block.
        let value = self.bcx.iconst(self.isize_type, idx as i64 + 1);
        self.suspend_blocks.push((value, self.bcx.current_block().unwrap()));

        // Branch to the suspend block.
        self.bcx.br(self.suspend_block);
    }

    /// Adds a resume point.
    fn add_resume_at(&mut self, block: B::BasicBlock) {
        self.resume_blocks.push(block);
    }

    /// Loads the word at the given pointer.
    fn load_word(&mut self, ptr: B::Value, name: &str) -> B::Value {
        self.bcx.load(self.word_type, ptr, name)
    }

    /// Gets a field at the given offset.
    fn get_field(&mut self, ptr: B::Value, offset: usize, name: &str) -> B::Value {
        get_field(&mut self.bcx, ptr, offset, name)
    }

    /// Loads the `ecx.input` pointer on demand.
    fn load_input(&mut self) -> B::Value {
        let ptr_type = self.bcx.type_ptr();
        let input_field = get_field(
            &mut self.bcx,
            self.ecx,
            mem::offset_of!(EvmContext<'_>, input),
            "ecx.input.addr",
        );
        self.bcx.load(ptr_type, input_field, "ecx.input")
    }

    /// Re-loads the address at `slot` as i160, zero-extends to i256, and stores it back.
    ///
    /// On little-endian the low 160 bits sit at byte offset 0, so a direct
    /// `load i160` + `zext i256` gives LLVM a typed narrow load — no AND needed
    /// to prove the high 96 bits are zero.
    #[allow(clippy::assertions_on_constants)]
    fn narrow_to_address(&mut self, slot: B::Value) {
        debug_assert!(self.little_endian(), "big-endian not yet supported");
        let value = self.bcx.load(self.address_type, slot, "address");
        let value = self.bcx.zext(self.word_type, value);
        self.bcx.store(value, slot);
    }

    fn gas_remaining_addr(&mut self) -> B::Value {
        const OFFSET: usize =
            mem::offset_of!(EvmContext<'_>, gas) + mem::offset_of!(pf::Gas, tracker.remaining);
        let offset = self.bcx.iconst(self.isize_type, OFFSET as i64);
        self.bcx.gep(self.i8_type, self.ecx, &[offset], "gas.remaining.addr")
    }

    /// Saves the local `stack_len` to `stack_len_arg`.
    fn save_stack_len(&mut self) {
        let len = self.stack_len.load(&mut self.bcx, "stack_len");
        let ptr = self.stack_len_arg();
        self.bcx.store(len, ptr);
    }

    /// Copies the live prefix of the stack from the argument to the local alloca.
    /// `len` is the number of live stack elements.
    fn copy_stack_from_arg(&mut self, len: B::Value) {
        if let Some(src) = self.sp_arg {
            let dst = self.stack.addr(&mut self.bcx);
            let word_size = 32i64;
            let byte_len = self.bcx.imul_imm(len, word_size);
            self.bcx.memcpy(dst, src, byte_len);
        }
    }

    /// Copies the live prefix of the stack from the local alloca to the argument.
    fn copy_stack_to_arg(&mut self) {
        if let Some(dst) = self.sp_arg {
            let len = self.stack_len.load(&mut self.bcx, "stack_len");
            let src = self.stack.addr(&mut self.bcx);
            let word_size = 32i64;
            let byte_len = self.bcx.imul_imm(len, word_size);
            self.bcx.memcpy(dst, src, byte_len);
        }
    }

    /// Returns the stack length argument.
    fn stack_len_arg(&mut self) -> B::Value {
        self.bcx.fn_param(2)
    }

    /// Returns the stack pointer at the top (`&stack[stack.len]`).
    ///
    /// Used by builtins that write a single output directly to memory.
    /// The virtual stack is synced at instruction end via `sync_virtual_stack_diff`.
    #[must_use]
    fn sp_at_top(&mut self) -> B::Value {
        self.sp_from_section(self.section_len_offset as i64)
    }

    /// Returns the stack pointer after the input has been popped
    /// (`&stack[stack.len - op.input()]`).
    ///
    /// This materializes all virtual values in the input/output window so builtins
    /// can read/write the physical stack. For any input whose value is a known constant,
    /// the constant is written into the corresponding stack slot. This allows DSE to NOOP
    /// the producing PUSH even for builtin-delegated opcodes that read operands directly
    /// from the stack pointer.
    ///
    /// The virtual stack is synced with the builtin's stack effect at instruction end
    /// via `sync_virtual_stack_diff`.
    #[must_use]
    fn sp_after_inputs(&mut self) -> B::Value {
        let (inputs, outputs) = self.current_inst().stack_io();
        let inputs = inputs as usize;
        let outputs = outputs as usize;
        let top = self.section_len_offset;
        let start = top - inputs as i32;
        let window = inputs.max(outputs) as i32;
        self.materialize_range(start, start + window);
        self.write_const_operands(inputs);
        self.sp_from_top(inputs)
    }

    /// Like [`sp_after_inputs`](Self::sp_after_inputs) but only materializes the
    /// specified operand depths.
    #[must_use]
    fn sp_after_inputs_with(&mut self, depths: &[usize]) -> B::Value {
        let (inputs, _) = self.current_inst().stack_io();
        let inputs = inputs as usize;
        let top = self.section_len_offset;
        for &depth in depths {
            let off = top - inputs as i32 + (inputs - 1 - depth) as i32;
            self.materialize_range(off, off + 1);
        }
        self.write_const_operands(inputs);
        self.sp_from_top(inputs)
    }

    /// Writes known-constant operands into the physical stack so that builtins see
    /// correct values even when DSE has NOOP'd the producing instruction.
    fn write_const_operands(&mut self, inputs: usize) {
        let inst = self.current_inst.unwrap();
        let top = self.section_len_offset;
        for depth in 0..inputs {
            let off = top - inputs as i32 + (inputs - 1 - depth) as i32;
            if let VSlot::Materialized = self.vstack.get_at_offset(off)
                && let Some(c) = self.bytecode.const_operand(inst, depth)
            {
                let value = self.bcx.iconst_256(c);
                let sp = self.sp_from_section(off as i64);
                self.bcx.store(value, sp);
            }
        }
    }

    /// Returns a stack pointer offset from `section_start_sp`.
    fn sp_from_section(&mut self, offset: i64) -> B::Value {
        if offset == 0 {
            return self.section_start_sp;
        }
        let offset = self.bcx.iconst(self.isize_type, offset);
        self.bcx.gep(self.word_type, self.section_start_sp, &[offset], "sp")
    }

    /// Returns the stack pointer at `len` (`&stack[len]`).
    fn sp_at(&mut self, len: B::Value) -> B::Value {
        let ptr = self.stack.addr(&mut self.bcx);
        self.bcx.gep(self.word_type, ptr, &[len], "sp")
    }

    /// Returns the stack pointer at `n` from the top (`&stack[len - n]`).
    fn sp_from_top(&mut self, n: usize) -> B::Value {
        self.sp_from_section(self.section_len_offset as i64 - n as i64)
    }

    /// Resolves a stack value at the given depth via the virtual stack.
    ///
    /// - `operand_depth`: depth for `const_operand` lookup (0 = first popped by the instruction).
    /// - `live_depth`: depth into the virtual stack's current live range (0 = current TOS).
    ///
    /// These differ only inside `popn` where multiple pops happen: `operand_depth` counts
    /// from the instruction start, while `live_depth` counts from the current virtual TOS.
    fn stack_value_at_depth(
        &mut self,
        operand_depth: usize,
        live_depth: usize,
        name: &str,
    ) -> B::Value {
        let inst = self.current_inst.unwrap();
        if let Some(c) = self.bytecode.const_operand(inst, operand_depth) {
            return self.bcx.iconst_256(c);
        }
        match self.vstack.get(live_depth) {
            VSlot::Virtual(v) => v,
            VSlot::Materialized => {
                let off = self.vstack.offset_at_depth(live_depth);
                let sp = self.sp_from_section(off as i64);
                let value = self.load_word(sp, name);
                self.vstack.set(live_depth, value);
                value
            }
        }
    }

    /// Materializes all live virtual slots in the current section to memory.
    fn materialize_live_stack(&mut self) {
        let range = self.vstack.live_range();
        self.materialize_range(range.start, range.end);
    }

    /// Eagerly materializes the coldest virtual slots when too many are live,
    /// preventing excessive register pressure in long sections.
    fn relieve_vstack_pressure(&mut self) {
        /// Materialize when more than this many virtual slots are live.
        const HIGH_WATER: usize = 2;
        /// Never materialize the top N slots (they're likely used soon).
        const KEEP_HOT: usize = 2;

        let live = self.vstack.live_range();
        if (live.end - live.start) as usize <= HIGH_WATER {
            return;
        }

        let virtual_count = self.vstack.virtual_count();
        if virtual_count <= HIGH_WATER {
            return;
        }

        // Materialize everything below the hot window.
        let cold_end = (self.vstack.top_offset() - KEEP_HOT as i32).max(live.start);
        if cold_end > live.start {
            self.materialize_range(live.start, cold_end);
        }
    }

    /// Materializes all virtual slots in the given section-relative offset range.
    fn materialize_range(&mut self, start: i32, end: i32) {
        let pending: Vec<_> = self.vstack.pending_stores(start..end).collect();
        for (off, value) in pending {
            let sp = self.sp_from_section(off as i64);
            self.bcx.store(value, sp);
        }
        self.vstack.mark_materialized_range(start..end);
    }

    /// Builds a gas cost deduction for an immediate value.
    fn gas_cost_imm(&mut self, cost: u64) {
        if !self.config.gas_metering || cost == 0 {
            return;
        }
        let value = self.bcx.iconst(self.isize_type, cost as i64);
        self.gas_cost(value);
    }

    /// Builds a gas cost deduction for a value.
    fn gas_cost(&mut self, cost: B::Value) {
        if !self.config.gas_metering {
            return;
        }

        // Modified from `Gas::record_cost`.
        // This can overflow the gas counters, which has to be adjusted for after the call.
        let addr = self.gas_remaining_addr();
        let i64_type = self.bcx.type_int(64);
        let gas_remaining = self.bcx.load(i64_type, addr, "gas.remaining");
        let (res, overflow) = self.bcx.usub_overflow(gas_remaining, cost);
        self.bcx.store(res, addr);
        self.build_check(overflow, InstructionResult::OutOfGas);
    }

    /// Ensures the memory is large enough for `offset + len` bytes, calling the `mresize`
    /// builtin on the cold path if needed. Returns the pointer to `mem_base + offset`.
    fn build_ensure_memory(&mut self, offset: B::Value, len: u64) -> B::Value {
        // 63 bits lets us avoid overflow in the addition below.
        let offset = self.u256_to_u64_saturating(offset, 63);
        // Analysis proved this access is already covered on every path.
        if self.current_inst.is_some_and(|inst| self.can_skip_ensure_memory(inst)) {
            return self.build_memory_addr(offset);
        }

        let isize_type = self.isize_type;
        let len_const = self.bcx.iconst(isize_type, len as i64);
        let min_size = self.bcx.iadd(offset, len_const);

        let direct_resize_size = self
            .current_inst
            .map(|inst| self.bytecode.memory_section(inst).direct_resize_size)
            .unwrap_or_default();
        // Analysis proved this access always grows memory to exactly `min_size`.
        if direct_resize_size != 0 {
            self.call_fallible_builtin(Builtin::Mresize, &[self.ecx, min_size]);
            self.cached_mem_base = None;
            return self.build_memory_addr(offset);
        }

        // Otherwise emit the normal checked-resize branch.
        // Fast path: offset + len <= mem_len (no overflow).
        let mem_len_field =
            self.get_field(self.ecx, mem::offset_of!(EvmContext<'_>, mem_len), "ecx.mem_len.addr");
        let mem_len = self.bcx.load(isize_type, mem_len_field, "ecx.mem_len");
        let exceeds = self.bcx.icmp(IntCC::UnsignedGreaterThan, min_size, mem_len);
        self.cached_mem_base = None;

        self.if_then(exceeds, |this| {
            this.bcx.set_current_block_cold();
            this.call_fallible_builtin(Builtin::Mresize, &[this.ecx, min_size]);
        });
        self.build_memory_addr(offset)
    }

    fn can_skip_ensure_memory(&self, inst: Inst) -> bool {
        let section = self.bytecode.memory_section(inst);
        if section.known_size < section.required_size {
            return false;
        }
        let mut has_memory_access = false;
        for (offset, len) in self.bytecode.const_memory_accesses(inst).into_iter().flatten() {
            has_memory_access = true;
            if offset.is_none() || len.is_none() {
                return false;
            }
        }
        has_memory_access
    }

    fn build_memory_addr(&mut self, offset: B::Value) -> B::Value {
        let mem_base = self.cached_mem_base.unwrap_or_else(|| self.load_memory_base());
        self.cached_mem_base = Some(mem_base);
        self.bcx.gep(self.i8_type, mem_base, &[offset], "mem.addr")
    }

    fn load_memory_base(&mut self) -> B::Value {
        let ptr_type = self.bcx.type_ptr();
        let mem_base_field = self.get_field(
            self.ecx,
            mem::offset_of!(EvmContext<'_>, mem_base),
            "ecx.mem_base.addr",
        );
        self.bcx.load(ptr_type, mem_base_field, "ecx.mem_base")
    }

    /*
    /// Builds a check, failing if the condition is false.
    ///
    /// `if success_cond { ... } else { return ret }`
    fn build_failure_inv(&mut self, success_cond: B::Value, ret: InstructionResult) {
        self.build_failure_imm_inner(false, success_cond, ret);
    }
    */

    /// Emits under/overflow bounds checks for a stack section.
    fn check_stack_bounds(&mut self, stack_section: StackSection) {
        let inp = stack_section.inputs;
        let diff = stack_section.max_growth as i64;

        let underflow = |this: &mut Self| {
            debug_assert!(inp > 0);
            this.bcx.icmp_imm(IntCC::UnsignedLessThan, this.len_before, inp as i64)
        };
        let overflow = |this: &mut Self| {
            debug_assert!(diff > 0);
            if diff > STACK_CAP as i64 {
                return this.bcx.bool_const(true);
            }
            this.bcx.icmp_imm(IntCC::UnsignedGreaterThan, this.len_before, STACK_CAP as i64 - diff)
        };

        let may_underflow = inp > 0;
        let may_overflow = diff > 0;
        if may_underflow && may_overflow {
            let underflow = underflow(self);
            let overflow = overflow(self);
            let cond = self.bcx.bitor(underflow, overflow);
            let ret = {
                let under = self.bcx.iconst(self.i8_type, InstructionResult::StackUnderflow as i64);
                let over = self.bcx.iconst(self.i8_type, InstructionResult::StackOverflow as i64);
                self.bcx.select(underflow, under, over)
            };
            let target = self.build_check_inner(true, cond, ret);
            self.bcx.switch_to_block(target);
        } else if may_underflow {
            let cond = underflow(self);
            self.build_check(cond, InstructionResult::StackUnderflow);
        } else if may_overflow {
            let cond = overflow(self);
            self.build_check(cond, InstructionResult::StackOverflow);
        }
    }

    /// Builds a check, failing if the condition is true.
    ///
    /// `if failure_cond { return ret } else { ... }`
    fn build_check(&mut self, failure_cond: B::Value, ret: InstructionResult) {
        self.build_check_imm_inner(true, failure_cond, ret);
    }

    fn build_check_imm_inner(&mut self, is_failure: bool, cond: B::Value, ret: InstructionResult) {
        let ret_value = self.bcx.iconst(self.i8_type, ret as i64);
        let target = self.build_check_inner(is_failure, cond, ret_value);
        if self.config.comments {
            self.add_comment(&format!("check {ret:?}"));
        }
        self.bcx.switch_to_block(target);
    }

    #[must_use]
    fn build_check_inner(
        &mut self,
        is_failure: bool,
        cond: B::Value,
        ret: B::Value,
    ) -> B::BasicBlock {
        let current_block = self.current_block();
        let target = self.create_block_after(current_block, "contd");

        let exit_block = if is_failure {
            if let Some(failure_block) = self.failure_block {
                self.incoming_failures.push((ret, current_block));
                failure_block
            } else {
                self.create_block_after(target, "failure")
            }
        } else if let Some(return_block) = self.return_block {
            self.incoming_returns.push((ret, current_block));
            return_block
        } else {
            self.create_block_after(target, "return")
        };
        let then_block = if is_failure { exit_block } else { target };
        let else_block = if is_failure { target } else { exit_block };
        self.bcx.brif_cold(cond, then_block, else_block, is_failure);

        if (is_failure && self.failure_block.is_none())
            || (!is_failure && self.return_block.is_none())
        {
            self.bcx.switch_to_block(exit_block);
            self.bcx.ret(&[ret]);
        }

        target
    }

    /// Builds a branch to the failure block.
    fn build_fail_imm(&mut self, ret: InstructionResult) {
        let ret_value = self.bcx.iconst(self.i8_type, ret as i64);
        self.build_fail(ret_value);
        if self.config.comments {
            self.add_comment(&format!("fail {ret:?}"));
        }
    }

    /// Builds a branch to the failure block.
    fn build_fail(&mut self, ret: B::Value) {
        if self.config.inspect_stack {
            self.materialize_live_stack();
        }
        if let Some(block) = self.failure_block {
            self.incoming_failures.push((ret, self.bcx.current_block().unwrap()));
            self.bcx.br(block);
        } else {
            self.bcx.ret(&[ret]);
        }
    }

    /// Builds a branch to the return block.
    fn build_return_imm(&mut self, ret: InstructionResult) {
        let ret_value = self.bcx.iconst(self.i8_type, ret as i64);
        self.build_return(ret_value);
        if self.config.comments {
            self.add_comment(&format!("return {ret:?}"));
        }
    }

    /// Builds a branch to the return block.
    fn build_return(&mut self, ret: B::Value) {
        if self.config.inspect_stack {
            self.materialize_live_stack();
        }
        self.build_return_inner(ret);
    }

    fn build_return_inner(&mut self, ret: B::Value) {
        if let Some(block) = self.return_block {
            self.incoming_returns.push((ret, self.bcx.current_block().unwrap()));
            self.bcx.br(block);
        } else {
            self.bcx.ret(&[ret]);
        }
    }

    fn add_invalid_jump(&mut self) -> B::BasicBlock {
        let block = self.failure_block.unwrap();
        self.incoming_failures.push((
            self.bcx.iconst(self.i8_type, InstructionResult::InvalidJump as i64),
            self.bcx.current_block().unwrap(),
        ));
        block
    }

    /// Build a call to the panic builtin.
    fn call_panic(&mut self, msg: &str) {
        let function = self.builtin_function(Builtin::Panic);
        let ptr = self.bcx.str_const(msg);
        let len = self.bcx.iconst(self.isize_type, msg.len() as i64);
        let _ = self.bcx.call(function, &[ptr, len]);
        self.bcx.unreachable();
    }

    #[allow(dead_code)]
    fn call_printf(&mut self, template: &std::ffi::CStr, values: &[B::Value]) {
        let mut args = Vec::with_capacity(values.len() + 1);
        args.push(self.bcx.cstr_const(template));
        args.extend_from_slice(values);
        let printf = self.bcx.get_printf_function();
        let _ = self.bcx.call(printf, &args);
    }

    /// Build a call to a fallible builtin.
    ///
    /// The builtin longjmps on error, so no return value check is needed.
    fn call_fallible_builtin(&mut self, builtin: Builtin, args: &[B::Value]) {
        let _ = self.call_builtin(builtin, args);
    }

    /// Build a call to a builtin.
    #[must_use]
    fn call_builtin(&mut self, builtin: Builtin, args: &[B::Value]) -> Option<B::Value> {
        let function = self.builtin_function(builtin);
        // self.call_printf(
        //     format_printf!("{} - calling {}\n", self.op_block_name(""), builtin.name()),
        //     &[],
        // );
        let invalidate_mem_base =
            !self.current_inst.is_some_and(|inst| self.can_skip_ensure_memory(inst));
        let value = self.bcx.call(function, args);
        if invalidate_mem_base {
            self.cached_mem_base = None;
        }
        value
    }

    /// Gets the function for the given builtin.
    fn builtin_function(&mut self, builtin: Builtin) -> B::Function {
        self.builtins.get(builtin, &mut self.bcx)
    }

    /// Adds a comment to the current instruction.
    fn add_comment(&mut self, comment: &str) {
        if comment.is_empty() || !self.config.comments {
            return;
        }
        self.bcx.add_comment_to_current_inst(comment);
    }

    /// Returns the current instruction.
    fn current_inst(&self) -> &InstData {
        self.bytecode.inst(self.current_inst.unwrap())
    }

    fn if_then(&mut self, cond: B::Value, then: impl FnOnce(&mut Self)) {
        let current_block = self.current_block();
        let then_block = self.create_block_after(current_block, "then");
        let done_block = self.create_block_after(then_block, "contd");

        self.bcx.brif(cond, then_block, done_block);

        self.bcx.switch_to_block(then_block);
        then(self);
        self.bcx.br(done_block);

        self.bcx.switch_to_block(done_block);
    }

    /// Returns the current block.
    fn current_block(&mut self) -> B::BasicBlock {
        // There always is a block present.
        self.bcx.current_block().expect("no blocks")
    }

    fn little_endian(&self) -> bool {
        true
    }

    /*
    /// Creates a named block.
    fn create_block(&mut self, name: &str) -> B::BasicBlock {
        let name = self.op_block_name(name);
        self.bcx.create_block(&name)
    }
    */

    /// Creates a named block after the given block.
    fn create_block_after(&mut self, after: B::BasicBlock, name: &str) -> B::BasicBlock {
        let name = self.op_block_name(name);
        self.bcx.create_block_after(after, &name)
    }

    /// Returns the block name for the current opcode with the given suffix.
    fn op_block_name(&self, name: &str) -> String {
        if !self.config.debug {
            return String::new();
        }
        self.bytecode.op_block_name(self.current_inst, name)
    }

    /// Converts a 256-bit unsigned integer to a 64-bit unsigned integer, saturating at
    /// `2^bits - 1`.
    fn u256_to_u64_saturating(&mut self, value: B::Value, bits: usize) -> B::Value {
        let i64_type = self.bcx.type_int(64);
        let reduced = self.bcx.ireduce(i64_type, value);
        let sentinel_lit = 1u128.checked_shl(bits as u32).unwrap_or(0).wrapping_sub(1);
        let sentinel_u256 = self.bcx.iconst_256(U256::from(sentinel_lit));
        let fits = self.bcx.icmp(IntCC::UnsignedLessThanOrEqual, value, sentinel_u256);
        let sentinel = self.bcx.iconst(i64_type, sentinel_lit as i64);
        self.bcx.select(fits, reduced, sentinel)
    }
}

/// IR builtins.
impl<B: Backend> FunctionCx<'_, B> {
    #[allow(dead_code)]
    #[must_use]
    fn call_ir_builtin(
        &mut self,
        name: &str,
        args: &[B::Value],
        arg_types: &[B::Type],
        ret: Option<B::Type>,
        build: impl FnOnce(&mut Self),
    ) -> Option<B::Value> {
        let prefix = "__revmc_ir_builtin_";
        let name = &format!("{prefix}{name}")[..];

        // self.call_printf(format_printf!("{} - calling {name}\n", self.op_block_name("")), &[]);

        debug_assert_eq!(args.len(), arg_types.len());
        let linkage = revmc_backend::Linkage::Private;
        // SAFETY: `this` aliases `self` to work around the borrow checker: we need `self.bcx`
        // borrowed by `get_or_build_function` while also swapping `self.bcx` inside the closure.
        // The closure's `bcx` is a fresh builder (not `self.bcx`), so the swap is safe.
        let debug_location = self
            .config
            .debug
            .then(|| self.current_inst.map(|inst| self.inst_lines[inst]))
            .flatten();
        self.bcx.clear_debug_location();

        let this = unsafe { &mut *(self as *mut Self) };
        let f = self.bcx.get_or_build_function(name, arg_types, ret, linkage, |bcx| {
            let prev_return_block = this.return_block.take();
            let prev_failure_block = this.failure_block.take();
            // SAFETY: `this.bcx` and `bcx` are non-overlapping (bcx is a fresh builder).
            unsafe { std::ptr::swap(&mut this.bcx, bcx) };

            for attr in default_attrs::for_fn().chain(std::iter::once(Attribute::NoUnwind)) {
                this.bcx.add_function_attribute(None, attr, FunctionAttributeLocation::Function)
            }
            for i in 0..this.bcx.num_fn_params() as u32 {
                for attr in default_attrs::for_param() {
                    this.bcx.add_function_attribute(None, attr, FunctionAttributeLocation::Param(i))
                }
            }
            build(this);

            // SAFETY: same as above.
            unsafe { std::ptr::swap(&mut this.bcx, bcx) };
            this.failure_block = prev_failure_block;
            this.return_block = prev_return_block;
        });
        if let Some(line) = debug_location {
            self.bcx.set_debug_location(line, 1);
        }
        self.bcx.call(f, args)
    }
}

// HACK: Need these structs' fields to be public for `offset_of!`.
// `pf == private_fields`.
#[allow(dead_code)]
mod pf {
    use super::*;

    #[repr(C)] // See core::ptr::metadata::PtrComponents
    pub(super) struct Slice {
        pub(super) ptr: *const u8,
        pub(super) len: usize,
    }
    const _: [(); mem::size_of::<&'static [u8]>()] = [(); mem::size_of::<Slice>()];

    pub(super) struct Gas {
        /// GasTracker fields (EIP-8037 reservoir model).
        pub(super) tracker: GasTracker,
        /// Memory gas tracking (words_num: usize, expansion_cost: u64).
        pub(super) memory: MemoryGas,
    }

    pub(super) struct GasTracker {
        /// The initial gas limit.
        pub(super) limit: u64,
        /// The remaining gas.
        pub(super) remaining: u64,
        /// State gas reservoir (EIP-8037).
        pub(super) reservoir: u64,
        /// Net state gas spent.
        pub(super) state_gas_spent: i64,
        /// State gas drawn from regular gas because the reservoir was empty (EIP-8037).
        pub(super) state_gas_spilled: u64,
        /// Refunded gas.
        pub(super) refunded: i64,
    }

    #[repr(C)]
    pub(super) struct MemoryGas {
        pub(super) words_num: usize,
        pub(super) expansion_cost: u64,
    }
    const _: [(); mem::size_of::<revm_interpreter::Gas>()] = [(); mem::size_of::<Gas>()];
}

/// Computes the effective stack diff for an instruction, matching the codegen semantics.
fn effective_stack_diff(inp: u8, out: u8, data: &InstData) -> i32 {
    let mut diff = out as i32 - inp as i32;
    // Suspending ops return one value pushed by the caller after resume.
    if data.may_suspend() {
        diff -= 1;
    }
    diff
}

fn get_field<B: Builder>(bcx: &mut B, ptr: B::Value, offset: usize, name: &str) -> B::Value {
    let offset = bcx.iconst(bcx.type_ptr_sized_int(), offset as i64);
    bcx.gep(bcx.type_int(8), ptr, &[offset], name)
}

#[allow(unused)]
macro_rules! format_printf {
    ($($t:tt)*) => {
        &std::ffi::CString::new(format!($($t)*)).unwrap()
    };
}
#[allow(unused)]
use format_printf;
