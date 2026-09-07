//! Public types and handle methods.

use std::sync::Arc;

use alloy_primitives::{B256, Bytes};
use revm_primitives::hardfork::SpecId;

use crate::{
    EvmCompilerFn,
    runtime::{storage::RuntimeCacheKey, worker::JitCodeBacking},
};

/// Request to look up a compiled function.
#[derive(Clone, Debug)]
pub struct LookupRequest {
    /// The cache key (code hash + spec).
    pub key: RuntimeCacheKey,
    /// The raw contract bytecode.
    pub code: Bytes,
}

/// Result of a lookup.
#[derive(Clone, Debug)]
pub enum LookupDecision {
    /// A compiled function is available.
    Compiled(Arc<CompiledProgram>),
    /// The caller should interpret instead.
    Interpret(InterpretReason),
}

/// Reason why the runtime returned "interpret" instead of a compiled function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterpretReason {
    /// The runtime is disabled.
    Disabled,
    /// The contract is not eligible for compilation.
    Ineligible,
    /// No compiled program is ready for this key.
    NotReady,
    /// JIT compilation failed for this contract.
    JitFailed,
}

/// A compiled EVM program kept alive in the resident map.
pub struct CompiledProgram {
    /// The callable compiled function.
    pub func: EvmCompilerFn,
    /// The cache key this program was compiled for.
    pub key: RuntimeCacheKey,
    /// Whether this is an AOT or JIT program.
    pub kind: ProgramKind,
    /// Keeps the backing memory (shared library / JIT module) alive.
    _backing: ProgramBacking,
}

impl std::fmt::Debug for CompiledProgram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledProgram")
            .field("key", &self.key)
            .field("kind", &self.kind)
            .field("func", &self.func)
            .finish_non_exhaustive()
    }
}

impl CompiledProgram {
    /// Creates a new compiled program backed by a loaded shared library.
    pub(crate) fn new_aot(
        key: RuntimeCacheKey,
        func: EvmCompilerFn,
        library: Arc<LoadedLibrary>,
    ) -> Self {
        Self { key, kind: ProgramKind::Aot, func, _backing: ProgramBacking::LoadedLibrary(library) }
    }

    /// Creates a new compiled program backed by a JIT module's
    /// [`ResourceTracker`](crate::llvm::orc::ResourceTracker).
    pub(crate) fn new_jit(
        key: RuntimeCacheKey,
        func: EvmCompilerFn,
        backing: Arc<JitCodeBacking>,
    ) -> Self {
        Self { key, kind: ProgramKind::Jit, func, _backing: ProgramBacking::JitModule(backing) }
    }
}

/// Request to prepare an AOT artifact.
#[derive(Clone, Debug)]
pub struct AotRequest {
    /// The code hash of the contract bytecode.
    pub code_hash: B256,
    /// The raw contract bytecode.
    pub code: Bytes,
    /// The EVM spec (hardfork) for compilation.
    pub spec_id: SpecId,
}

/// Whether this program was compiled AOT or JIT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramKind {
    /// Loaded from a pre-compiled artifact at startup.
    Aot,
    /// Compiled at runtime by the JIT.
    Jit,
}

/// Backing storage that keeps compiled code alive.
#[expect(dead_code, reason = "variant fields are held alive for Drop")]
enum ProgramBacking {
    /// A dynamically loaded shared library (AOT).
    LoadedLibrary(Arc<LoadedLibrary>),
    /// An ORCv2 ResourceTracker that owns JIT machine code.
    /// Dropping the last reference calls `tracker.remove()`, freeing the code.
    JitModule(Arc<JitCodeBacking>),
}

/// Owns a loaded shared library.
///
/// Dropping this unloads the library.
pub(crate) struct LoadedLibrary {
    /// The loaded library.
    _library: libloading::Library,
}

impl LoadedLibrary {
    /// Creates a new loaded library wrapper.
    pub(crate) fn new(library: libloading::Library) -> Self {
        Self { _library: library }
    }
}
