//! Implementation of the storage layout attribute macros.

use alloy_primitives::U256;
use proc_macro2::TokenStream;
use quote::quote;
use syn::{
    Data, DeriveInput, Expr, Fields, Ident, Lit, Token, Type, Visibility, parse::ParseStream,
};

use crate::{
    layout, packing,
    utils::{extract_attributes, parse_slot_value},
};

pub(crate) struct ContractConfig {
    pub(crate) address: Option<Expr>,
    pub(crate) base_slot: U256,
}

impl syn::parse::Parse for ContractConfig {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut address = None;
        let mut base_slot = U256::ZERO;
        let mut has_base_slot = false;

        if input.is_empty() {
            return Ok(Self { address, base_slot });
        }

        while !input.is_empty() {
            let ident: Ident = input.parse()?;
            input.parse::<Token![=]>()?;

            if ident == "addr" || ident == "address" {
                if address.is_some() {
                    return Err(syn::Error::new(ident.span(), "duplicate address attribute"));
                }
                address = Some(input.parse()?);
            } else if ident == "base_slot" {
                if has_base_slot {
                    return Err(syn::Error::new(ident.span(), "duplicate `base_slot` attribute"));
                }
                let value: Lit = input.parse()?;
                base_slot = parse_slot_value(&value)?;
                has_base_slot = true;
            } else {
                return Err(syn::Error::new(
                    ident.span(),
                    "supported attributes are `addr`, `address`, and `base_slot`",
                ));
            }

            if input.is_empty() {
                break;
            }
            input.parse::<Token![,]>()?;
        }

        Ok(Self { address, base_slot })
    }
}

pub(crate) const RESERVED: &[&str] = &["address", "storage", "msg_sender"];

#[derive(Debug)]
pub(crate) struct FieldInfo {
    pub(crate) name: Ident,
    pub(crate) ty: Type,
    pub(crate) slot: Option<U256>,
    pub(crate) base_slot: Option<U256>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum FieldKind<'a> {
    Direct(&'a Type),
    Mapping { key: &'a Type, value: &'a Type },
}

pub(crate) fn generate(input: DeriveInput, config: &ContractConfig) -> proc_macro::TokenStream {
    match gen_output(input, config) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn gen_output(input: DeriveInput, config: &ContractConfig) -> syn::Result<TokenStream> {
    let (ident, vis) = (input.ident.clone(), input.vis.clone());
    let fields = parse_fields(input)?;

    let storage_output = gen_storage(&ident, &vis, &fields, config)?;
    Ok(quote! { #storage_output })
}

pub(crate) fn parse_fields(input: DeriveInput) -> syn::Result<Vec<FieldInfo>> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "Contract structs cannot have generic parameters",
        ));
    }

    let named_fields = if let Data::Struct(data) = input.data
        && let Fields::Named(fields) = data.fields
    {
        fields.named
    } else {
        return Err(syn::Error::new_spanned(
            input.ident,
            "Only structs with named fields are supported",
        ));
    };

    named_fields
        .into_iter()
        .map(|field| {
            let name = field
                .ident
                .as_ref()
                .ok_or_else(|| syn::Error::new_spanned(&field, "Fields must have names"))?;

            if RESERVED.contains(&name.to_string().as_str()) {
                return Err(syn::Error::new_spanned(
                    name,
                    format!("Field name '{name}' is reserved"),
                ));
            }

            let (slot, base_slot) = extract_attributes(&field.attrs)?;
            Ok(FieldInfo { name: name.to_owned(), ty: field.ty, slot, base_slot })
        })
        .collect()
}

fn gen_storage(
    ident: &Ident,
    vis: &Visibility,
    fields: &[FieldInfo],
    config: &ContractConfig,
) -> syn::Result<TokenStream> {
    let allocated_fields = packing::allocate_slots(fields, config.base_slot)?;
    let transformed_struct = layout::gen_struct(ident, vis, &allocated_fields);
    let storage_trait = layout::gen_contract_storage_impl(ident);
    let constructor = layout::gen_constructor(ident, &allocated_fields, config.address.as_ref());
    let slots_module = layout::gen_slots_module(&allocated_fields);
    let default_impl = if config.address.is_some() {
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
}
