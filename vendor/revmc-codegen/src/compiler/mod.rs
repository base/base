//! EVM bytecode compiler implementation.

use std::{
    cell::Cell,
    fs,
    io::{self, Write},
    mem,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use revm_primitives::{Bytes, hardfork::SpecId};
use revmc_backend::{
    Attribute, BackendConfig, FunctionAttributeLocation, Linkage, OptimizationLevel, eyre::ensure,
    format_bytes,
};
use revmc_builtins::Builtins;
use revmc_context::RawEvmCompilerFn;

use crate::{
    Backend, Builder, Bytecode, EvmCompilerFn, EvmContext, EvmStack, FxHashMap, GasParams, Result,
    bytecode::AnalysisConfig,
};

mod translate;
use translate::{FcxConfig, FunctionCx};

/// Collected timing remarks for the compiler dump.
#[derive(Default)]
struct Remarks {
    parse: Cell<Duration>,
    translate: Cell<Duration>,
    verify: Cell<Duration>,
    optimize: Cell<Duration>,
    codegen: Cell<Duration>,
    finalize_total: Cell<Duration>,
}

/// Per-phase timing breakdown from a compilation.
#[derive(Clone, Copy, Debug, Default)]
pub struct CompileTimings {
    /// Time spent parsing and analyzing EVM bytecode.
    pub parse: Duration,
    /// Time spent translating analyzed bytecode to IR.
    pub translate: Duration,
    /// Time spent running optimization passes.
    pub optimize: Duration,
    /// Time spent emitting machine code (JIT lookup or object write).
    pub codegen: Duration,
}

impl Remarks {
    fn clear(&mut self) {
        *self = Self::default();
    }

    fn time(&self, field: impl FnOnce(&Self) -> &Cell<Duration>) -> TimingGuard<'_> {
        TimingGuard { target: field(self), start: Instant::now() }
    }
}

struct TimingGuard<'a> {
    target: &'a Cell<Duration>,
    start: Instant,
}

impl Drop for TimingGuard<'_> {
    fn drop(&mut self) {
        self.target.set(self.target.get() + self.start.elapsed());
    }
}

/// EVM bytecode compiler.
///
/// This currently represents one single-threaded IR context and module, which can be used to
/// compile multiple functions as JIT or AOT.
///
/// Functions can be incrementally added with [`translate`], and then either written to an object
/// file with [`write_object`] when in AOT mode, or JIT-compiled with [`jit_function`].
///
/// Performing either of these operations finalizes the module, and no more functions can be added
/// afterwards until [`clear`] is called, which will reset the module to its initial state.
///
/// [`translate`]: EvmCompiler::translate
/// [`write_object`]: EvmCompiler::write_object
/// [`jit_function`]: EvmCompiler::jit_function
/// [`clear`]: EvmCompiler::clear
#[derive(derive_more::Debug)]
pub struct EvmCompiler<B: Backend> {
    name: Option<String>,
    #[debug(skip)]
    backend: B,
    out_dir: Option<PathBuf>,
    config: FcxConfig,
    #[debug(skip)]
    builtins: Builtins<B>,
    #[debug(skip)]
    gas_params: Option<GasParams>,

    dedup: bool,
    dse: bool,

    dump_assembly: bool,
    dump_unopt_assembly: bool,

    compiler_gas_limit: u64,

    #[debug(skip)]
    remarks: Remarks,
    /// Function sizes accumulated across all `jit_function`/`write_object` calls
    /// in the current finalize cycle. Reset by `clear` / `clear_ir`.
    compiled_sizes: Vec<(String, usize)>,
    finalized: bool,
}

#[cfg(feature = "llvm")]
impl EvmCompiler<revmc_llvm::EvmLlvmBackend> {
    /// Creates a new instance of the compiler with the LLVM backend.
    pub fn new_llvm(aot: bool) -> Result<Self> {
        revmc_llvm::EvmLlvmBackend::new(aot).map(Self::new)
    }
}

impl<B: Backend> EvmCompiler<B> {
    /// Creates a new instance of the compiler with the given backend.
    pub fn new(backend: B) -> Self {
        Self {
            name: None,
            backend,
            out_dir: None,
            config: FcxConfig::default(),
            builtins: Builtins::new(),
            gas_params: None,
            dedup: true,
            dse: true,
            dump_assembly: true,
            dump_unopt_assembly: false,
            compiler_gas_limit: crate::bytecode::DEFAULT_COMPILER_GAS_LIMIT,
            remarks: Remarks::default(),
            compiled_sizes: Vec::new(),
            finalized: false,
        }
    }

    /// Sets the name of the module.
    pub fn set_module_name(&mut self, name: impl Into<String>) {
        let name = name.into();
        self.backend.set_module_name(&name);
        self.name = Some(name);
    }

