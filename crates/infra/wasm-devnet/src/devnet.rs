#[cfg(not(target_family = "wasm"))]
mod native;
#[cfg(not(target_family = "wasm"))]
pub use native::Devnet;

#[cfg(target_family = "wasm")]
mod wasm_impl;
#[cfg(target_family = "wasm")]
pub use wasm_impl::{
    DEV_KEY, DapQueue, Devnet, InMemoryDap, L1Block, L1ProviderError, L2ProviderError, SharedL1,
    WasmL1Provider, WasmL2Provider, address_from_verifying_key, l1_origin_from_attrs,
};
