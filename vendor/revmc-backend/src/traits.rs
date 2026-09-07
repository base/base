use std::{
    fmt,
    path::{Path, PathBuf},
};

use ruint::aliases::U256;

use crate::{Pointer, Result};

/// Backend configuration.
///
/// Collects all tuneable settings that [`EvmCompiler`](crate::Backend) forwards to the backend.
/// Backends receive a full snapshot via [`Backend::apply_config`] whenever the compiler
/// changes a setting, so they can apply side-effects (e.g. toggling ASM verbosity) in one place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendConfig {
    /// Optimization level.
    pub opt_level: OptimizationLevel,
    /// Whether IR output is being dumped (enables verbose names, asm comments, etc.).
    pub is_dumping: bool,
    /// Whether to enable debug assertions in generated code.
    pub debug_assertions: bool,
    /// Whether to enable JIT debug support (GDB/LLDB registration).
    ///
    /// Applied once per process on first JIT compilation.
    pub debug_support: bool,
    /// Whether to enable JIT profiling support (perf jitdump).
    ///
    /// Applied once per process on first JIT compilation.
    pub profiling_support: bool,
    /// Whether to enable the simple perf map plugin.
    ///
    /// Writes `/tmp/perf-<pid>.map` in the perf map format so that profilers
    /// can resolve JIT-compiled symbols without the jitdump machinery.
    ///
    /// Not suitable for long-running programs. The map file is append-only
    /// and never cleaned up, so entries for freed JIT code accumulate
    /// indefinitely. Prefer [`profiling_support`](Self::profiling_support)
    /// (jitdump) for long-lived processes.
    ///
    /// Applied once per process on first JIT compilation.
    pub simple_perf: bool,
    /// Debug info source file path. `Some` enables debug info emission.
    pub debug_file: Option<PathBuf>,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            opt_level: OptimizationLevel::Default,
            is_dumping: false,
            debug_assertions: cfg!(debug_assertions),
            debug_support: true,
            profiling_support: false,
            simple_perf: true,
            debug_file: None,
        }
    }
}

/// Optimization level.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OptimizationLevel {
    /// No optimizations.
    None,
    /// Less optimizations.
    Less,
    /// Default optimizations. Highly recommended.
    #[default]
    Default,
    /// Aggressive optimizations.
    ///
    /// Not recommended, since it's generally a lot slower and doesn't really produce better code
    /// than the default level.
    Aggressive,
}

impl std::str::FromStr for OptimizationLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "0" | "none" => Self::None,
            "1" | "less" => Self::Less,
            "2" | "default" => Self::Default,
            "3" | "aggressive" => Self::Aggressive,
            _ => return Err(format!("unknown optimization level: {s}")),
        })
    }
}

/// Integer comparison condition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IntCC {
    /// `==`.
    Equal,
    /// `!=`.
    NotEqual,
    /// Signed `<`.
    SignedLessThan,
    /// Signed `>=`.
    SignedGreaterThanOrEqual,
    /// Signed `>`.
    SignedGreaterThan,
    /// Signed `<=`.
    SignedLessThanOrEqual,
    /// Unsigned `<`.
    UnsignedLessThan,
    /// Unsigned `>=`.
    UnsignedGreaterThanOrEqual,
    /// Unsigned `>`.
    UnsignedGreaterThan,
    /// Unsigned `<=`.
    UnsignedLessThanOrEqual,
}

/// Function or parameter attribute.
///
/// Mostly copied from [LLVM](https://llvm.org/docs/LangRef.html).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Attribute {
    // Function attributes.
    WillReturn,
    NoReturn,
    NoFree,
    NoRecurse,
    NoSync,
    NoUnwind,
    NonLazyBind,
    UWTable,
    AllFramePointers,
    NativeTargetCpu,
    Cold,
    Hot,
    HintInline,
    AlwaysInline,
    NoInline,
    Speculatable,

    // Parameter attributes.
    NoAlias,
    NoCapture,
    NoUndef,
    Align(u64),
    NonNull,
    Dereferenceable(u64),
    /// Size of the return type in bytes.
    SRet(u64),
    ReadNone,
    ReadOnly,
    WriteOnly,
    Writable,
    /// `memory(argmem: readwrite)` — function only accesses memory through pointer arguments.
    ArgMemOnly,
    /// `initializes((0, N))` — function initializes bytes `[0, N)` through this pointer.
    Initializes(u64),
    /// `dead_on_return` — the contents of the pointed-to memory are dead after the function
    /// returns, allowing the caller to elide stores to the memory.
    DeadOnReturn,
    // TODO: Range?
}

/// Linkage type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Linkage {
    /// Defined outside of the module.
    Import,
    /// Defined in the module and visible outside.
    Public,
    /// Defined in the module, but not visible outside.
    Private,
}

