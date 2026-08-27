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
    let storage_features = config.storage_features.ok_or_else(|| {
        syn::Error::new(
            input.ident.span(),
            "`#[precompile]` requires `storage_features = <expr>` so the wrapper is pinned to the \
             active fork instead of falling back to a default (see Cantina finding #17).",
        )
    })?;
    let args = config.args;
    let arg_defs = args.iter().map(PrecompileArg::definition);
    let install_arg_defs = args.iter().map(PrecompileArg::definition);
    let install_arg_names = args.iter().map(|arg| &arg.ident);
    let install = config.install.then(|| {
        let doc = format!("Installs the `{ident}` precompile into `precompiles`.");

        quote! {
            #[doc = #doc]
            pub fn install(
                precompiles: &mut ::alloy_evm::precompiles::PrecompilesMap,
                #(#install_arg_defs),*
            ) {
                precompiles.extend_precompiles(::core::iter::once((
                    <#storage>::ADDRESS,
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
                #macro_path!(
                    #id,
                    storage_features: #storage_features,
                    |ctx, calldata| {
                        <#storage>::new(ctx).dispatch(ctx, &calldata #(, #arg_names)*)
                    },
                )
            }
        }
    })
}

struct PrecompileConfig {
    id: Option<Expr>,
    storage: Option<Type>,
    macro_path: Option<Path>,
    storage_features: Option<Expr>,
    args: Vec<PrecompileArg>,
    install: bool,
}

impl Parse for PrecompileConfig {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut id = None;
        let mut storage = None;
        let mut macro_path = None;
        let mut storage_features = None;
        let mut args = Vec::new();
        let mut args_seen = false;
        let mut install = false;

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
                "storage_features" => {
                    reject_duplicate(&storage_features, &key)?;
                    input.parse::<Token![=]>()?;
                    storage_features = Some(input.parse()?);
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
                    if install {
                        return Err(syn::Error::new_spanned(key, "duplicate `install` option"));
                    }
                    if input.peek(syn::token::Paren) {
                        return Err(syn::Error::new_spanned(
                            &key,
                            "`install` does not accept arguments; registration uses `<storage>::ADDRESS`",
                        ));
                    }
                    install = true;
                }
                _ => {
                    return Err(syn::Error::new_spanned(
                        key,
                        "expected `id`, `storage`, `macro_path`, `storage_features`, `args`, or `install`",
                    ));
                }
            }

            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        Ok(Self { id, storage, macro_path, storage_features, args, install })
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

    use super::{PrecompileConfig, expand_impl};

    fn parse_config(tokens: TokenStream2) -> syn::Result<PrecompileConfig> {
        syn::parse2(tokens)
    }

    fn assert_install_rejects_arguments(tokens: TokenStream2) {
        let err = parse_config(tokens).err().unwrap();
        let msg = err.to_string();
        assert!(
            msg.contains(
                "`install` does not accept arguments; registration uses `<storage>::ADDRESS`"
            ),
            "got: {msg}"
        );
    }

    #[test]
    fn config_rejects_unknown_options() {
        let err = parse_config(quote! { instal }).err().unwrap();

        assert!(err.to_string().contains(
            "expected `id`, `storage`, `macro_path`, `storage_features`, `args`, or `install`"
        ));
    }

    #[test]
    fn config_rejects_positional_storage() {
        let err = parse_config(quote! { CustomStorage<'_> }).err().unwrap();

        assert!(err.to_string().contains(
            "expected `id`, `storage`, `macro_path`, `storage_features`, `args`, or `install`"
        ));
    }

    #[test]
    fn config_accepts_explicit_storage_and_macro_path() {
        let config = parse_config(quote! {
            storage = CustomStorage<'_>,
            macro_path = crate::macros::custom_precompile,
            storage_features = StorageFeatures::Cobalt,
        })
        .unwrap();

        assert!(config.storage.is_some());
        assert!(config.macro_path.is_some());
        assert!(config.storage_features.is_some());
        assert!(!config.install);
    }

    #[test]
    fn config_accepts_bare_install() {
        let config = parse_config(quote! { install }).unwrap();

        assert!(config.install);
    }

    #[test]
    fn expand_requires_storage_features() {
        let err = expand_impl(
            quote! { install },
            quote! {
                pub struct Example;
            },
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("`storage_features = <expr>`"), "got: {err}");
    }

    #[test]
    fn expand_emits_pinned_storage_features() {
        let tokens = expand_impl(
            quote! {
                install,
                args(upgrade: BaseUpgrade),
                storage_features = pin_from_upgrade(upgrade),
            },
            quote! {
                pub struct Example;
            },
        )
        .unwrap()
        .to_string();

        assert!(tokens.contains("storage_features :"), "got: {tokens}");
        assert!(tokens.contains("pin_from_upgrade (upgrade)"), "got: {tokens}");
    }

    #[test]
    fn install_rejects_empty_parentheses() {
        assert_install_rejects_arguments(quote! { install() });
    }

    #[test]
    fn install_rejects_addr_override() {
        assert_install_rejects_arguments(quote! { install(addr = X) });
    }

    #[test]
    fn install_rejects_address_alias() {
        assert_install_rejects_arguments(quote! { install(address = X) });
    }

    #[test]
    fn install_rejects_typo_argument() {
        assert_install_rejects_arguments(quote! { install(a = X) });
    }

    #[test]
    fn config_rejects_duplicate_install() {
        let err = parse_config(quote! { install, install }).err().unwrap();

        assert!(err.to_string().contains("duplicate `install` option"));
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

    #[test]
    fn bare_install_expands_to_storage_address() {
        let tokens = expand_impl(
            quote! { install, storage_features = StorageFeatures::Cobalt },
            quote! {
                pub struct Example;
            },
        )
        .unwrap()
        .to_string();

        assert!(tokens.contains("ExampleStorage"), "got: {tokens}");
        assert!(tokens.contains("ADDRESS"), "got: {tokens}");
        assert!(tokens.contains("extend_precompiles"), "got: {tokens}");
    }

    #[test]
    fn install_with_explicit_storage_uses_that_address() {
        let tokens = expand_impl(
            quote! {
                storage = CustomStorage<'_>,
                install,
                storage_features = StorageFeatures::Cobalt,
            },
            quote! {
                pub struct Example;
            },
        )
        .unwrap()
        .to_string();

        assert!(tokens.contains("CustomStorage"), "got: {tokens}");
        assert!(tokens.contains("ADDRESS"), "got: {tokens}");
        assert!(tokens.contains("extend_precompiles"), "got: {tokens}");
        assert!(tokens.contains(". dispatch"), "got: {tokens}");
        assert!(!tokens.contains("ExampleStorage"), "got: {tokens}");
    }
}
