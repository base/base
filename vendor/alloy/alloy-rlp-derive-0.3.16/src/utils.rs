use proc_macro2::TokenStream;
use quote::quote;
use syn::{
    parse_quote, Attribute, DataStruct, Error, Field, GenericParam, Generics, Meta, Result, Type,
    TypePath,
};

#[derive(Default, Clone, Copy)]
pub(crate) struct TrailingOpts {
    pub enabled: bool,
    pub no_gaps: bool,
    pub canonical: bool,
}

pub(crate) fn parse_trailing_opts(attrs: &[Attribute]) -> TrailingOpts {
    let mut opts = TrailingOpts::default();
    for attr in attrs {
        if attr.path().is_ident("rlp") {
            if let Meta::List(meta) = &attr.meta {
                let _ = meta.parse_nested_meta(|meta| {
                    if meta.path.is_ident("trailing") {
                        opts.enabled = true;
                        if meta.input.peek(syn::token::Paren) {
                            meta.parse_nested_meta(|inner| {
                                if inner.path.is_ident("no_gaps") {
                                    opts.no_gaps = true;
                                } else if inner.path.is_ident("canonical") {
                                    opts.canonical = true;
                                }
                                Ok(())
                            })?;
                        }
                    }
                    Ok(())
                });
            }
        }
    }
    opts
}

pub(crate) const EMPTY_STRING_CODE: u8 = 0x80;

pub(crate) fn parse_struct<'a>(
    ast: &'a syn::DeriveInput,
    derive_attr: &str,
) -> Result<&'a DataStruct> {
    if let syn::Data::Struct(s) = &ast.data {
        Ok(s)
    } else {
        Err(Error::new_spanned(
            ast,
            format!("#[derive({derive_attr})] is only defined for structs."),
        ))
    }
}

pub(crate) fn attributes_include(attrs: &[Attribute], attr_name: &str) -> bool {
    for attr in attrs {
        if attr.path().is_ident("rlp") {
            if let Meta::List(meta) = &attr.meta {
                let mut found = false;
                let _ = meta.parse_nested_meta(|meta| {
                    if meta.path.is_ident(attr_name) {
                        found = true;
                    }
                    Ok(())
                });
                if found {
                    return true;
                }
            }
        }
    }
    false
}

pub(crate) fn is_optional(field: &Field) -> bool {
    option_inner_type(field).is_some()
}

/// Extracts the inner type `T` from an `Option<T>` field, or returns `None` if the field is not
/// an `Option`.
pub(crate) fn option_inner_type(field: &Field) -> Option<&syn::Type> {
    if let Type::Path(TypePath { qself, path }) = &field.ty {
        if qself.is_none()
            && path.leading_colon.is_none()
            && path.segments.len() == 1
            && path.segments.first().unwrap().ident == "Option"
        {
            if let syn::PathArguments::AngleBracketed(args) =
                &path.segments.first().unwrap().arguments
            {
                if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
                    return Some(inner);
                }
            }
        }
    }
    None
}

pub(crate) fn field_ident(index: usize, field: &syn::Field) -> TokenStream {
    field.ident.as_ref().map_or_else(
        || {
            let index = syn::Index::from(index);
            quote! { #index }
        },
        |ident| quote! { #ident },
    )
}

pub(crate) fn make_generics(generics: &Generics, trait_name: TokenStream) -> Generics {
    let mut generics = generics.clone();
    generics.make_where_clause();
    let mut where_clause = generics.where_clause.take().unwrap();

    for generic in &generics.params {
        if let GenericParam::Type(ty) = &generic {
            let t = &ty.ident;
            let pred = parse_quote!(#t: #trait_name);
            where_clause.predicates.push(pred);
        }
    }
    generics.where_clause = Some(where_clause);
    generics
}
