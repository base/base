#![doc = include_str!("../README.md")]

mod contract;
pub(crate) use contract::{FieldInfo, FieldKind};

mod accounting;
mod layout;
mod namespace;
mod packing;
mod precompile;
mod storable;
mod storable_primitives;
mod storable_tests;
mod test_fields;
mod utils;

use proc_macro::TokenStream;
use syn::{DeriveInput, parse_macro_input};

/// Transforms a struct that represents a storage layout into a contract with helper methods to
/// easily interact with the EVM storage.
/// Its packing and encoding schemes aim to be an exact representation of the storage model used by Solidity.
#[proc_macro_attribute]
pub fn contract(attr: TokenStream, item: TokenStream) -> TokenStream {
    let config = parse_macro_input!(attr as contract::ContractConfig);
    let input = parse_macro_input!(item as DeriveInput);
    contract::generate(input, config.address.as_ref())
}

/// Namespaces a `#[contract]` storage struct or `Storable` layout using an ERC-7201 storage root.
#[proc_macro_attribute]
pub fn namespace(attr: TokenStream, item: TokenStream) -> TokenStream {
    namespace::expand(attr, item)
}

/// Generates EVM precompile constructor and optional singleton installation methods.
///
/// By default this expands through `crate::macros::base_precompile!` in the invoking crate. Callers
/// outside `base-common-precompiles` can pass `macro_path = path::to::wrapper_macro` to override the
/// runtime wrapper macro.
#[proc_macro_attribute]
pub fn precompile(attr: TokenStream, item: TokenStream) -> TokenStream {
    precompile::expand(attr, item)
}

/// Derives the `Storable` trait for structs with named fields and `#[repr(u8)]` unit enums.
#[proc_macro_derive(
    Storable,
    attributes(accessor, mutator, storable_arrays, namespace, storage_namespace)
)]
pub fn derive_storage_block(input: TokenStream) -> TokenStream {
    storable::derive(parse_macro_input!(input as DeriveInput))
}

/// Derives the B-20 `TokenAccounting` storage port for contract storage structs.
#[proc_macro_derive(TokenAccounting)]
pub fn derive_token_accounting(input: TokenStream) -> TokenStream {
    accounting::derive_token(parse_macro_input!(input as DeriveInput))
}

/// Derives the stablecoin storage port for contract storage structs.
#[proc_macro_derive(StablecoinAccounting)]
pub fn derive_stablecoin_accounting(input: TokenStream) -> TokenStream {
    accounting::derive_stablecoin(parse_macro_input!(input as DeriveInput))
}

/// Derives the security-token storage port for contract storage structs.
#[proc_macro_derive(SecurityAccounting)]
pub fn derive_security_accounting(input: TokenStream) -> TokenStream {
    accounting::derive_security(parse_macro_input!(input as DeriveInput))
}

/// Generate `StorableType` and `Storable` implementations for all standard integer types.
#[proc_macro]
pub fn storable_rust_ints(_input: TokenStream) -> TokenStream {
    storable_primitives::gen_storable_rust_ints().into()
}

/// Generate `StorableType` and `Storable` implementations for alloy integer types.
#[proc_macro]
pub fn storable_alloy_ints(_input: TokenStream) -> TokenStream {
    storable_primitives::gen_storable_alloy_ints().into()
}

/// Generate `StorableType` and `Storable` implementations for alloy `FixedBytes<N>` types.
#[proc_macro]
pub fn storable_alloy_bytes(_input: TokenStream) -> TokenStream {
    storable_primitives::gen_storable_alloy_bytes().into()
}

/// Generate comprehensive property tests for all storage types.
#[proc_macro]
pub fn gen_storable_tests(_input: TokenStream) -> TokenStream {
    storable_tests::gen_storable_tests().into()
}

/// Generate `Storable` implementations for fixed-size arrays of primitive types.
#[proc_macro]
pub fn storable_arrays(_input: TokenStream) -> TokenStream {
    storable_primitives::gen_storable_arrays().into()
}

/// Generate `Storable` implementations for nested arrays of small primitive types.
#[proc_macro]
pub fn storable_nested_arrays(_input: TokenStream) -> TokenStream {
    storable_primitives::gen_nested_arrays().into()
}

/// Test helper macro for validating slots.
#[proc_macro]
pub fn gen_test_fields_layout(input: TokenStream) -> TokenStream {
    test_fields::gen_layout(proc_macro2::TokenStream::from(input))
}

/// Test helper macro for validating struct field slots.
#[proc_macro]
pub fn gen_test_fields_struct(input: TokenStream) -> TokenStream {
    test_fields::gen_struct_fields(proc_macro2::TokenStream::from(input))
}