    fn is_aot(&self) -> bool {
        self.backend.is_aot()
    }

    fn is_jit(&self) -> bool {
        !self.is_aot()
    }

    /// Returns a reference to the underlying compiler backend.
    #[doc(hidden)]
    #[inline]
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Returns a mutable reference to the underlying compiler backend.
    #[doc(hidden)]
    #[inline]
    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    /// Returns the per-phase timing breakdown from the last compilation and resets the counters.
    pub fn take_timings(&self) -> CompileTimings {
        let r = &self.remarks;
        let timings = CompileTimings {
            parse: r.parse.get(),
            translate: r.translate.get(),
            optimize: r.optimize.get(),
            codegen: r.codegen.get(),
        };
        r.parse.set(Duration::ZERO);
        r.translate.set(Duration::ZERO);
        r.optimize.set(Duration::ZERO);
        r.codegen.set(Duration::ZERO);
        timings
    }

    /// Clones the current backend config, applies `f`, and sends the updated snapshot.
    fn update_backend_config(&mut self, f: impl FnOnce(&mut BackendConfig)) {
        let mut config = self.backend.config().clone();
        f(&mut config);
        self.backend.apply_config(config);
    }

    /// Returns the output directory.
    pub fn out_dir(&self) -> Option<&Path> {
        self.out_dir.as_deref()
    }

    /// Dumps intermediate outputs and other debug info to the given directory after compilation.
    ///
    /// Disables dumping if `output_dir` is `None`.
    pub fn set_dump_to(&mut self, output_dir: Option<PathBuf>) {
        self.update_backend_config(|c| c.is_dumping = output_dir.is_some());
        self.config.comments = output_dir.is_some();
        self.config.debug = output_dir.is_some();
        if output_dir.is_some() {
            self.config.frame_pointers = true;
        }
        self.out_dir = output_dir;
    }

    /// Dumps assembly to the output directory.
    ///
    /// This can be quite slow.
    ///
    /// Defaults to `true`.
    pub fn dump_assembly(&mut self, yes: bool) {
        self.dump_assembly = yes;
    }

    /// Dumps the unoptimized assembly to the output directory.
    ///
    /// This can be quite slow.
    ///
    /// Defaults to `false`.
    pub fn dump_unopt_assembly(&mut self, yes: bool) {
        self.dump_unopt_assembly = yes;
    }

    /// Returns the optimization level.
    pub fn opt_level(&self) -> OptimizationLevel {
        self.backend.config().opt_level
    }

    /// Sets the optimization level.
    ///
    /// Note that some backends may not support setting the optimization level after initialization.
    ///
    /// Defaults to the backend's initial optimization level.
    pub fn set_opt_level(&mut self, level: OptimizationLevel) {
        self.update_backend_config(|c| c.opt_level = level);
    }

    /// Sets whether to enable debug assertions.
    ///
    /// These are useful for debugging, but they do a moderate performance penalty due to the
    /// insertion of extra checks and removal of certain assumptions.
    ///
    /// Defaults to `cfg!(debug_assertions)`.
    pub fn debug_assertions(&mut self, yes: bool) {
        self.update_backend_config(|c| c.debug_assertions = yes);
        self.config.debug_assertions = yes;
    }

    /// Enables the block deduplication pass.
    ///
    /// Defaults to `true`.
    pub fn set_dedup(&mut self, yes: bool) {
        self.dedup = yes;
    }

    /// Enables the dead store elimination pass.
    ///
    /// Defaults to `true`.
    pub fn set_dse(&mut self, yes: bool) {
        self.dse = yes;
    }

    /// Returns whether JIT debug support is enabled.
    ///
    /// Registers JIT objects with debuggers via `__jit_debug_register_code`,
    /// allowing GDB/LLDB to resolve JIT-compiled function names and set breakpoints.
    ///
    /// This setting is applied once per process on first JIT compilation.
    /// Subsequent compilers inherit the value set by the first.
    ///
    /// Defaults to `true`.
    pub fn debug_support(&self) -> bool {
        self.backend.config().debug_support
    }

    /// Sets whether to enable JIT debug support.
    ///
    /// This setting is applied once per process on first JIT compilation.
    /// Subsequent compilers inherit the value set by the first.
    pub fn set_debug_support(&mut self, yes: bool) {
        self.update_backend_config(|c| c.debug_support = yes);
    }

    /// Returns whether JIT profiling support is enabled.
    ///
    /// Installs the LLVM `PerfSupportPlugin` which writes jitdump records,
    /// allowing profilers to resolve JIT-compiled symbols with debug and unwind info.
    ///
    /// This setting is applied once per process on first JIT compilation.
    /// Subsequent compilers inherit the value set by the first.
    ///
    /// Defaults to `false`.
    pub fn profiling_support(&self) -> bool {
        self.backend.config().profiling_support
    }

