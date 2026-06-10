//! Implementation of the `#[precompile]` attribute macro.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{
    Data, DeriveInput, Expr, Ident, LitStr, Path, Token, Type, parenthesized,
    parse::{Parse, ParseStream},
};

pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    match expand_impl(attr.into(), item.into()) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_impl(attr: TokenStream2, item: TokenStream2) -> syn::Result<TokenStream2> {
    let config: PrecompileConfig = syn::parse2(attr)?;
    let input: DeriveInput = syn::parse2(item)?;
    let Data::Struct(_) = &input.data else {
        return Err(syn::Error::new_spanned(input.ident, "`#[precompile]` supports structs only"));
    };

    let ident = input.ident.clone();
    let generics = input.generics.clone();
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let base_name = precompile_name(&ident);
    let id = config.id.unwrap_or_else(|| {
        let id = LitStr::new(&base_name, ident.span());
        syn::parse_quote!(#id)
    });
    let storage = config.storage.unwrap_or_else(|| {
        let storage = format_ident!("{base_name}Storage", span = ident.span());
        syn::parse_quote!(#storage<'_>)
    });
    let macro_path =
        config.macro_path.unwrap_or_else(|| syn::parse_quote!(crate::macros::base_precompile));
    let args = config.args;
    let arg_defs = args.iter().map(PrecompileArg::definition);
    let install_arg_defs = args.iter().map(PrecompileArg::definition);
    let install_arg_names = args.iter().map(|arg| &arg.ident);
    let install = config.install.map(|install| {
        let address = install
            .address
            .map_or_else(|| quote! { <#storage>::ADDRESS }, |address| quote! { #address });
        let doc = format!("Installs the `{ident}` precompile into `precompiles`.");

        quote! {
            #[doc = #doc]
            pub fn install(
                precompiles: &mut ::alloy_evm::precompiles::PrecompilesMap,
                #(#install_arg_defs),*
            ) {
                precompiles.extend_precompiles(::core::iter::once((
                    #address,
                    Self::precompile(#(#install_arg_names),*),
                )));
            }
        }
    });
    let precompile_doc = format!("Creates the EVM precompile wrapper for `{ident}`.");
    let arg_names = args.iter().map(|arg| &arg.ident);

    Ok(quote! {
        #input

        impl #impl_generics #ident #ty_generics #where_clause {
            #install

            #[doc = #precompile_doc]
            pub fn precompile(#(#arg_defs),*) -> ::alloy_evm::precompiles::DynPrecompile {
                #macro_path!(#id, |ctx, calldata| {
                    <#storage>::new(ctx).dispatch(ctx, &calldata #(, #arg_names)*)
                })
            }
        }
    })
}

struct PrecompileConfig {
    id: Option<Expr>,
    storage: Option<Type>,
    macro_path: Option<Path>,
    args: Vec<PrecompileArg>,
    install: Option<InstallConfig>,
}

impl Parse for PrecompileConfig {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut id = None;
        let mut storage = None;
        let mut macro_path = None;
        let mut args = Vec::new();
        let mut args_seen = false;
        let mut install = None;

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            match key.to_string().as_str() {
                "id" => {
                    reject_duplicate(&id, &key)?;
                    input.parse::<Token![=]>()?;
                    id = Some(input.parse()?);
                }
                "storage" => {
                    reject_duplicate(&storage, &key)?;
                    input.parse::<Token![=]>()?;
                    storage = Some(input.parse()?);
                }
                "macro_path" => {
                    reject_duplicate(&macro_path, &key)?;
                    input.parse::<Token![=]>()?;
                    macro_path = Some(input.parse()?);
                }
                "args" => {
                    if args_seen {
                        return Err(syn::Error::new_spanned(key, "duplicate `args` option"));
                    }
                    args_seen = true;
                    let content;
                    parenthesized!(content in input);
                    args = content
                        .parse_terminated(PrecompileArg::parse, Token![,])?
                        .into_iter()
                        .collect();
                }
                "install" => {
                    reject_duplicate(&install, &key)?;
                    install = if input.peek(syn::token::Paren) {
                        let content;
                        parenthesized!(content in input);
                        Some(content.parse()?)
                    } else {
                        Some(InstallConfig { address: None })
                    };
                }
                _ => {
                    return Err(syn::Error::new_spanned(
                        key,
                        "expected `id`, `storage`, `macro_path`, `args`, or `install`",
                    ));
                }
            }

            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        Ok(Self { id, storage, macro_path, args, install })
    }
}

struct PrecompileArg {
    ident: Ident,
    ty: Type,
}

impl PrecompileArg {
    fn definition(&self) -> TokenStream2 {
        let ident = &self.ident;
        let ty = &self.ty;

        quote! { #ident: #ty }
    }
}

impl Parse for PrecompileArg {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let ident = input.parse()?;
        input.parse::<Token![:]>()?;
        let ty = input.parse()?;

        Ok(Self { ident, ty })
    }
}

struct InstallConfig {
    address: Option<Expr>,
}

impl Parse for InstallConfig {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let key: Ident = input.parse()?;
        if key != "addr" {
            return Err(syn::Error::new_spanned(
                &key,
                format!(
                    "unrecognized install argument `{key}`, the supported argument is `addr = \"0x...\"`"
                ),
            ));
        }

        input.parse::<Token![=]>()?;
        let address = input.parse()?;

        let has_non_comma_remainder = !input.is_empty() && !input.peek(Token![,]);
        if has_non_comma_remainder {
            return Err(syn::Error::new(input.span(), "unexpected `install` option"));
        }
        if !input.is_empty() {
            input.parse::<Token![,]>()?;
        }
        if !input.is_empty() {
            return Err(syn::Error::new(input.span(), "unexpected `install` option"));
        }

        Ok(Self { address: Some(address) })
    }
}

