use crate::utils::{
    attributes_include, field_ident, is_optional, make_generics, option_inner_type, parse_struct,
    parse_trailing_opts, TrailingOpts, EMPTY_STRING_CODE,
};
use proc_macro2::TokenStream;
use quote::quote;
use syn::{Error, Result};

pub(crate) fn impl_decodable(ast: &syn::DeriveInput) -> Result<TokenStream> {
    let body = parse_struct(ast, "RlpDecodable")?;

    let fields = body.fields.iter().enumerate();

    let trailing_opts = parse_trailing_opts(&ast.attrs);

    let mut encountered_opt_item = false;
    let mut decode_stmts = Vec::with_capacity(body.fields.len());
    let mut canonical_assert_types = Vec::new();
    for (i, field) in fields {
        let is_opt = is_optional(field);
        if is_opt {
            if !trailing_opts.enabled {
                let msg = "optional fields are disabled.\nAdd the `#[rlp(trailing)]` attribute to the struct in order to enable optional fields";
                return Err(Error::new_spanned(field, msg));
            }
            encountered_opt_item = true;
            if trailing_opts.canonical {
                if let Some(inner_ty) = option_inner_type(field) {
                    canonical_assert_types.push(inner_ty.clone());
                }
            }
        } else if encountered_opt_item && !attributes_include(&field.attrs, "default") {
            let msg =
                "all the fields after the first optional field must be either optional or default";
            return Err(Error::new_spanned(field, msg));
        }

        decode_stmts.push(decodable_field(i, field, is_opt, trailing_opts));
    }

    let name = &ast.ident;
    let generics = make_generics(&ast.generics, quote!(alloy_rlp::Decodable));
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let strict_asserts = canonical_assert_types.iter().map(|ty| {
        let msg = format!(
            "trailing(canonical): `Option<{}>` is not supported because `{}` decodes successfully from 0x80 (the None placeholder). \
             Use a type whose RLP encoding is never 0x80.",
            quote!(#ty), quote!(#ty),
        );
        quote! {
            debug_assert!(
                <#ty as alloy_rlp::Decodable>::decode(&mut &[#EMPTY_STRING_CODE][..]).is_err(),
                #msg,
            );
        }
    });

    Ok(quote! {
        const _: () = {
            extern crate alloy_rlp;

            impl #impl_generics alloy_rlp::Decodable for #name #ty_generics #where_clause {
                #[inline]
                fn decode(b: &mut &[u8]) -> alloy_rlp::Result<Self> {
                    #(#strict_asserts)*
                    let alloy_rlp::Header { list, payload_length } = alloy_rlp::Header::decode(b)?;
                    if !list {
                        return Err(alloy_rlp::Error::UnexpectedString);
                    }

                    let started_len = b.len();
                    if started_len < payload_length {
                        return Err(alloy_rlp::DecodeError::InputTooShort);
                    }

                    let this = Self {
                        #(#decode_stmts)*
                    };

                    let consumed = started_len - b.len();
                    if consumed != payload_length {
                        return Err(alloy_rlp::Error::ListLengthMismatch {
                            expected: payload_length,
                            got: consumed,
                        });
                    }

                    Ok(this)
                }
            }
        };
    })
}

pub(crate) fn impl_decodable_wrapper(ast: &syn::DeriveInput) -> Result<TokenStream> {
    let body = parse_struct(ast, "RlpEncodableWrapper")?;

    if body.fields.iter().count() != 1 {
        let msg = "`RlpEncodableWrapper` is only defined for structs with one field.";
        return Err(Error::new(ast.ident.span(), msg));
    }

    let name = &ast.ident;
    let generics = make_generics(&ast.generics, quote!(alloy_rlp::Decodable));
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    Ok(quote! {
        const _: () = {
            extern crate alloy_rlp;

            impl #impl_generics alloy_rlp::Decodable for #name #ty_generics #where_clause {
                #[inline]
                fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
                    alloy_rlp::private::Result::map(alloy_rlp::Decodable::decode(buf), Self)
                }
            }
        };
    })
}

fn decodable_field(
    index: usize,
    field: &syn::Field,
    is_opt: bool,
    trailing_opts: TrailingOpts,
) -> TokenStream {
    let ident = field_ident(index, field);

    if attributes_include(&field.attrs, "default") {
        quote! { #ident: alloy_rlp::private::Default::default(), }
    } else if is_opt {
        if trailing_opts.no_gaps {
            // no_gaps: no 0x80 sentinel logic; just decode if there's remaining payload
            quote! {
                #ident: if started_len - b.len() < payload_length {
                    Some(alloy_rlp::Decodable::decode(b)?)
                } else {
                    None
                },
            }
        } else if trailing_opts.canonical {
            // canonical: 0x80 is only treated as a None placeholder if there is
            // more payload remaining after consuming it. This ensures canonical encoding:
            // trailing None fields must be omitted, not encoded as 0x80.
            quote! {
                #ident: if started_len - b.len() < payload_length {
                    if alloy_rlp::private::Option::map_or(b.first(), false, |b| *b == #EMPTY_STRING_CODE)
                        && (started_len - b.len() + 1) < payload_length
                    {
                        alloy_rlp::Buf::advance(b, 1);
                        None
                    } else {
                        Some(alloy_rlp::Decodable::decode(b)?)
                    }
                } else {
                    None
                },
            }
        } else {
            // plain trailing: 0x80 always treated as None
            quote! {
                #ident: if started_len - b.len() < payload_length {
                    if alloy_rlp::private::Option::map_or(b.first(), false, |b| *b == #EMPTY_STRING_CODE) {
                        alloy_rlp::Buf::advance(b, 1);
                        None
                    } else {
                        Some(alloy_rlp::Decodable::decode(b)?)
                    }
                } else {
                    None
                },
            }
        }
    } else {
        quote! { #ident: alloy_rlp::Decodable::decode(b)?, }
    }
}
