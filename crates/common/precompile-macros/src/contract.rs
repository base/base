//! Implementation of the `#[contract]` attribute macro.

use alloy_primitives::U256;
use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Expr, Fields, Ident, Token, Type, Visibility, parse::ParseStream};

use crate::{
    layout, packing,
    utils::{NamespaceInfo, extract_attributes, extract_namespace},
};

pub(crate) struct ContractConfig {
    pub(crate) address: Option<Expr>,
}

impl syn::parse::Parse for ContractConfig {
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

pub(crate) const RESERVED: &[&str] = &["address", "storage", "msg_sender"];

#[derive(Debug)]
pub(crate) struct FieldInfo {
    pub(crate) name: Ident,
    pub(crate) ty: Type,
    pub(crate) slot: Option<U256>,
    pub(crate) base_slot: Option<U256>,
    pub(crate) namespace: Option<NamespaceInfo>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum FieldKind<'a> {
    Direct(&'a Type),
    Mapping { key: &'a Type, value: &'a Type },
}

pub(crate) fn generate(input: DeriveInput, address: Option<&Expr>) -> proc_macro::TokenStream {
    match gen_output(input, address) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn gen_output(input: DeriveInput, address: Option<&Expr>) -> syn::Result<TokenStream> {
    let (ident, vis) = (input.ident.clone(), input.vis.clone());
    let namespace = extract_namespace(&input.attrs)?;
    let fields = parse_fields(input, namespace.is_some())?;

    let storage_output = gen_storage(&ident, &vis, &fields, address, namespace.as_ref())?;
    Ok(quote! { #storage_output })
}

pub(crate) fn parse_fields(
    input: DeriveInput,
    namespace_enabled: bool,
) -> syn::Result<Vec<FieldInfo>> {
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

            let (slot, base_slot, namespace) = extract_attributes(&field.attrs)?;
            if namespace_enabled && (slot.is_some() || base_slot.is_some() || namespace.is_some()) {
                return Err(syn::Error::new_spanned(
                    name,
                    "field-level `slot`, `base_slot`, and `namespace` attributes cannot be used with contract-level `namespace`",
                ));
            }
            Ok(FieldInfo { name: name.to_owned(), ty: field.ty, slot, base_slot, namespace })
        })
        .collect()
}

fn gen_storage(
    ident: &Ident,
    vis: &Visibility,
    fields: &[FieldInfo],
    address: Option<&Expr>,
    namespace: Option<&NamespaceInfo>,
) -> syn::Result<TokenStream> {
    let allocated_fields = packing::allocate_slots_from(
        fields,
        namespace.map_or(U256::ZERO, |namespace| namespace.root),
        namespace.is_none(),
    )?;
    let transformed_struct = layout::gen_struct(ident, vis, &allocated_fields);
    let storage_trait = layout::gen_contract_storage_impl(ident);
    let constructor = layout::gen_constructor(ident, &allocated_fields, address);
    let slots_module = layout::gen_slots_module(&allocated_fields, namespace);

    Ok(quote! {
        #slots_module
        #transformed_struct
        #constructor
        #storage_trait
    })
}
