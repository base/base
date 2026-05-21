//! Implementation of the `#[namespace]` attribute macro.

use proc_macro::TokenStream;
use quote::quote;
use syn::{DeriveInput, LitStr};

use crate::utils::{attr_path_is, parse_namespace_id};

pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    match expand_impl(attr.into(), item.into()) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_impl(
    attr: proc_macro2::TokenStream,
    item: proc_macro2::TokenStream,
) -> syn::Result<proc_macro2::TokenStream> {
    let namespace_id: LitStr = syn::parse2(attr)?;
    let mut input: DeriveInput = syn::parse2(item)?;

    parse_namespace_id(namespace_id.clone())?;

    if input.attrs.iter().any(|attr| attr_path_is(attr.path(), "namespace")) {
        return Err(syn::Error::new_spanned(&input.ident, "duplicate `namespace` attribute"));
    }

    let contract_index =
        input.attrs.iter().position(|attr| attr_path_is(attr.path(), "contract")).ok_or_else(
            || {
                syn::Error::new_spanned(
                    &input.ident,
                    "`#[namespace]` must be paired with `#[contract]`",
                )
            },
        )?;

    input.attrs.insert(contract_index + 1, syn::parse_quote!(#[namespace(#namespace_id)]));

    Ok(quote! { #input })
}