/// Determines where on a function an attribute is assigned to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FunctionAttributeLocation {
    /// Assign to the function's return type.
    Return,
    /// Assign to one of the function's params (0-indexed).
    Param(u32),
    /// Assign to the function itself.
    Function,
}

/// Calling convention.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CallConv {
    #[default]
    Default,
    /// Preserve most caller registers across the call to reduce callsite register pressure.
    Cold,
}

/// Tail call kind.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub enum TailCallKind {
    #[default]
    None,
    Tail,
    MustTail,
    NoTail,
}

pub trait BackendTypes: Sized {
    type Type: Copy + Eq + fmt::Debug;
    type Value: Copy + Eq + fmt::Debug;
    type StackSlot: Copy + Eq + fmt::Debug;
    type BasicBlock: Copy + Eq + fmt::Debug;
    type Function: Copy + Eq + fmt::Debug;
}

#[allow(clippy::missing_safety_doc)]
pub trait Backend: BackendTypes + TypeMethods {
    type Builder<'a>: Builder<
            Type = Self::Type,
            Value = Self::Value,
            StackSlot = Self::StackSlot,
            BasicBlock = Self::BasicBlock,
            Function = Self::Function,
        >
    where
        Self: 'a;
    type FuncId: Copy + Eq + std::hash::Hash + fmt::Debug;

    fn ir_extension(&self) -> &'static str;

    fn set_module_name(&mut self, name: &str);

    /// Returns the current backend configuration.
    fn config(&self) -> &BackendConfig;

    /// Applies the given configuration snapshot.
    ///
    /// Backends should apply any side-effects (e.g. toggling ASM verbosity) here.
    fn apply_config(&mut self, config: BackendConfig);

    /// Finalizes any pending debug info metadata.
    ///
    /// Must be called before verification, optimization, or code emission.
    fn finalize_debug_info(&mut self) -> Result<()> {
        Ok(())
    }
    fn dump_ir(&mut self, path: &Path) -> Result<()>;
    fn dump_disasm(&mut self, path: &Path) -> Result<()>;

    fn is_aot(&self) -> bool;

    fn function_name_is_unique(&self, name: &str) -> bool;

    fn build_function(
        &mut self,
        name: &str,
        ret: Option<Self::Type>,
        params: &[Self::Type],
        param_names: &[&str],
        linkage: Linkage,
    ) -> Result<(Self::Builder<'_>, Self::FuncId)>;
    fn verify_module(&mut self) -> Result<()>;
    fn optimize_module(&mut self) -> Result<()>;
    fn write_object<W: std::io::Write>(&mut self, w: W) -> Result<()>;
    fn jit_function(&mut self, id: Self::FuncId) -> Result<usize>;

    /// Returns the name of a compiled function by its ID.
    fn function_name(&self, id: Self::FuncId) -> Option<&str>;

    /// Returns the estimated sizes of compiled functions as `(name, size)` pairs.
    fn function_sizes(&self) -> Vec<(String, usize)> {
        Vec::new()
    }

    /// Clears the IR module, freeing memory used by IR representations.
    ///
    /// This does **not** free JIT-compiled machine code, so previously obtained function pointers
    /// remain valid. The module is left in a state where new functions can be translated.
    fn clear_ir(&mut self) -> Result<()>;

    unsafe fn free_function(&mut self, id: Self::FuncId) -> Result<()>;
    unsafe fn free_all_functions(&mut self) -> Result<()>;
}

pub trait TypeMethods: BackendTypes {
    fn type_ptr(&self) -> Self::Type;
    fn type_ptr_sized_int(&self) -> Self::Type;
    fn type_int(&self, bits: u32) -> Self::Type;
    fn type_array(&self, ty: Self::Type, size: u32) -> Self::Type;
    fn type_bit_width(&self, ty: Self::Type) -> u32;
}

pub trait Builder: BackendTypes + TypeMethods {
    fn create_block(&mut self, name: &str) -> Self::BasicBlock;
    fn create_block_after(&mut self, after: Self::BasicBlock, name: &str) -> Self::BasicBlock;
    fn switch_to_block(&mut self, block: Self::BasicBlock);
    fn seal_block(&mut self, block: Self::BasicBlock);
    fn seal_all_blocks(&mut self);
    fn set_current_block_cold(&mut self);
    fn current_block(&mut self) -> Option<Self::BasicBlock>;
    fn block_addr(&mut self, block: Self::BasicBlock) -> Option<Self::Value>;

    fn add_comment_to_current_inst(&mut self, comment: &str);

    /// Sets the current debug source location for subsequently emitted instructions.
    fn set_debug_location(&mut self, _line: u32, _col: u32) {}

    /// Clears the current debug source location.
    fn clear_debug_location(&mut self) {}

    fn fn_param(&mut self, index: usize) -> Self::Value;
    fn num_fn_params(&self) -> usize;

