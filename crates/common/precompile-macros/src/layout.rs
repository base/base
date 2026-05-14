use quote::{format_ident, quote};
use syn::{Expr, Ident, Visibility};

use crate::{
    FieldKind,
    packing::{self, LayoutField, PackingConstants, SlotAssignment},
};

pub(crate) fn gen_handler_field_decl(field: &LayoutField<'_>) -> proc_macro2::TokenStream {
    let field_name = field.name;
    let doc_str = format!("Storage handler for the `{field_name}` slot.");
    let handler_type = match &field.kind {
        FieldKind::Direct(ty) => {
            quote! { <#ty as ::base_precompile_storage::StorableType>::Handler }
        }
        FieldKind::Mapping { key, value } => {
            quote! { <::base_precompile_storage::Mapping<#key, #value> as ::base_precompile_storage::StorableType>::Handler }
        }
    };

    quote! {
        #[doc = #doc_str]
        pub #field_name: #handler_type
    }
}

pub(crate) fn gen_handler_field_init(
    field: &LayoutField<'_>,
    field_idx: usize,
    all_fields: &[LayoutField<'_>],
    packing_mod: Option<&Ident>,
) -> proc_macro2::TokenStream {
    let field_name = field.name;
    let handler = gen_handler_expr(field, field_idx, all_fields, packing_mod);

    quote! {
        #field_name: #handler
    }
}

pub(crate) fn gen_handler_expr(
    field: &LayoutField<'_>,
    field_idx: usize,
    all_fields: &[LayoutField<'_>],
    packing_mod: Option<&Ident>,
) -> proc_macro2::TokenStream {
    let field_name = field.name;
    let consts = PackingConstants::new(field_name);
    let (loc_const, (slot_const, offset_const)) = (consts.location(), consts.into_tuple());

    let is_contract = packing_mod.is_none();
    let slots_mod = format_ident!("slots");
    let const_mod = packing_mod.unwrap_or(&slots_mod);

    let slot_expr = if is_contract {
        quote! { #const_mod::#slot_const }
    } else {
        quote! { base_slot.saturating_add(::alloy_primitives::U256::from_limbs([#const_mod::#loc_const.offset_slots as u64, 0, 0, 0])) }
    };

    match &field.kind {
        FieldKind::Direct(ty) => {
            let (prev_slot_const_ref, next_slot_const_ref) = packing::get_neighbor_slot_refs(
                field_idx,
                all_fields,
                const_mod,
                |f| f.name,
                is_contract,
            );

            let layout_ctx = if is_contract {
                packing::gen_layout_ctx_expr(
                    ty,
                    matches!(field.assigned_slot, SlotAssignment::Manual(_)),
                    quote! { #const_mod::#slot_const },
                    quote! { #const_mod::#offset_const },
                    prev_slot_const_ref,
                    next_slot_const_ref,
                )
            } else {
                packing::gen_layout_ctx_expr(
                    ty,
                    false,
                    quote! { #const_mod::#loc_const.offset_slots },
                    quote! { #const_mod::#loc_const.offset_bytes },
                    prev_slot_const_ref,
                    next_slot_const_ref,
                )
            };

            quote! {
                <#ty as ::base_precompile_storage::StorableType>::handle(
                    #slot_expr, #layout_ctx, address
                )
            }
        }
        FieldKind::Mapping { key, value } => {
            quote! {
                <::base_precompile_storage::Mapping<#key, #value> as ::base_precompile_storage::StorableType>::handle(
                    #slot_expr, ::base_precompile_storage::LayoutCtx::FULL, address
                )
            }
        }
    }
}

pub(crate) fn gen_struct(
    name: &Ident,
    vis: &Visibility,
    _allocated_fields: &[LayoutField<'_>],
) -> proc_macro2::TokenStream {
    let doc_str = format!("Storage layout for the [`{name}`] precompile.");

    quote! {
        #[doc = #doc_str]
        #vis struct #name {
            address: ::alloy_primitives::Address,
            storage: ::base_precompile_storage::StorageCtx,
        }
    }
}

pub(crate) fn gen_constructor(
    name: &Ident,
    allocated_fields: &[LayoutField<'_>],
    address: Option<&Expr>,
) -> proc_macro2::TokenStream {
    let accessors = allocated_fields
        .iter()
        .enumerate()
        .map(|(idx, field)| gen_accessor_method(field, idx, allocated_fields));

    let new_fn = address.map(|addr| {
        quote! {
            /// Creates an instance of the precompile.
            ///
            /// Caution: This does not initialize the account. See [`Self::initialize`].
            pub fn new() -> Self {
                Self::with_address(#addr)
            }
        }
    });

    quote! {
        impl #name {
            #new_fn

            /// Creates an instance of the precompile at the given address.
            ///
            /// Caution: This does not initialize the account. See [`Self::initialize`].
            pub fn with_address(address: ::alloy_primitives::Address) -> Self {
                #[cfg(debug_assertions)]
                {
                    slots::__check_all_collisions();
                }

                Self {
                    address,
                    storage: ::base_precompile_storage::StorageCtx::default(),
                }
            }

            #(#accessors)*

            /// Initializes the precompile account by installing marker bytecode.
            #[inline(always)]
            pub fn initialize(&mut self) -> ::base_precompile_storage::Result<()> {
                let bytecode = ::revm::state::Bytecode::new_legacy(::alloy_primitives::Bytes::from_static(&[0xef]));
                self.storage.set_code(self.address, bytecode)?;
                Ok(())
            }

            /// Emits an event from this precompile address.
            #[inline(always)]
            pub fn emit_event(&mut self, event: impl ::alloy_primitives::IntoLogData) -> ::base_precompile_storage::Result<()> {
                self.storage.emit_event(self.address, event.into_log_data())
            }

            #[cfg(any(test, feature = "test-utils"))]
            /// Returns all events emitted by this contract (test-utils only).
            pub fn emitted_events(&self) -> &Vec<::alloy_primitives::LogData> {
                self.storage.get_events(self.address)
            }

            #[cfg(any(test, feature = "test-utils"))]
            /// Clears all events emitted by this contract (test-utils only).
            pub fn clear_emitted_events(&mut self) {
                self.storage.clear_events(self.address);
            }

            #[cfg(any(test, feature = "test-utils"))]
            /// Asserts that emitted events match the expected list (test-utils only).
            pub fn assert_emitted_events(&self, expected: Vec<impl ::alloy_primitives::IntoLogData>) {
                let emitted = self.storage.get_events(self.address);
                assert_eq!(emitted.len(), expected.len());
                for (i, event) in expected.into_iter().enumerate() {
                    assert_eq!(emitted[i], event.into_log_data());
                }
            }
        }
    }
}

fn gen_accessor_method(
    field: &LayoutField<'_>,
    field_idx: usize,
    all_fields: &[LayoutField<'_>],
) -> proc_macro2::TokenStream {
    let field_name = field.name;
    let doc_str = format!("Returns a storage handler for the `{field_name}` field.");
    let handler_type = match &field.kind {
        FieldKind::Direct(ty) => {
            quote! { <#ty as ::base_precompile_storage::StorableType>::Handler }
        }
        FieldKind::Mapping { key, value } => {
            quote! { <::base_precompile_storage::Mapping<#key, #value> as ::base_precompile_storage::StorableType>::Handler }
        }
    };
    let handler = gen_handler_expr(field, field_idx, all_fields, None);

    quote! {
        #[doc = #doc_str]
        #[inline(always)]
        pub fn #field_name(&self) -> #handler_type {
            let address = self.address;
            #handler
        }
    }
}

pub(crate) fn gen_contract_storage_impl(name: &Ident) -> proc_macro2::TokenStream {
    quote! {
        impl ::base_precompile_storage::ContractStorage for #name {
            #[inline(always)]
            fn address(&self) -> ::alloy_primitives::Address {
                self.address
            }

            #[inline(always)]
            fn storage(&self) -> &::base_precompile_storage::StorageCtx {
                &self.storage
            }

            #[inline(always)]
            fn storage_mut(&mut self) -> &mut ::base_precompile_storage::StorageCtx {
                &mut self.storage
            }
        }
    }
}

pub(crate) fn gen_slots_module(allocated_fields: &[LayoutField<'_>]) -> proc_macro2::TokenStream {
    let constants = packing::gen_constants_from_ir(allocated_fields, false);
    let collision_checks = gen_collision_checks(allocated_fields);

    quote! {
        /// Storage slot indices and packing constants for this contract.
        pub mod slots {
            use super::*;

            #constants
            #collision_checks
        }
    }
}

fn gen_collision_checks(allocated_fields: &[LayoutField<'_>]) -> proc_macro2::TokenStream {
    let mut generated = proc_macro2::TokenStream::new();
    let mut check_fn_calls = Vec::new();

    for (idx, allocated) in allocated_fields.iter().enumerate() {
        let (check_fn_name, check_fn) =
            packing::gen_collision_check_fn(idx, allocated, allocated_fields);
        generated.extend(check_fn);
        check_fn_calls.push(check_fn_name);
    }

    generated.extend(quote! {
        #[cfg(debug_assertions)]
        #[inline(always)]
        pub(super) fn __check_all_collisions() {
            #(#check_fn_calls();)*
        }
    });

    generated
}

pub(crate) fn gen_default_impl(name: &Ident) -> proc_macro2::TokenStream {
    quote! {
        impl ::core::default::Default for #name {
            fn default() -> Self {
                Self::new()
            }
        }
    }
}
