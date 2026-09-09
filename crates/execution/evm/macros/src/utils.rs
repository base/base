//! Utility functions for the contract macro implementation.

use alloy_primitives::{U256, keccak256};
use syn::{Attribute, Lit, LitStr, Path, Type};

/// Parsed `#[namespace("...")]` metadata.
#[derive(Debug, Clone)]
pub(crate) struct NamespaceInfo {
    pub(crate) id: LitStr,
    pub(crate) root: U256,
}

/// Return type for [`extract_attributes`]: (`slot`, `base_slot`, `namespace`)
type ExtractedAttributes = (Option<U256>, Option<U256>, Option<NamespaceInfo>);

/// Parses a slot value from a literal.
///
/// Supports:
/// - Integer literals: decimal (`42`) or hexadecimal (`0x2a`)
/// - String literals: computes keccak256 hash of the string
fn parse_slot_value(value: &Lit) -> syn::Result<U256> {
    match value {
        Lit::Int(int) => {
            let lit_str = int.to_string();
            let slot = lit_str
                .strip_prefix("0x")
                .map_or_else(
                    || U256::from_str_radix(&lit_str, 10),
                    |hex| U256::from_str_radix(hex, 16),
                )
                .map_err(|_| syn::Error::new_spanned(int, "Invalid slot number"))?;
            Ok(slot)
        }
        Lit::Str(lit) => Ok(keccak256(lit.value().as_bytes()).into()),
        _ => Err(syn::Error::new_spanned(
            value,
            "slot attribute must be an integer or a string literal",
        )),
    }
}

/// Returns whether an attribute path ends with the provided identifier.
pub(crate) fn attr_path_is(path: &Path, ident: &str) -> bool {
    path.segments.last().is_some_and(|segment| segment.ident == ident)
}

/// Parses and validates a namespace id string.
pub(crate) fn parse_namespace_id(id: LitStr) -> syn::Result<NamespaceInfo> {
    let value = id.value();
    if value.is_empty() {
        return Err(syn::Error::new(id.span(), "namespace id cannot be empty"));
    }
    if value.chars().any(char::is_whitespace) {
        return Err(syn::Error::new(id.span(), "namespace id must not contain whitespace"));
    }

    Ok(NamespaceInfo { root: erc7201_root(&id)?, id })
}

/// Computes the ERC-7201 namespace root for `id`.
pub(crate) fn erc7201_root(id: &LitStr) -> syn::Result<U256> {
    let id_hash = U256::from_be_bytes(keccak256(id.value().as_bytes()).0);
    let shifted = id_hash.checked_sub(U256::ONE).ok_or_else(|| {
        syn::Error::new(id.span(), "namespace root underflow while applying ERC-7201 formula")
    })?;
    let root = U256::from_be_bytes(keccak256(shifted.to_be_bytes::<32>()).0);
    Ok(root & (U256::MAX - U256::from(0xffu64)))
}

/// Converts a string from `CamelCase` or `snake_case` to `snake_case`.
pub(crate) fn to_snake_case(s: &str) -> String {
    let constant = s.to_uppercase();
    if s == constant {
        return constant;
    }

    let mut result = String::with_capacity(s.len() + 4);
    let mut chars = s.chars().peekable();
    let mut prev_upper = false;

    while let Some(c) = chars.next() {
        if c.is_uppercase() {
            if !result.is_empty()
                && (!prev_upper || chars.peek().is_some_and(|&next| next.is_lowercase()))
            {
                result.push('_');
            }
            result.push(c.to_ascii_lowercase());
            prev_upper = true;
        } else {
            result.push(c);
            prev_upper = false;
        }
    }

    result
}

/// Converts a string from `snake_case` to `camelCase`.
pub(crate) fn to_camel_case(s: &str) -> String {
    let mut result = String::new();
    let mut first_word = true;

    for word in s.split('_') {
        if word.is_empty() {
            continue;
        }

        if first_word {
            result.push_str(word);
            first_word = false;
        } else {
            let mut chars = word.chars();
            if let Some(first) = chars.next() {
                result.push_str(&first.to_uppercase().collect::<String>());
                result.push_str(chars.as_str());
            }
        }
    }
    result
}