    fn bool_const(&mut self, value: bool) -> Self::Value;
    /// Sign-extends negative values to `ty`.
    fn iconst(&mut self, ty: Self::Type, value: i64) -> Self::Value;
    fn uconst(&mut self, ty: Self::Type, value: u64) -> Self::Value;
    fn iconst_256(&mut self, value: impl TryInto<U256>) -> Self::Value;
    fn cstr_const(&mut self, value: &std::ffi::CStr) -> Self::Value {
        self.str_const(value.to_str().unwrap())
    }
    fn str_const(&mut self, value: &str) -> Self::Value;
    fn nullptr(&mut self) -> Self::Value;

    fn new_stack_slot(&mut self, ty: Self::Type, name: &str) -> Pointer<Self> {
        Pointer::new_stack_slot(self, ty, name)
    }
    fn new_stack_slot_raw(&mut self, ty: Self::Type, name: &str) -> Self::StackSlot;
    fn stack_load(&mut self, ty: Self::Type, slot: Self::StackSlot, name: &str) -> Self::Value;
    fn stack_store(&mut self, value: Self::Value, slot: Self::StackSlot);
    fn stack_addr(&mut self, ty: Self::Type, slot: Self::StackSlot) -> Self::Value;

    /// Loads a value from a pointer, assuming natural alignment.
    fn load(&mut self, ty: Self::Type, ptr: Self::Value, name: &str) -> Self::Value;
    /// Loads a value from a pointer with an explicit alignment override.
    fn load_aligned(
        &mut self,
        ty: Self::Type,
        ptr: Self::Value,
        align: usize,
        name: &str,
    ) -> Self::Value;
    /// Stores a value to a pointer, assuming natural alignment.
    fn store(&mut self, value: Self::Value, ptr: Self::Value);
    /// Stores a value to a pointer with an explicit alignment override.
    fn store_aligned(&mut self, value: Self::Value, ptr: Self::Value, align: usize);

    fn nop(&mut self);
    fn ret(&mut self, values: &[Self::Value]);
    fn assume(&mut self, cond: Self::Value) {
        let _ = cond;
    }

    fn icmp(&mut self, cond: IntCC, lhs: Self::Value, rhs: Self::Value) -> Self::Value;
    fn icmp_imm(&mut self, cond: IntCC, lhs: Self::Value, rhs: i64) -> Self::Value;
    fn is_null(&mut self, ptr: Self::Value) -> Self::Value;
    fn is_not_null(&mut self, ptr: Self::Value) -> Self::Value;

    fn br(&mut self, dest: Self::BasicBlock);
    fn brif(
        &mut self,
        cond: Self::Value,
        then_block: Self::BasicBlock,
        else_block: Self::BasicBlock,
    );
    fn brif_cold(
        &mut self,
        cond: Self::Value,
        then_block: Self::BasicBlock,
        else_block: Self::BasicBlock,
        then_is_cold: bool,
    ) {
        let _ = then_is_cold;
        self.brif(cond, then_block, else_block)
    }
    fn switch(
        &mut self,
        index: Self::Value,
        default: Self::BasicBlock,
        targets: &[(u64, Self::BasicBlock)],
        default_is_cold: bool,
    );
    fn br_indirect(&mut self, address: Self::Value, destinations: &[Self::BasicBlock]);
    fn phi(&mut self, ty: Self::Type, incoming: &[(Self::Value, Self::BasicBlock)]) -> Self::Value;
    fn select(
        &mut self,
        cond: Self::Value,
        then_value: Self::Value,
        else_value: Self::Value,
    ) -> Self::Value;

