#![doc = include_str!("../README.md")]

<<<<<<< HEAD
=======
mod contract;
pub(crate) use contract::{FieldInfo, FieldKind};

>>>>>>> d46df9d45d1cea16baddff35c768ecceea2e02d5
mod layout;
mod packing;
mod storable;
mod storable_primitives;
mod storable_tests;
<<<<<<< HEAD
mod utils;

use alloy_primitives::U256;
use proc_macro::TokenStream;
use quote::quote;
use syn::{
    Data, DeriveInput, Expr, Fields, Ident, Token, Type, Visibility,
    parse::{Parse, ParseStream, Parser},
    parse_macro_input,
    punctuated::Punctuated,
};

use crate::utils::extract_attributes;

struct ContractConfig {
    address: Option<Expr>,
}

impl Parse for ContractConfig {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        if input.is_empty() {
            return Ok(Self { address: None });
        }

        let ident: Ident = input.parse()?;
        if ident != "addr" && ident != "address" {
            return Err(syn::Error::new(ident.span(), "only `addr` attribute is supported"));
        }

        input.parse::<Token![=]>()?;
        let address: Expr = input.parse()?;

        Ok(Self { address: Some(address) })
    }
}

const RESERVED: &[&str] = &["address", "storage", "msg_sender"];
=======
mod test_fields;
mod utils;

use proc_macro::TokenStream;
use syn::{DeriveInput, parse_macro_input};
>>>>>>> d46df9d45d1cea16baddff35c768ecceea2e02d5

/// Transforms a struct that represents a storage layout into a contract with helper methods to
/// easily interact with the EVM storage.
/// Its packing and encoding schemes aim to be an exact representation of the storage model used by Solidity.
#[proc_macro_attribute]
pub fn contract(attr: TokenStream, item: TokenStream) -> TokenStream {
<<<<<<< HEAD
    let config = parse_macro_input!(attr as ContractConfig);
    let input = parse_macro_input!(item as DeriveInput);

    match gen_contract_output(input, config.address.as_ref()) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn gen_contract_output(
    input: DeriveInput,
    address: Option<&Expr>,
) -> syn::Result<proc_macro2::TokenStream> {
    let (ident, vis) = (input.ident.clone(), input.vis.clone());
    let fields = parse_fields(input)?;

    let storage_output = gen_contract_storage(&ident, &vis, &fields, address)?;
    Ok(quote! { #storage_output })
}

#[derive(Debug)]
struct FieldInfo {
    name: Ident,
    ty: Type,
    slot: Option<U256>,
    base_slot: Option<U256>,
}

#[derive(Debug, Clone, Copy)]
enum FieldKind<'a> {
    Direct(&'a Type),
    Mapping { key: &'a Type, value: &'a Type },
}

fn parse_fields(input: DeriveInput) -> syn::Result<Vec<FieldInfo>> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(&input.generics, "Contract structs cannot have generic parameters"));
    }

    let named_fields = if let Data::Struct(data) = input.data
        && let Fields::Named(fields) = data.fields
    {
        fields.named
    } else {
        return Err(syn::Error::new_spanned(input.ident, "Only structs with named fields are supported"));
    };

    named_fields.into_iter().map(|field| {
        let name = field.ident.as_ref()
            .ok_or_else(|| syn::Error::new_spanned(&field, "Fields must have names"))?;

        if RESERVED.contains(&name.to_string().as_str()) {
            return Err(syn::Error::new_spanned(name, format!("Field name '{name}' is reserved")));
        }

        let (slot, base_slot) = extract_attributes(&field.attrs)?;
        Ok(FieldInfo { name: name.to_owned(), ty: field.ty, slot, base_slot })
    }).collect()
}

