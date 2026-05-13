//! Production EVM-backed [`PrecompileStorageProvider`].
//!
//! TODO: Implement a production-grade EVM storage provider backed by the live revm journal.
//! The base repo uses alloy-evm 0.27.x which doesn't expose `EvmInternals` (a Tempo-specific
//! abstraction from alloy-evm 0.34). The production binding will be implemented when the
//! precompile integration layer (how precompiles hook into the base node's EVM) is defined.
//!
//! For development and testing, use [`crate::hashmap::HashMapStorageProvider`].

// Production EVM provider will be added in a follow-up when the EVM integration layer is defined.