    fn iadd(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value;
    fn isub(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value;
    fn imul(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value;
    fn udiv(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value;
    fn sdiv(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value;
    fn urem(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value;
    fn srem(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value;

    fn iadd_imm(&mut self, lhs: Self::Value, rhs: i64) -> Self::Value;
    fn isub_imm(&mut self, lhs: Self::Value, rhs: i64) -> Self::Value;
    fn imul_imm(&mut self, lhs: Self::Value, rhs: i64) -> Self::Value;

    // `(result, overflow)`
    fn uadd_overflow(&mut self, lhs: Self::Value, rhs: Self::Value) -> (Self::Value, Self::Value);
    fn usub_overflow(&mut self, lhs: Self::Value, rhs: Self::Value) -> (Self::Value, Self::Value);

    fn uadd_sat(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value;

    fn umax(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value;
    fn umin(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value;
    fn bswap(&mut self, value: Self::Value) -> Self::Value;

    fn bitor(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value;
    fn bitand(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value;
    fn bitxor(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value;
    fn bitnot(&mut self, value: Self::Value) -> Self::Value;
    fn clz(&mut self, value: Self::Value) -> Self::Value;

    fn bitor_imm(&mut self, lhs: Self::Value, rhs: i64) -> Self::Value;
    fn bitand_imm(&mut self, lhs: Self::Value, rhs: i64) -> Self::Value;
    fn bitxor_imm(&mut self, lhs: Self::Value, rhs: i64) -> Self::Value;

    fn ishl(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value;
    fn ushr(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value;
    fn sshr(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value;

    fn zext(&mut self, ty: Self::Type, value: Self::Value) -> Self::Value;
    fn sext(&mut self, ty: Self::Type, value: Self::Value) -> Self::Value;
    #[doc(alias = "trunc")]
    fn ireduce(&mut self, to: Self::Type, value: Self::Value) -> Self::Value;

    /// Converts an integer value to a pointer.
    fn inttoptr(&mut self, value: Self::Value, ty: Self::Type) -> Self::Value;

    fn gep(
        &mut self,
        ty: Self::Type,
        ptr: Self::Value,
        indexes: &[Self::Value],
        name: &str,
    ) -> Self::Value;

    #[must_use]
    fn call(&mut self, function: Self::Function, args: &[Self::Value]) -> Option<Self::Value> {
        self.tail_call(function, args, TailCallKind::None)
    }
    #[must_use]
    fn tail_call(
        &mut self,
        function: Self::Function,
        args: &[Self::Value],
        tail_call: TailCallKind,
    ) -> Option<Self::Value>;

    /// Returns `Some(is_value_compile_time)`, or `None` if unsupported.
    fn is_compile_time_known(&mut self, value: Self::Value) -> Option<Self::Value>;

    fn memcpy(&mut self, dst: Self::Value, src: Self::Value, len: Self::Value);
    fn memcpy_inline(&mut self, dst: Self::Value, src: Self::Value, len: i64) {
        let len = self.iconst(self.type_int(64), len);
        self.memcpy(dst, src, len);
    }

    fn unreachable(&mut self);

    fn get_or_build_function(
        &mut self,
        name: &str,
        params: &[Self::Type],
        ret: Option<Self::Type>,
        linkage: Linkage,
        build: impl FnOnce(&mut Self),
    ) -> Self::Function;

    fn get_function(&mut self, name: &str) -> Option<Self::Function>;

    fn get_printf_function(&mut self) -> Self::Function;

    /// Adds a function to the module that's located at `address`.
    ///
    /// If `address` is `None`, the function must be built.
    fn add_function(
        &mut self,
        name: &str,
        params: &[Self::Type],
        ret: Option<Self::Type>,
        address: Option<usize>,
        linkage: Linkage,
        call_conv: CallConv,
    ) -> Self::Function;

    /// Adds a local stub with the given calling convention for an existing function.
    fn add_function_stub(
        &mut self,
        function: Self::Function,
        call_conv: CallConv,
    ) -> Self::Function {
        let _ = call_conv;
        function
    }

    /// Adds an attribute to a function, one of its parameters, or its return value.
    ///
    /// If `function` is `None`, the attribute is added to the current function.
    fn add_function_attribute(
        &mut self,
        function: Option<Self::Function>,
        attribute: Attribute,
        loc: FunctionAttributeLocation,
    );
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, str::FromStr};

    use super::{BackendConfig, OptimizationLevel};

    #[test]
    fn backend_config_defaults_match_runtime_settings() {
        let config = BackendConfig::default();

        assert_eq!(config.opt_level, OptimizationLevel::Default);
        assert!(!config.is_dumping);
        assert_eq!(config.debug_assertions, cfg!(debug_assertions));
        assert!(config.debug_support);
        assert!(!config.profiling_support);
        assert!(config.simple_perf);
        assert_eq!(config.debug_file, None);
    }

    #[test]
    fn backend_config_is_cloneable_and_comparable() {
        let config = BackendConfig {
            opt_level: OptimizationLevel::Aggressive,
            is_dumping: true,
            debug_assertions: true,
            debug_support: false,
            profiling_support: true,
            simple_perf: false,
            debug_file: Some(PathBuf::from("debug.sol")),
        };

        assert_eq!(config.clone(), config);
    }

    #[test]
    fn optimization_level_parses_numeric_and_named_values() {
        for (input, expected) in [
            ("0", OptimizationLevel::None),
            ("none", OptimizationLevel::None),
            ("1", OptimizationLevel::Less),
            ("less", OptimizationLevel::Less),
            ("2", OptimizationLevel::Default),
            ("default", OptimizationLevel::Default),
            ("3", OptimizationLevel::Aggressive),
            ("aggressive", OptimizationLevel::Aggressive),
        ] {
            assert_eq!(OptimizationLevel::from_str(input), Ok(expected));
        }
    }

    #[test]
    fn optimization_level_reports_unknown_values() {
        assert_eq!(
            OptimizationLevel::from_str("fast").unwrap_err(),
            "unknown optimization level: fast"
        );
    }
}
