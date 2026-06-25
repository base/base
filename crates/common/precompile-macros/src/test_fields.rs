//! Test helper macros for validating storage slot layouts.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Expr, Ident, Token, parse::ParseStream, punctuated::Punctuated};

use crate::utils::to_camel_case;

pub(crate) fn gen_layout(input: TokenStream2) -> TokenStream {
    let parser = syn::punctuated::Punctuated::<Ident, syn::Token![,]>::parse_terminated;
    let idents = match syn::parse::Parser::parse2(parser, input) {
        Ok(idents) => idents,
        Err(err) => return err.to_compile_error().into(),
    };

    let field_calls: Vec<_> = idents
        .into_iter()
        .map(|ident| {
            let field_name = ident.to_string();
            let const_name = field_name.to_uppercase();
            let field_name = to_camel_case(&field_name);
            let slot_ident = Ident::new(&const_name, ident.span());
            let offset_ident = Ident::new(&format!("{const_name}_OFFSET"), ident.span());
            let bytes_ident = Ident::new(&format!("{const_name}_BYTES"), ident.span());

            quote! {
                RustStorageField::new(#field_name, slots::#slot_ident, slots::#offset_ident, slots::#bytes_ident)
            }
        })
        .collect();

    let output = quote! { vec![#(#field_calls),*] };
    output.into()
}

pub(crate) fn gen_struct_fields(input: TokenStream2) -> TokenStream {
    let parser = |input: ParseStream<'_>| {
        let base_slot: Expr = input.parse()?;
        input.parse::<Token![,]>()?;
        let fields = Punctuated::<Ident, Token![,]>::parse_terminated(input)?;
        Ok((base_slot, fields))
    };

    let (base_slot, idents) = match syn::parse::Parser::parse2(parser, input) {
        Ok(result) => result,
        Err(err) => return err.to_compile_error().into(),
    };

    let field_calls: Vec<_> = idents
        .into_iter()
        .map(|ident| {
            let field_name = ident.to_string();
            let const_name = field_name.to_uppercase();
            let field_name = to_camel_case(&field_name);
            let slot_ident = Ident::new(&const_name, ident.span());
            let offset_ident = Ident::new(&format!("{const_name}_OFFSET"), ident.span());
            let loc_ident = Ident::new(&format!("{const_name}_LOC"), ident.span());
            let bytes_ident = quote! { #loc_ident.size };

            quote! {
                RustStorageField::new(#field_name, #base_slot + #slot_ident, #offset_ident, #bytes_ident)
            }
        })
        .collect();

    let output = quote! { vec![#(#field_calls),*] };
    output.into()
}