    /// Sets whether to enable JIT profiling support.
    ///
    /// This setting is applied once per process on first JIT compilation.
    /// Subsequent compilers inherit the value set by the first.
    pub fn set_profiling_support(&mut self, yes: bool) {
        self.update_backend_config(|c| c.profiling_support = yes);
    }

    /// Returns whether the simple perf map plugin is enabled.
    ///
    /// Writes `/tmp/perf-<pid>.map` in the perf map format so that profilers
    /// can resolve JIT-compiled symbols without the jitdump machinery.
    ///
    /// Not suitable for long-running programs. The map file is append-only
    /// and never cleaned up, so entries for freed JIT code accumulate
    /// indefinitely. Prefer [`set_profiling_support`](Self::set_profiling_support)
    /// (jitdump) for long-lived processes.
    ///
    /// This setting is applied once per process on first JIT compilation.
    /// Subsequent compilers inherit the value set by the first.
    ///
    /// Defaults to `true`.
    pub fn simple_perf(&self) -> bool {
        self.backend.config().simple_perf
    }

    /// Sets whether to enable the simple perf map plugin.
    ///
    /// This setting is applied once per process on first JIT compilation.
    /// Subsequent compilers inherit the value set by the first.
    pub fn set_simple_perf(&mut self, yes: bool) {
        self.update_backend_config(|c| c.simple_perf = yes);
    }

    /// Sets whether to enable debug info emission.
    ///
    /// Debug info embeds DWARF metadata and annotates IR/assembly dumps with source locations
    /// from the parsed bytecode.
    ///
    /// Automatically enabled by [`set_dump_to`](Self::set_dump_to).
    ///
    /// Defaults to `false`.
    pub fn set_debug_info(&mut self, yes: bool) {
        self.config.debug = yes;
    }

    /// Sets whether to enable frame pointers.
    ///
    /// This is useful for profiling and debugging, but it incurs a very slight performance penalty.
    ///
    /// Enabled by default in debug builds, when `-Cforce-frame-pointers` is set, or when
    /// [`set_dump_to`](Self::set_dump_to) is called with a directory.
    pub fn frame_pointers(&mut self, yes: bool) {
        self.config.frame_pointers = yes;
    }

    /// Sets whether to treat the stack as observable outside the function.
    ///
    /// If this is set to `true`, the stack length must be passed in the arguments.
    ///
    /// This is useful to inspect the stack after the function has been executed, but it does
    /// incur a performance penalty as the stack will be stored at all return sites.
    ///
    /// Defaults to `false`.
    pub fn inspect_stack(&mut self, yes: bool) {
        self.config.inspect_stack = yes;
    }

    /// Sets whether to enable stack bound checks.
    ///
    /// Defaults to `true`.
    ///
    /// # Safety
    ///
    /// Removing stack length checks may improve compilation speed and performance, but will result
    /// in **undefined behavior** if the stack length overflows at runtime, rather than a
    /// [`StackUnderflow`]/[`StackOverflow`] result.
    ///
    /// [`StackUnderflow`]: revm_interpreter::InstructionResult::StackUnderflow
    /// [`StackOverflow`]: revm_interpreter::InstructionResult::StackOverflow
    pub unsafe fn stack_bound_checks(&mut self, yes: bool) {
        self.config.stack_bound_checks = yes;
    }

    /// Sets the gas budget for compile-time evaluation of user-supplied bytecode.
    ///
    /// The compiler evaluates EVM operations at compile time during analysis passes. Without a
    /// budget, adversarial bytecode (e.g. many `EXP`) can make compilation
    /// arbitrarily slow. This limit uses the EVM gas schedule to bound work.
    ///
    /// When the budget is exhausted, further evaluation is skipped and values remain dynamic.
    ///
    /// Defaults to 100k gas. Set to `0` to disable compile-time evaluation entirely, or
    /// `u64::MAX` to disable the limit.
    pub fn set_compiler_gas_limit(&mut self, limit: u64) {
        self.compiler_gas_limit = limit;
    }

    /// Sets whether to track gas costs.
    ///
    /// Disabling this will greatly improves compilation speed and performance, at the cost of not
    /// being able to check for gas exhaustion.
    ///
    /// Note that this does not disable gas usage in certain instructions, mainly the ones that
    /// are implemented as builtins.
    ///
    /// Use with care, as executing a function with gas disabled may result in an infinite loop.
    ///
    /// Defaults to `true`.
    pub fn gas_metering(&mut self, yes: bool) {
        self.config.gas_metering = yes;
    }