fn reject_duplicate<T>(option: &Option<T>, ident: &Ident) -> syn::Result<()> {
    if option.is_some() {
        return Err(syn::Error::new_spanned(ident, format!("duplicate `{ident}` option")));
    }

    Ok(())
}

fn precompile_name(ident: &Ident) -> String {
    ident.to_string().trim_end_matches("Precompile").to_owned()
}

#[cfg(test)]
mod tests {
    use proc_macro2::TokenStream as TokenStream2;
    use quote::quote;

    use super::PrecompileConfig;

    fn parse_config(tokens: TokenStream2) -> syn::Result<PrecompileConfig> {
        syn::parse2(tokens)
    }

    #[test]
    fn config_rejects_unknown_options() {
        let err = parse_config(quote! { instal }).err().unwrap();

        assert!(
            err.to_string()
                .contains("expected `id`, `storage`, `macro_path`, `args`, or `install`")
        );
    }

    #[test]
    fn config_rejects_positional_storage() {
        let err = parse_config(quote! { CustomStorage<'_> }).err().unwrap();

        assert!(
            err.to_string()
                .contains("expected `id`, `storage`, `macro_path`, `args`, or `install`")
        );
    }

    #[test]
    fn config_accepts_explicit_storage_and_macro_path() {
        let config = parse_config(quote! {
            storage = CustomStorage<'_>,
            macro_path = crate::macros::custom_precompile,
        })
        .unwrap();

        assert!(config.storage.is_some());
        assert!(config.macro_path.is_some());
    }

    #[test]
    fn install_config_rejects_address_alias_with_helpful_diagnostic() {
        let err = parse_config(quote! { install(address = X) }).err().unwrap();

        let msg = err.to_string();
        assert!(msg.contains("unrecognized install argument `address`"), "got: {msg}");
        assert!(msg.contains("addr"), "got: {msg}");
    }

    #[test]
    fn install_config_rejects_typo_with_helpful_diagnostic() {
        let err = parse_config(quote! { install(a = X) }).err().unwrap();

        let msg = err.to_string();
        assert!(msg.contains("unrecognized install argument `a`"), "got: {msg}");
        assert!(msg.contains("addr"), "got: {msg}");
    }

    #[test]
    fn install_config_rejects_extra_option_without_comma() {
        let err = parse_config(quote! { install(addr = X extra) }).err().unwrap();

        assert!(err.to_string().contains("unexpected `install` option"));
    }

    #[test]
    fn install_config_rejects_extra_option_after_comma() {
        let err = parse_config(quote! { install(addr = X, extra) }).err().unwrap();

        assert!(err.to_string().contains("unexpected `install` option"));
    }

    #[test]
    fn config_rejects_duplicate_empty_args() {
        let err = parse_config(quote! { args(), args() }).err().unwrap();

        assert!(err.to_string().contains("duplicate `args` option"));
    }

    #[test]
    fn config_rejects_duplicate_args_where_first_is_empty() {
        let err = parse_config(quote! { args(), args(x: u8) }).err().unwrap();

        assert!(err.to_string().contains("duplicate `args` option"));
    }
}