/// Extracts `#[slot(N)]`, `#[base_slot(N)]` attributes from a field.
pub(crate) fn extract_attributes(attrs: &[Attribute]) -> syn::Result<ExtractedAttributes> {
    let mut slot_attr: Option<U256> = None;
    let mut base_slot_attr: Option<U256> = None;
    let mut namespace_attr: Option<NamespaceInfo> = None;

    for attr in attrs {
        if attr.path().is_ident("slot") {
            if slot_attr.is_some() {
                return Err(syn::Error::new_spanned(attr, "duplicate `slot` attribute"));
            }
            if base_slot_attr.is_some() || namespace_attr.is_some() {
                return Err(syn::Error::new_spanned(
                    attr,
                    "cannot combine `slot`, `base_slot`, and `namespace` attributes on the same field",
                ));
            }
            let value: Lit = attr.parse_args()?;
            slot_attr = Some(parse_slot_value(&value)?);
        } else if attr.path().is_ident("base_slot") {
            if base_slot_attr.is_some() {
                return Err(syn::Error::new_spanned(attr, "duplicate `base_slot` attribute"));
            }
            if slot_attr.is_some() || namespace_attr.is_some() {
                return Err(syn::Error::new_spanned(
                    attr,
                    "cannot combine `slot`, `base_slot`, and `namespace` attributes on the same field",
                ));
            }
            let value: Lit = attr.parse_args()?;
            base_slot_attr = Some(parse_slot_value(&value)?);
        } else if attr_path_is(attr.path(), "namespace") {
            if namespace_attr.is_some() {
                return Err(syn::Error::new_spanned(attr, "duplicate `namespace` attribute"));
            }
            if slot_attr.is_some() || base_slot_attr.is_some() {
                return Err(syn::Error::new_spanned(
                    attr,
                    "cannot combine `slot`, `base_slot`, and `namespace` attributes on the same field",
                ));
            }
            namespace_attr = Some(parse_namespace_id(attr.parse_args()?)?);
        }
    }

    Ok((slot_attr, base_slot_attr, namespace_attr))
}

/// Extracts a contract-level `#[namespace("...")]` attribute.
pub(crate) fn extract_namespace(attrs: &[Attribute]) -> syn::Result<Option<NamespaceInfo>> {
    let mut namespace = None;

    for attr in attrs {
        if !attr_path_is(attr.path(), "namespace") {
            continue;
        }
        if namespace.is_some() {
            return Err(syn::Error::new_spanned(attr, "duplicate `namespace` attribute"));
        }

        namespace = Some(parse_namespace_id(attr.parse_args()?)?);
    }

    Ok(namespace)
}

/// Extracts a type-level `#[namespace("...")]` attribute from a `Storable` layout.
pub(crate) fn extract_storage_namespace(attrs: &[Attribute]) -> syn::Result<Option<NamespaceInfo>> {
    let mut namespace = None;

    for attr in attrs {
        let is_namespace = attr_path_is(attr.path(), "namespace")
            || attr_path_is(attr.path(), "storage_namespace");
        if !is_namespace {
            continue;
        }
        if namespace.is_some() {
            return Err(syn::Error::new_spanned(attr, "duplicate `namespace` attribute"));
        }

        namespace = Some(parse_namespace_id(attr.parse_args()?)?);
    }

    Ok(namespace)
}