    /// Sets whether to collapse every JIT failure path to a single
    /// [`OutOfGas`](revm_interpreter::InstructionResult::OutOfGas) constant.
    ///
    /// Failures (stack under/overflow, invalid jump, real OOG, invalid opcode, etc.) are
    /// semantically interchangeable for callers that only branch on success vs failure, so
    /// this lets LLVM DCE the per-failure-site materialization and the failure-block phi.
    /// Successful exits (`STOP`/`RETURN`/`REVERT`) keep their original codes.
    ///
    /// Useful for benchmarking the cost of failure-result materialization.
    ///
    /// Defaults to `true`.
    pub fn single_error(&mut self, yes: bool) {
        self.config.single_error = yes;
    }

    /// Sets custom gas parameters.
    ///
    /// Overrides the default gas schedule derived from the spec_id.
    /// Useful for custom chains or hardforks with non-standard gas costs.
    ///
    /// Defaults to `GasParams::new_spec(spec_id)`.
    pub fn set_gas_params(&mut self, gas_params: GasParams) {
        self.gas_params = Some(gas_params);
    }

    /// Translates the given EVM bytecode into an internal function.
    ///
    /// NOTE: `name` must be unique for each function, as it is used as the name of the final
    /// symbol.
    pub fn translate<'a>(
        &mut self,
        name: &str,
        input: impl Into<EvmCompilerInput<'a>>,
        spec_id: SpecId,
    ) -> Result<B::FuncId> {
        ensure!(!self.finalized, "cannot compile more functions after finalizing the module");
        let bytecode = self.parse(input.into(), spec_id)?;
        self.translate_inner(name, &bytecode)
    }

    /// (JIT) Compiles the given EVM bytecode into a JIT function.
    ///
    /// See [`translate`](Self::translate) for more information.
    ///
    /// # Safety
    ///
    /// The returned function pointer is owned by the module, and must not be called after the
    /// module is cleared or the function is freed.
    pub unsafe fn jit<'a>(
        &mut self,
        name: &str,
        bytecode: impl Into<EvmCompilerInput<'a>>,
        spec_id: SpecId,
    ) -> Result<EvmCompilerFn> {
        let id = self.translate(name, bytecode.into(), spec_id)?;
        unsafe { self.jit_function(id) }
    }

    /// (JIT) Finalizes the module and JITs the given function.
    ///
    /// # Safety
    ///
    /// The returned function pointer is owned by the module, and must not be called after the
    /// module is cleared or the function is freed.
    pub unsafe fn jit_function(&mut self, id: B::FuncId) -> Result<EvmCompilerFn> {
        ensure!(self.is_jit(), "cannot JIT functions during AOT compilation");
        self.finalize()?;
        let addr = {
            let _t = self.remarks.time(|r| &r.codegen);
            self.backend.jit_function(id)?
        };
        debug_assert!(addr != 0);
        self.record_compiled_sizes();
        if let Some(dump_dir) = &self.dump_dir() {
            self.dump_remarks(dump_dir)?;
        }
        Ok(EvmCompilerFn::new(unsafe { std::mem::transmute::<usize, RawEvmCompilerFn>(addr) }))
    }

    /// (AOT) Writes the compiled object to the given file.
    pub fn write_object_to_file(&mut self, path: &Path) -> Result<()> {
        let file = fs::File::create(path)?;
        let mut writer = io::BufWriter::new(file);
        self.write_object(&mut writer)?;
        writer.flush()?;
        Ok(())
    }

    /// Finalizes the module and writes the compiled object to the given writer.
    pub fn write_object<W: io::Write>(&mut self, w: W) -> Result<()> {
        self.finalize()?;
        {
            let _t = self.remarks.time(|r| &r.codegen);
            self.backend.write_object(w)?;
        }
        if let Some(dump_dir) = &self.dump_dir() {
            self.dump_remarks(dump_dir)?;
        }
        Ok(())
    }

    /// (JIT) Frees the memory associated with a single function.
    ///
    /// Note that this will not reset the state of the internal module even if all functions are
    /// freed with this function. Use [`clear`] to reset the module.
    ///
    /// [`clear`]: EvmCompiler::clear
    ///
    /// # Safety
    ///
    /// Because this function invalidates any pointers retrieved from the corresponding module, it
    /// should only be used when none of the functions from that module are currently executing and
    /// none of the `fn` pointers are called afterwards.
    pub unsafe fn free_function(&mut self, id: B::FuncId) -> Result<()> {
        unsafe { self.backend.free_function(id) }
    }

    /// Clears the IR module, freeing memory used by IR representations.
    ///
    /// This does **not** free JIT-compiled machine code, so previously obtained function pointers
    /// remain valid. The module is left in a state where new functions can be translated.
    pub fn clear_ir(&mut self) -> Result<()> {
        self.builtins.clear();
        self.remarks.clear();
        self.compiled_sizes.clear();
        self.finalized = false;
        self.backend.clear_ir()
    }

    /// Frees all functions and resets the state of the internal module, allowing for new functions
    /// to be compiled.
    ///
    /// # Safety
    ///
    /// Because this function invalidates any pointers retrieved from the corresponding module, it
    /// should only be used when none of the functions from that module are currently executing and
    /// none of the `fn` pointers are called afterwards.
    pub unsafe fn clear(&mut self) -> Result<()> {
        self.builtins.clear();
        self.remarks.clear();
        self.compiled_sizes.clear();
        self.finalized = false;
        unsafe { self.backend.free_all_functions() }
    }

    /// Parses the given EVM bytecode. Not public API.
    #[doc(hidden)] // Not public API.
    pub fn parse<'a>(
        &mut self,
        input: EvmCompilerInput<'a>,
        spec_id: SpecId,
    ) -> Result<Bytecode<'a>> {
        let _t = self.remarks.time(|r| &r.parse);
        let EvmCompilerInput::Code(bytecode) = input;

        let mut bytecode = Bytecode::new(bytecode, spec_id, self.gas_params.clone());
        bytecode.compiler_gas_limit = self.compiler_gas_limit;
        bytecode.config.set(AnalysisConfig::INSPECT_STACK, self.config.inspect_stack);
        bytecode.config.set(AnalysisConfig::DEDUP, self.dedup);
        bytecode.config.set(AnalysisConfig::DSE, self.dse);
        bytecode.analyze()?;
        if let Some(dump_dir) = &self.dump_dir() {
            Self::dump_bytecode(dump_dir, &bytecode)?;
        }
        Ok(bytecode)
    }

    #[instrument(name = "translate", level = "debug", skip_all)]
    #[doc(hidden)] // Not public API.
    pub fn translate_inner(&mut self, name: &str, bytecode: &Bytecode<'_>) -> Result<B::FuncId> {
        ensure!(cfg!(target_endian = "little"), "only little-endian is supported");
        let _t = self.remarks.time(|r| &r.translate);
        ensure!(self.backend.function_name_is_unique(name), "function name `{name}` is not unique");

        // Use bytecode.txt as the debug info source file.
        if self.config.debug
            && let Some(dump_dir) = &self.dump_dir()
        {
            let path = dump_dir.join("bytecode.txt");
            let mut config = self.backend.config().clone();
            config.debug_file = Some(path);
            self.backend.apply_config(config);
        }

        let linkage = Linkage::Public;
        let (bcx, id) =
            Self::make_builder(&mut self.backend, &self.config, bytecode, name, linkage)?;
        FunctionCx::translate(bcx, self.config, &mut self.builtins, bytecode)?;
        Ok(id)
    }

    #[instrument(level = "debug", skip_all)]
    fn finalize(&mut self) -> Result<()> {
        if self.finalized {
            return Ok(());
        }

        let finalize_start = Instant::now();

        // Finalize debug info before any verification or code generation.
        self.backend.finalize_debug_info()?;

        let dump_dir = self.dump_dir();

        if let Some(dump_dir) = &dump_dir {
            let path = dump_dir.join("unopt").with_extension(self.backend.ir_extension());
            self.dump_ir(&path)?;

            // Dump IR before verifying for better debugging.
            self.verify_module()?;

            if self.dump_assembly && self.dump_unopt_assembly {
                let path = dump_dir.join("unopt.s");
                self.dump_disasm(&path)?;
                if self.config.debug {
                    let src_path = dump_dir.join("bytecode.txt");
                    if src_path.exists() {
                        Self::annotate_asm(&path, &src_path)?;
                    }
                }
            }
        } else {
            self.verify_module()?;
        }

        self.optimize_module()?;

        if let Some(dump_dir) = &dump_dir {
            let path = dump_dir.join("opt").with_extension(self.backend.ir_extension());
            self.dump_ir(&path)?;

            if self.dump_assembly {
                let path = dump_dir.join("opt.s");
                self.dump_disasm(&path)?;
                if self.config.debug {
                    let src_path = dump_dir.join("bytecode.txt");
                    if src_path.exists() {
                        Self::annotate_asm(&path, &src_path)?;
                    }
                }
            }
        }
        self.finalized = true;

        let finalize_total = &self.remarks.finalize_total;
        finalize_total.set(finalize_total.get() + finalize_start.elapsed());

        Ok(())
    }

    #[instrument(level = "debug", skip_all)]
    fn make_builder<'a>(
        backend: &'a mut B,
        config: &FcxConfig,
        bytecode: &Bytecode<'_>,
        name: &str,
        linkage: Linkage,
    ) -> Result<(B::Builder<'a>, B::FuncId)> {
        fn size_align<T>(i: usize) -> (usize, usize, usize) {
            (i, mem::size_of::<T>(), mem::align_of::<T>())
        }

        let i8 = backend.type_int(8);
        let ptr = backend.type_ptr();
        let (ret, params, param_names, ptr_attrs) = (
            Some(i8),
            &[ptr, ptr, ptr],
            &["arg.ecx.addr", "arg.stack.addr", "arg.stack_len.addr"],
            &[size_align::<EvmContext<'_>>(0), size_align::<EvmStack>(1), size_align::<usize>(2)],
        );
        debug_assert_eq!(params.len(), param_names.len());
        let (mut bcx, id) = backend.build_function(name, ret, params, param_names, linkage)?;

        // Function attributes.
        let function_attributes = default_attrs::for_fn()
            .chain(config.frame_pointers.then_some(Attribute::AllFramePointers));
        for attr in function_attributes {
            bcx.add_function_attribute(None, attr, FunctionAttributeLocation::Function);
        }

        // Pointer argument attributes.
        for &(i, size, align) in ptr_attrs {
            let attrs = default_attrs::for_sized_ref((size, align));
            for attr in attrs {
                let loc = FunctionAttributeLocation::Param(i as _);
                bcx.add_function_attribute(None, attr, loc);
            }
        }

        // The stack argument's contents are dead after return when the stack is not observed,
        // so the caller can elide any stores to the buffer.
        if !bytecode.stack_observed() {
            for param in 1..=2 {
                bcx.add_function_attribute(
                    None,
                    Attribute::DeadOnReturn,
                    FunctionAttributeLocation::Param(param),
                );
            }
        }

        Ok((bcx, id))
    }

    #[instrument(level = "debug", skip_all)]
    fn dump_ir(&mut self, path: &Path) -> Result<()> {
        self.backend.dump_ir(path)?;
        if self.config.debug
            && let Some(dump_dir) = &self.dump_dir()
        {
            let src_path = dump_dir.join("bytecode.txt");
            if src_path.exists() {
                Self::annotate_ir(path, &src_path)?;
            }
        }
        Ok(())
    }

    #[instrument(level = "debug", skip_all)]
    fn dump_disasm(&mut self, path: &Path) -> Result<()> {
        self.backend.dump_disasm(path)
    }

    #[instrument(level = "debug", skip_all)]
    fn verify_module(&mut self) -> Result<()> {
        if !self.config.debug_assertions {
            return Ok(());
        }
        let _t = self.remarks.time(|r| &r.verify);
        self.backend.verify_module()
    }

    #[instrument(level = "debug", skip_all)]
    fn optimize_module(&mut self) -> Result<()> {
        let _t = self.remarks.time(|r| &r.optimize);
        self.backend.optimize_module()
    }

    fn dump_remarks(&self, dump_dir: &Path) -> Result<()> {
        let r = &self.remarks;
        let parse = r.parse.get();
        let translate = r.translate.get();
        let finalize = r.finalize_total.get();
        let verify = r.verify.get();
        let optimize = r.optimize.get();
        let codegen = r.codegen.get();
        let total = parse + translate + finalize + codegen;
        let file = fs::File::create(dump_dir.join("remarks.txt"))?;
        let mut w = io::BufWriter::new(file);
        write!(
            w,
            "\
Compilation remarks
===================

parse:      {parse:>11.3?}
translate:  {translate:>11.3?}
finalize:   {finalize:>11.3?}
- verify:   {verify:>11.3?}
- optimize: {optimize:>11.3?}
codegen:    {codegen:>11.3?}

total:      {total:>11.3?}
"
        )?;

        // Display sizes of generated files.
        let mut files: Vec<_> = fs::read_dir(dump_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter(|e| e.file_name() != "remarks.txt")
            .collect();
        if !files.is_empty() {
            files.sort_by_key(|e| e.file_name());
            writeln!(w)?;
            writeln!(w, "Generated files")?;
            writeln!(w, "===============")?;
            for entry in &files {
                let name = entry.file_name();
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                writeln!(w, "{}: {}", name.to_string_lossy(), format_bytes(size as usize))?;
            }
        }

        // Cumulative JIT code sizes (across all jit_function calls in this finalize cycle).
        if !self.compiled_sizes.is_empty() {
            let total: usize = self.compiled_sizes.iter().map(|(_, s)| *s).sum();
            writeln!(w)?;
            writeln!(w, "JIT code sizes (estimated)")?;
            writeln!(w, "==========================")?;
            for (name, size) in &self.compiled_sizes {
                writeln!(w, "{name}: {}", format_bytes(*size))?;
            }
            if self.compiled_sizes.len() > 1 {
                writeln!(w, "total: {}", format_bytes(total))?;
            }
        }

        w.flush()?;
        Ok(())
    }

    /// Pull the latest compiled function sizes from the backend and merge them into
    /// `compiled_sizes`, deduplicating by name (later entries overwrite earlier ones).
    fn record_compiled_sizes(&mut self) {
        for (name, size) in self.backend.function_sizes() {
            if let Some(slot) = self.compiled_sizes.iter_mut().find(|(n, _)| *n == name) {
                slot.1 = size;
            } else {
                self.compiled_sizes.push((name, size));
            }
        }
    }

    #[instrument(level = "debug", skip_all)]
    fn dump_bytecode(dump_dir: &Path, bytecode: &Bytecode<'_>) -> Result<()> {
        {
            let file = fs::File::create(dump_dir.join("bytecode.txt"))?;
            let mut writer = io::BufWriter::new(file);
            write!(writer, "{bytecode}")?;
            writer.flush()?;
        }

        {
            let file = fs::File::create(dump_dir.join("bytecode.dbg.txt"))?;
            let mut writer = io::BufWriter::new(file);
            writeln!(writer, "{bytecode:#?}")?;
            writer.flush()?;
        }

        fs::write(dump_dir.join("bytecode.bin"), &*bytecode.code)?;

        {
            let file = fs::File::create(dump_dir.join("bytecode.dot"))?;
            let mut writer = io::BufWriter::new(file);
            let mut dot = String::new();
            bytecode.write_dot(&mut dot).map_err(|e| revmc_backend::eyre::eyre!("{e}"))?;
            writer.write_all(dot.as_bytes())?;
            writer.flush()?;
        }

        Ok(())
    }

    /// Rewrites an IR dump file in-place, appending bytecode source lines as comments.
    ///
    /// For each IR instruction with `!dbg !N`, resolves the `!DILocation(line: L, ...)`
    /// metadata and appends `; >> <source line>`.
    fn annotate_ir(ir_path: &Path, src_path: &Path) -> Result<()> {
        let src = fs::read_to_string(src_path)?;
        let src_lines: Vec<&str> = src.lines().collect();
        let ir = fs::read_to_string(ir_path)?;

        // Parse `!N = !DILocation(line: L, ...)` metadata.
        let mut di_locs = FxHashMap::default();
        for line in ir.lines() {
            let line = line.trim_start();
            if !line.starts_with('!') {
                continue;
            }
            // Match: !N = !DILocation(line: L, ...)
            let Some(rest) = line.strip_prefix('!') else { continue };
            let Some((id_str, rest)) = rest.split_once(" = !DILocation(line: ") else { continue };
            let Ok(id) = id_str.parse::<u32>() else { continue };
            let Some((line_str, _)) = rest.split_once(',') else { continue };
            let Ok(line_no) = line_str.parse::<u32>() else { continue };
            di_locs.insert(id, line_no);
        }

        if di_locs.is_empty() {
            return Ok(());
        }

        // Collect lines and find max width for comment alignment.
        let annotated: Vec<_> =
            ir.lines().map(|line| (line, resolve_dbg_line(line, &di_locs, &src_lines))).collect();
        let comment_col = annotated
            .iter()
            .filter_map(|(line, src)| src.is_some().then_some(line.len()))
            .max()
            .unwrap_or(0)
            .min(100);

        // Rewrite the file with aligned annotations.
        let file = fs::File::create(ir_path)?;
        let mut w = io::BufWriter::new(file);
        for (line, src_line) in &annotated {
            if let Some(src_line) = src_line {
                writeln!(w, "{line:<comment_col$} ; >> {src_line}")?;
            } else {
                writeln!(w, "{line}")?;
            }
        }
        w.flush()?;
        Ok(())
    }

    /// Rewrites an assembly dump file in-place, replacing `# bytecode.txt:LL:CC` comments
    /// with the corresponding source line from `bytecode.txt`.
    fn annotate_asm(asm_path: &Path, src_path: &Path) -> Result<()> {
        let src = fs::read_to_string(src_path)?;
        let src_lines: Vec<&str> = src.lines().collect();
        let asm = fs::read_to_string(asm_path)?;

        let annotated: Vec<_> = asm
            .lines()
            .map(|line| {
                let resolved = resolve_asm_source_line(line, &src_lines);
                // Strip the original `# bytecode.txt:...` comment when we have a resolved line.
                let stripped = if resolved.is_some() {
                    line.find("# bytecode.txt:").map(|pos| line[..pos].trim_end()).unwrap_or(line)
                } else {
                    line
                };
                (stripped, resolved)
            })
            .collect();
        let comment_col = 40;

        let file = fs::File::create(asm_path)?;
        let mut w = io::BufWriter::new(file);
        for (line, src_line) in &annotated {
            if let Some(src_line) = src_line {
                writeln!(w, "{line:<comment_col$} # {src_line}")?;
            } else {
                writeln!(w, "{line}")?;
            }
        }
        w.flush()?;
        Ok(())
    }

    /// Returns the dump directory, if set.
    #[doc(hidden)]
    pub fn dump_dir(&self) -> Option<PathBuf> {
        let mut dump_dir = self.out_dir.clone()?;
        if let Some(name) = &self.name {
            dump_dir.push(name.replace(char::is_whitespace, "_"));
        }
        if !dump_dir.exists() {
            let _ = fs::create_dir_all(&dump_dir);
        }
        Some(dump_dir)
    }
}

/// [`EvmCompiler`] input.
#[allow(missing_debug_implementations)]
pub enum EvmCompilerInput<'a> {
    /// EVM bytecode.
    Code(&'a [u8]),
}

impl<'a> From<&'a [u8]> for EvmCompilerInput<'a> {
    fn from(code: &'a [u8]) -> Self {
        EvmCompilerInput::Code(code)
    }
}