fn gen_contract_storage(
    ident: &Ident,
    vis: &Visibility,
    fields: &[FieldInfo],
    address: Option<&Expr>,
) -> syn::Result<proc_macro2::TokenStream> {
    let allocated_fields = packing::allocate_slots(fields)?;
    let transformed_struct = layout::gen_struct(ident, vis, &allocated_fields);
    let storage_trait = layout::gen_contract_storage_impl(ident);
    let constructor = layout::gen_constructor(ident, &allocated_fields, address);
    let slots_module = layout::gen_slots_module(&allocated_fields);
    let default_impl = if address.is_some() {
        layout::gen_default_impl(ident)
    } else {
        proc_macro2::TokenStream::new()
    };

    Ok(quote! {
        #slots_module
        #transformed_struct
        #constructor
        #storage_trait
        #default_impl
    })
=======
    let config = parse_macro_input!(attr as contract::ContractConfig);
    let input = parse_macro_input!(item as DeriveInput);
    contract::generate(input, config.address.as_ref())
>>>>>>> d46df9d45d1cea16baddff35c768ecceea2e02d5
}

/// Derives the `Storable` trait for structs with named fields and `#[repr(u8)]` unit enums.
#[proc_macro_derive(Storable, attributes(storable_arrays))]
pub fn derive_storage_block(input: TokenStream) -> TokenStream {
<<<<<<< HEAD
    let input = parse_macro_input!(input as DeriveInput);

    match storable::derive_impl(input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
=======
    storable::derive(parse_macro_input!(input as DeriveInput))
>>>>>>> d46df9d45d1cea16baddff35c768ecceea2e02d5
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

<<<<<<< HEAD
/// Test helper macro for validating slots
#[proc_macro]
pub fn gen_test_fields_layout(input: TokenStream) -> TokenStream {
    let input = proc_macro2::TokenStream::from(input);

    let parser = syn::punctuated::Punctuated::<Ident, syn::Token![,]>::parse_terminated;
    let idents = match parser.parse2(input) {
        Ok(idents) => idents,
        Err(err) => return err.to_compile_error().into(),
    };

    let field_calls: Vec<_> = idents.into_iter().map(|ident| {
        let field_name = ident.to_string();
        let const_name = field_name.to_uppercase();
        let field_name = utils::to_camel_case(&field_name);
        let slot_ident = Ident::new(&const_name, ident.span());
        let offset_ident = Ident::new(&format!("{const_name}_OFFSET"), ident.span());
        let bytes_ident = Ident::new(&format!("{const_name}_BYTES"), ident.span());

        quote! {
            RustStorageField::new(#field_name, slots::#slot_ident, slots::#offset_ident, slots::#bytes_ident)
        }
    }).collect();

    let output = quote! { vec![#(#field_calls),*] };
    output.into()
}

/// Test helper macro for validating slots
#[proc_macro]
pub fn gen_test_fields_struct(input: TokenStream) -> TokenStream {
    let input = proc_macro2::TokenStream::from(input);

    let parser = |input: ParseStream<'_>| {
        let base_slot: Expr = input.parse()?;
        input.parse::<Token![,]>()?;
        let fields = Punctuated::<Ident, Token![,]>::parse_terminated(input)?;
        Ok((base_slot, fields))
    };

    let (base_slot, idents) = match Parser::parse2(parser, input) {
        Ok(result) => result,
        Err(err) => return err.to_compile_error().into(),
    };

    let field_calls: Vec<_> = idents.into_iter().map(|ident| {
        let field_name = ident.to_string();
        let const_name = field_name.to_uppercase();
        let field_name = utils::to_camel_case(&field_name);
        let slot_ident = Ident::new(&const_name, ident.span());
        let offset_ident = Ident::new(&format!("{const_name}_OFFSET"), ident.span());
        let loc_ident = Ident::new(&format!("{const_name}_LOC"), ident.span());
        let bytes_ident = quote! { #loc_ident.size };

        quote! {
            RustStorageField::new(#field_name, #base_slot + #slot_ident, #offset_ident, #bytes_ident)
        }
    }).collect();

    let output = quote! { vec![#(#field_calls),*] };
    output.into()
=======
/// Test helper macro for validating slots.
#[proc_macro]
pub fn gen_test_fields_layout(input: TokenStream) -> TokenStream {
    test_fields::gen_layout(proc_macro2::TokenStream::from(input))
}

/// Test helper macro for validating struct field slots.
#[proc_macro]
pub fn gen_test_fields_struct(input: TokenStream) -> TokenStream {
    test_fields::gen_struct_fields(proc_macro2::TokenStream::from(input))
>>>>>>> d46df9d45d1cea16baddff35c768ecceea2e02d5
}