/// Extracts array sizes from the `#[storable_arrays(...)]` attribute.
pub(crate) fn extract_storable_array_sizes(attrs: &[Attribute]) -> syn::Result<Option<Vec<usize>>> {
    for attr in attrs {
        if attr.path().is_ident("storable_arrays") {
            let parsed = attr.parse_args_with(
                syn::punctuated::Punctuated::<Lit, syn::Token![,]>::parse_terminated,
            )?;

            let mut sizes = Vec::new();
            for lit in parsed {
                if let Lit::Int(int) = lit {
                    let size = int.base10_parse::<usize>().map_err(|_| {
                        syn::Error::new_spanned(
                            &int,
                            "Invalid array size: must be a positive integer",
                        )
                    })?;

                    if size == 0 {
                        return Err(syn::Error::new_spanned(
                            &int,
                            "Array size must be greater than 0",
                        ));
                    }
                    if size > 256 {
                        return Err(syn::Error::new_spanned(
                            &int,
                            "Array size must not exceed 256",
                        ));
                    }
                    if sizes.contains(&size) {
                        return Err(syn::Error::new_spanned(
                            &int,
                            format!("Duplicate array size: {size}"),
                        ));
                    }
                    sizes.push(size);
                } else {
                    return Err(syn::Error::new_spanned(
                        lit,
                        "Array sizes must be integer literals",
                    ));
                }
            }

            if sizes.is_empty() {
                return Err(syn::Error::new_spanned(
                    attr,
                    "storable_arrays attribute requires at least one size",
                ));
            }

            return Ok(Some(sizes));
        }
    }

    Ok(None)
}

/// Extracts the type parameters from Mapping<K, V>.
pub(crate) fn extract_mapping_types(ty: &Type) -> Option<(&Type, &Type)> {
    if let Type::Path(type_path) = ty {
        let last_segment = type_path.path.segments.last()?;

        if last_segment.ident != "Mapping" {
            return None;
        }

        if let syn::PathArguments::AngleBracketed(args) = &last_segment.arguments {
            let mut iter = args.args.iter();

            let key_type = if let Some(syn::GenericArgument::Type(ty)) = iter.next() {
                ty
            } else {
                return None;
            };
            let value_type = if let Some(syn::GenericArgument::Type(ty)) = iter.next() {
                ty
            } else {
                return None;
            };

            return Some((key_type, value_type));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use alloy_primitives::uint;
    use syn::parse_quote;

    use super::*;

    #[test]
    fn test_to_snake_case() {
        assert_eq!(to_snake_case("balanceOf"), "balance_of");
        assert_eq!(to_snake_case("transferFrom"), "transfer_from");
        assert_eq!(to_snake_case("name"), "name");
        assert_eq!(to_snake_case("already_snake"), "already_snake");
        assert_eq!(to_snake_case("updateQuoteToken"), "update_quote_token");
        assert_eq!(to_snake_case("DOMAIN_SEPARATOR"), "DOMAIN_SEPARATOR");
        assert_eq!(to_snake_case("ERC20Token"), "erc20_token");
    }

    #[test]
    fn test_to_camel_case() {
        assert_eq!(to_camel_case("balance_of"), "balanceOf");
        assert_eq!(to_camel_case("transfer_from"), "transferFrom");
        assert_eq!(to_camel_case("update_quote_token"), "updateQuoteToken");
        assert_eq!(to_camel_case("name"), "name");
    }

    #[test]
    fn test_extract_mapping_types() {
        let ty: Type = parse_quote!(Mapping<Address, U256>);
        assert!(extract_mapping_types(&ty).is_some());

        let ty: Type = parse_quote!(Mapping<Address, Mapping<Address, U256>>);
        assert!(extract_mapping_types(&ty).is_some());

        let ty: Type = parse_quote!(String);
        assert!(extract_mapping_types(&ty).is_none());

        let ty: Type = parse_quote!(Vec<u8>);
        assert!(extract_mapping_types(&ty).is_none());
    }

    #[test]
    fn test_erc7201_root() {
        let id: LitStr = parse_quote!("b20.policy");
        assert_eq!(
            erc7201_root(&id).unwrap(),
            uint!(0x50861ae81a7f4392b927efbaeecf8f091f3bd39245aa45ea91499a137b8b3100_U256)
        );
    }

    #[test]
    fn test_parse_namespace_id_rejects_whitespace() {
        let id: LitStr = parse_quote!("b20 policy");
        assert!(parse_namespace_id(id).is_err());
    }
}