impl<'a> From<&'a Vec<u8>> for EvmCompilerInput<'a> {
    fn from(code: &'a Vec<u8>) -> Self {
        EvmCompilerInput::Code(code)
    }
}

impl<'a> From<&'a Bytes> for EvmCompilerInput<'a> {
    fn from(code: &'a Bytes) -> Self {
        EvmCompilerInput::Code(code)
    }
}

#[allow(dead_code)]
mod default_attrs {
    use revmc_backend::Attribute;

    pub(crate) fn for_fn() -> impl Iterator<Item = Attribute> {
        [
            Attribute::WillReturn,      // Always returns.
            Attribute::NoSync,          // No thread synchronization.
            Attribute::NativeTargetCpu, // Optimization.
            Attribute::NoRecurse,       // Revm is not recursive.
            Attribute::NonLazyBind,     // Skip PLT indirection.
            Attribute::UWTable,         // Unwind tables for profilers/debuggers.
        ]
        .into_iter()
    }

    pub(crate) fn for_param() -> impl Iterator<Item = Attribute> {
        [Attribute::NoUndef].into_iter()
    }

    pub(crate) fn for_ptr() -> impl Iterator<Item = Attribute> {
        for_param().chain([Attribute::NoCapture])
    }

    pub(crate) fn for_sized_ptr((size, align): (usize, usize)) -> impl Iterator<Item = Attribute> {
        for_ptr().chain([Attribute::Dereferenceable(size as u64), Attribute::Align(align as u64)])
    }

