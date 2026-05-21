//! Shared code generation utilities for storage slot packing.
//!
//! This module provides common logic for computing slot and offset assignments
//! used by both the `#[derive(Storable)]` and `#[contract]` macros.

use alloy_primitives::U256;
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Ident, Type};

use crate::{FieldInfo, FieldKind};

/// Helper for generating packing constant identifiers
pub(crate) struct PackingConstants(String);

impl PackingConstants {
    pub(crate) fn new(name: &Ident) -> Self {
        Self(const_name(name))
    }

    pub(crate) fn slot(&self) -> Ident {
        format_ident!("{}", &self.0)
    }

    pub(crate) fn location(&self) -> Ident {
        let span = proc_macro2::Span::call_site();
        Ident::new(&format!("{}_LOC", self.0), span)
    }

    pub(crate) fn offset(&self) -> Ident {
        let span = proc_macro2::Span::call_site();
        Ident::new(&format!("{}_OFFSET", self.0), span)
    }

    pub(crate) fn into_tuple(self) -> (Ident, Ident) {
        (self.slot(), self.offset())
    }
}

pub(crate) fn const_name(name: &Ident) -> String {
    name.to_string().to_uppercase()
}

#[derive(Debug, Clone)]
pub(crate) enum SlotAssignment {
    Manual(U256),
    Auto { base_slot: U256 },
}

impl SlotAssignment {
    pub(crate) const fn ref_slot(&self) -> &U256 {
        match self {
            Self::Manual(slot) => slot,
            Self::Auto { base_slot } => base_slot,
        }
    }
}

#[derive(Debug)]
pub(crate) struct LayoutField<'a> {
    pub name: &'a Ident,
    pub ty: &'a Type,
    pub kind: FieldKind<'a>,
    pub assigned_slot: SlotAssignment,
}

/// Build layout IR from field information.
pub(crate) fn allocate_slots(fields: &[FieldInfo]) -> syn::Result<Vec<LayoutField<'_>>> {
    allocate_slots_from(fields, U256::ZERO)
}

/// Build layout IR from field information, starting auto-allocation at `initial_base_slot`.
pub(crate) fn allocate_slots_from(
    fields: &[FieldInfo],
    initial_base_slot: U256,
) -> syn::Result<Vec<LayoutField<'_>>> {
    let mut result = Vec::with_capacity(fields.len());
    let mut current_base_slot = initial_base_slot;

    for field in fields {
        let kind = classify_field_type(&field.ty)?;

        let assigned_slot = match (field.slot, field.base_slot, field.namespace.as_ref()) {
            (Some(explicit), _, _) => SlotAssignment::Manual(explicit),
            (None, Some(new_base), _) => {
                current_base_slot = new_base;
                SlotAssignment::Auto { base_slot: new_base }
            }
            (None, None, Some(namespace)) => SlotAssignment::Auto { base_slot: namespace.root },
            (None, None, None) => SlotAssignment::Auto { base_slot: current_base_slot },
        };

        result.push(LayoutField { name: &field.name, ty: &field.ty, kind, assigned_slot });
    }

    Ok(result)
}