    pub(crate) fn for_ptr_t<T>() -> impl Iterator<Item = Attribute> {
        for_sized_ptr(size_align::<T>())
    }

    pub(crate) fn for_ref() -> impl Iterator<Item = Attribute> {
        for_ptr().chain([Attribute::NonNull, Attribute::NoAlias])
    }

    pub(crate) fn for_sized_ref((size, align): (usize, usize)) -> impl Iterator<Item = Attribute> {
        for_ref().chain([Attribute::Dereferenceable(size as u64), Attribute::Align(align as u64)])
    }

    pub(crate) fn for_ref_t<T>() -> impl Iterator<Item = Attribute> {
        for_sized_ref(size_align::<T>())
    }

    pub(crate) fn size_align<T>() -> (usize, usize) {
        (std::mem::size_of::<T>(), std::mem::align_of::<T>())
    }
}

/// Resolves a `# bytecode.txt:LL:CC` or `# bytecode.txt:LL` comment in an assembly line
/// to the corresponding source line.
fn resolve_asm_source_line<'a>(line: &str, src_lines: &[&'a str]) -> Option<&'a str> {
    let pos = line.find("# bytecode.txt:")?;
    let after = &line[pos + "# bytecode.txt:".len()..];
    let num_len = after.find(|c: char| !c.is_ascii_digit()).unwrap_or(after.len());
    let line_no: u32 = after[..num_len].parse().ok()?;
    let idx = line_no.checked_sub(1)? as usize;
    let src_line = src_lines.get(idx)?.trim();
    (!src_line.is_empty()).then_some(src_line)
}

/// Resolves a `!dbg !N` reference in an IR line to the corresponding source line.
fn resolve_dbg_line<'a>(
    ir_line: &str,
    di_locs: &FxHashMap<u32, u32>,
    src_lines: &[&'a str],
) -> Option<&'a str> {
    let pos = ir_line.find("!dbg !")?;
    let after = &ir_line[pos + 6..];
    let id_len = after.find(|c: char| !c.is_ascii_digit()).unwrap_or(after.len());
    let id: u32 = after[..id_len].parse().ok()?;
    let line_no = *di_locs.get(&id)?;
    let idx = line_no.checked_sub(1)? as usize;
    let src_line = src_lines.get(idx)?.trim();
    (!src_line.is_empty()).then_some(src_line)
}