/// Generate packing constants from layout IR.
pub(crate) fn gen_constants_from_ir(fields: &[LayoutField<'_>], gen_location: bool) -> TokenStream {
    let mut constants = TokenStream::new();
    let mut last_auto_fields = Vec::<&LayoutField<'_>>::new();

    for field in fields {
        let ty = field.ty;
        let consts = PackingConstants::new(field.name);
        let (loc_const, (slot_const, offset_const)) = (consts.location(), consts.into_tuple());
        let slots_to_end = quote! {
            ::alloy_primitives::U256::from_limbs([<#ty as ::base_precompile_storage::StorableType>::SLOTS as u64, 0, 0, 0])
                .saturating_sub(::alloy_primitives::U256::ONE)
        };

        let bytes_expr = quote! { <#ty as ::base_precompile_storage::StorableType>::BYTES };

        let (slot_expr, offset_expr) = match &field.assigned_slot {
            SlotAssignment::Manual(manual_slot) => {
                let hex_value = format!("{manual_slot}_U256");
                let slot_lit = syn::LitInt::new(&hex_value, proc_macro2::Span::call_site());
                let slot_expr = quote! {
                    ::alloy_primitives::uint!(#slot_lit)
                        .checked_add(#slots_to_end).expect("slot overflow")
                        .saturating_sub(#slots_to_end)
                };
                (slot_expr, quote! { 0 })
            }
            SlotAssignment::Auto { base_slot, .. } => {
                let output = last_auto_fields
                    .iter()
                    .rev()
                    .find(|candidate| candidate.assigned_slot.ref_slot() == base_slot)
                    .map_or_else(
                        || {
                            let limbs = *base_slot.as_limbs();
                            let slot_expr = quote! {
                                ::alloy_primitives::U256::from_limbs([#(#limbs),*])
                                    .checked_add(#slots_to_end).expect("slot overflow")
                                    .saturating_sub(#slots_to_end)
                            };
                            (slot_expr, quote! { 0 })
                        },
                        |current_base| {
                            let (prev_slot, prev_offset) =
                                PackingConstants::new(current_base.name).into_tuple();
                            gen_slot_packing_logic(
                                current_base.ty,
                                field.ty,
                                quote! { #prev_slot },
                                quote! { #prev_offset },
                            )
                        },
                    );
                last_auto_fields.push(field);
                output
            }
        };

        let slot_doc = format!("Base storage slot for the `{}` field.", field.name);
        let offset_doc = format!("Byte offset within the slot for the `{}` field.", field.name);
        constants.extend(quote! {
            #[doc = #slot_doc]
            pub const #slot_const: ::alloy_primitives::U256 = #slot_expr;
            #[doc = #offset_doc]
            pub const #offset_const: usize = #offset_expr;
        });

        if gen_location {
            let loc_doc = format!("Storage location descriptor for the `{}` field.", field.name);
            constants.extend(quote! {
                #[doc = #loc_doc]
                pub const #loc_const: ::base_precompile_storage::FieldLocation =
                    ::base_precompile_storage::FieldLocation::new(#slot_const.as_limbs()[0] as usize, #offset_const, #bytes_expr);
            });
        }

        #[cfg(debug_assertions)]
        {
            let bytes_const = format_ident!("{slot_const}_BYTES");
            let bytes_doc = format!("Size in bytes of the `{}` field.", field.name);
            constants.extend(quote! {
                #[doc = #bytes_doc]
                pub const #bytes_const: usize = #bytes_expr;
            });
        }
    }

    constants
}

/// Classify a field based on its type.
pub(crate) fn classify_field_type(ty: &Type) -> syn::Result<FieldKind<'_>> {
    use crate::utils::extract_mapping_types;

    if let Some((key_ty, value_ty)) = extract_mapping_types(ty) {
        return Ok(FieldKind::Mapping { key: key_ty, value: value_ty });
    }

    Ok(FieldKind::Direct(ty))
}

/// Helper to compute prev and next slot constant references for a field at a given index.
pub(crate) fn get_neighbor_slot_refs<T, F>(
    idx: usize,
    fields: &[T],
    packing: &Ident,
    get_name: F,
    use_full_slot: bool,
) -> (Option<TokenStream>, Option<TokenStream>)
where
    F: Fn(&T) -> &Ident,
{
    let prev_slot_ref = if idx > 0 {
        let prev_name = get_name(&fields[idx - 1]);
        if use_full_slot {
            let prev_slot = PackingConstants::new(prev_name).slot();
            Some(quote! { #packing::#prev_slot })
        } else {
            let prev_loc = PackingConstants::new(prev_name).location();
            Some(quote! { #packing::#prev_loc.offset_slots })
        }
    } else {
        None
    };

    let next_slot_ref = if idx + 1 < fields.len() {
        let next_name = get_name(&fields[idx + 1]);
        if use_full_slot {
            let next_slot = PackingConstants::new(next_name).slot();
            Some(quote! { #packing::#next_slot })
        } else {
            let next_loc = PackingConstants::new(next_name).location();
            Some(quote! { #packing::#next_loc.offset_slots })
        }
    } else {
        None
    };

    (prev_slot_ref, next_slot_ref)
}

/// Returns previous and next slot constants for fields that share the same auto-allocation root.
pub(crate) fn get_same_root_neighbor_slot_refs(
    idx: usize,
    fields: &[LayoutField<'_>],
    packing: &Ident,
) -> (Option<TokenStream>, Option<TokenStream>) {
    if !matches!(fields[idx].assigned_slot, SlotAssignment::Auto { .. }) {
        return (None, None);
    }

    let root = fields[idx].assigned_slot.ref_slot();
    let prev_slot_ref = fields[..idx]
        .iter()
        .rev()
        .find(|field| {
            matches!(field.assigned_slot, SlotAssignment::Auto { .. })
                && field.assigned_slot.ref_slot() == root
        })
        .map(|field| {
            let prev_slot = PackingConstants::new(field.name).slot();
            quote! { #packing::#prev_slot }
        });

    let next_slot_ref = fields[idx + 1..]
        .iter()
        .find(|field| {
            matches!(field.assigned_slot, SlotAssignment::Auto { .. })
                && field.assigned_slot.ref_slot() == root
        })
        .map(|field| {
            let next_slot = PackingConstants::new(field.name).slot();
            quote! { #packing::#next_slot }
        });

    (prev_slot_ref, next_slot_ref)
}

/// Generate slot packing decision logic.
pub(crate) fn gen_slot_packing_logic(
    prev_ty: &Type,
    curr_ty: &Type,
    prev_slot_expr: TokenStream,
    prev_offset_expr: TokenStream,
) -> (TokenStream, TokenStream) {
    let prev_layout_slots = quote! {
        ::alloy_primitives::U256::from_limbs([<#prev_ty as ::base_precompile_storage::StorableType>::SLOTS as u64, 0, 0, 0])
    };
    let curr_slots_to_end = quote! {
        ::alloy_primitives::U256::from_limbs([<#curr_ty as ::base_precompile_storage::StorableType>::SLOTS as u64, 0, 0, 0])
            .saturating_sub(::alloy_primitives::U256::ONE)
    };

    let can_pack_expr = quote! {
        #prev_offset_expr
            + <#prev_ty as ::base_precompile_storage::StorableType>::BYTES
            + <#curr_ty as ::base_precompile_storage::StorableType>::BYTES <= 32
    };

    let slot_expr = quote! {{
        if #can_pack_expr {
            #prev_slot_expr
        } else {
            #prev_slot_expr
                .checked_add(#prev_layout_slots).expect("slot overflow")
                .checked_add(#curr_slots_to_end).expect("slot overflow")
                .saturating_sub(#curr_slots_to_end)
        }
    }};

    let offset_expr = quote! {{
        if #can_pack_expr { #prev_offset_expr + <#prev_ty as ::base_precompile_storage::StorableType>::BYTES } else { 0 }
    }};

    (slot_expr, offset_expr)
}

/// Generate [`LayoutCtx`] expression for accessing a field.
pub(crate) fn gen_layout_ctx_expr(
    ty: &Type,
    is_manual_slot: bool,
    slot_const_ref: TokenStream,
    offset_const_ref: TokenStream,
    prev_slot_const_ref: Option<TokenStream>,
    next_slot_const_ref: Option<TokenStream>,
) -> TokenStream {
    if !is_manual_slot && (prev_slot_const_ref.is_some() || next_slot_const_ref.is_some()) {
        let prev_check = prev_slot_const_ref.map(|prev| quote! { #slot_const_ref == #prev });
        let next_check = next_slot_const_ref.map(|next| quote! { #slot_const_ref == #next });

        let shares_slot_check = match (prev_check, next_check) {
            (Some(prev), Some(next)) => quote! { (#prev || #next) },
            (Some(prev), None) => prev,
            (None, Some(next)) => next,
            (None, None) => unreachable!(),
        };

        quote! {
            {
                if #shares_slot_check && <#ty as ::base_precompile_storage::StorableType>::IS_PACKABLE {
                    ::base_precompile_storage::LayoutCtx::packed(#offset_const_ref)
                } else {
                    ::base_precompile_storage::LayoutCtx::FULL
                }
            }
        }
    } else {
        quote! { ::base_precompile_storage::LayoutCtx::FULL }
    }
}

/// Generate collision detection debug assertions for a field against all other fields.
pub(crate) fn gen_collision_check_fn(
    idx: usize,
    field: &LayoutField<'_>,
    all_fields: &[LayoutField<'_>],
) -> (Ident, TokenStream) {
    fn gen_slot_count_expr(ty: &Type) -> TokenStream {
        quote! { ::alloy_primitives::U256::from_limbs([<#ty as ::base_precompile_storage::StorableType>::SLOTS as u64, 0, 0, 0]) }
    }

    let check_fn_name = format_ident!("__check_collision_{}", field.name);
    let consts = PackingConstants::new(field.name);
    let (slot_const, offset_const) = consts.into_tuple();
    let (field_name, field_ty) = (field.name, field.ty);

    let mut checks = TokenStream::new();

    for (other_idx, other_field) in all_fields.iter().enumerate() {
        if other_idx == idx {
            continue;
        }

        let other_consts = PackingConstants::new(other_field.name);
        let (other_slot_const, other_offset_const) = other_consts.into_tuple();
        let other_name = other_field.name;
        let other_ty = other_field.ty;

        let current_count_expr = gen_slot_count_expr(field.ty);
        let other_count_expr = gen_slot_count_expr(other_field.ty);

        checks.extend(quote! {
            {
                let slot = #slot_const;
                let slot_end = slot.checked_add(#current_count_expr).expect("slot range overflow");
                let other_slot = #other_slot_const;
                let other_slot_end = other_slot.checked_add(#other_count_expr).expect("slot range overflow");

                let no_overlap = if slot == other_slot {
                    let byte_end = #offset_const + <#field_ty as ::base_precompile_storage::StorableType>::BYTES;
                    let other_byte_end = #other_offset_const + <#other_ty as ::base_precompile_storage::StorableType>::BYTES;
                    byte_end <= #other_offset_const || other_byte_end <= #offset_const
                } else {
                    slot_end.le(&other_slot) || other_slot_end.le(&slot)
                };

                debug_assert!(
                    no_overlap,
                    "Storage slot collision: field `{}` (slot {:?}, offset {}) overlaps with field `{}` (slot {:?}, offset {})",
                    stringify!(#field_name),
                    slot,
                    #offset_const,
                    stringify!(#other_name),
                    other_slot,
                    #other_offset_const
                );
            }
        });
    }

    let check_fn = quote! {
        #[cfg(debug_assertions)]
        #[inline(always)]
        #[allow(non_snake_case)]
        fn #check_fn_name() {
            #checks
        }
    };

    (check_fn_name, check_fn)
}
