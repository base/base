use quote::{format_ident, quote};
use syn::{Expr, Ident, Visibility};

use crate::{
    FieldKind,
    packing::{self, LayoutField, PackingConstants, SlotAssignment},
    utils::NamespaceInfo,
};

pub(crate) fn gen_handler_field_decl(field: &LayoutField<'_>) -> proc_macro2::TokenStream {
    let field_name = field.name;
    let doc_str = format!("Storage handler for the `{field_name}` slot.");
    let handler_type = match &field.kind {
        FieldKind::Direct(ty) => {
            quote! { <#ty as ::base_precompile_storage::StorableType>::Handler<'a> }
        }
        FieldKind::Mapping { key, value } => {
            quote! { <::base_precompile_storage::Mapping<#key, #value> as ::base_precompile_storage::StorableType>::Handler<'a> }
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

    let shares_slot_check =
        gen_shares_slot_check(field, field_idx, all_fields, const_mod, is_contract);

    match &field.kind {
        FieldKind::Direct(ty) => {
            let layout_ctx = if is_contract {
                packing::gen_layout_ctx_expr(
                    ty,
                    matches!(field.assigned_slot, SlotAssignment::Manual(_)),
                    quote! { #const_mod::#offset_const },
                    shares_slot_check,
                )
            } else {
                packing::gen_layout_ctx_expr(
                    ty,
                    false,
                    quote! { #const_mod::#loc_const.offset_bytes },
                    shares_slot_check,
                )
            };

            quote! {
                #field_name: <#ty as ::base_precompile_storage::StorableType>::handle(
                    #slot_expr, #layout_ctx, address, storage
                )
            }
        }
        FieldKind::Mapping { key, value } => {
            quote! {
                #field_name: <::base_precompile_storage::Mapping<#key, #value> as ::base_precompile_storage::StorableType>::handle(
                    #slot_expr, ::base_precompile_storage::LayoutCtx::FULL, address, storage
                )
            }
        }
    }
}

fn gen_shares_slot_check(
    field: &LayoutField<'_>,
    field_idx: usize,
    all_fields: &[LayoutField<'_>],
    const_mod: &Ident,
    is_contract: bool,
) -> Option<proc_macro2::TokenStream> {
    let current_consts = PackingConstants::new(field.name);
    let current_slot = if is_contract {
        let current_slot = current_consts.slot();
        quote! { #const_mod::#current_slot }
    } else {
        let current_loc = current_consts.location();
        quote! { #const_mod::#current_loc.offset_slots }
    };

    let checks: Vec<_> = all_fields
        .iter()
        .enumerate()
        .filter(|(idx, _)| *idx != field_idx)
        .map(|(_, other)| {
            let other_consts = PackingConstants::new(other.name);
            if is_contract {
                let other_slot = other_consts.slot();
                quote! { #current_slot == #const_mod::#other_slot }
            } else {
                let other_loc = other_consts.location();
                quote! { #current_slot == #const_mod::#other_loc.offset_slots }
            }
        })
        .collect();

    if checks.is_empty() { None } else { Some(quote! { false #(|| #checks)* }) }
}

pub(crate) fn gen_struct(
    name: &Ident,
    vis: &Visibility,
    allocated_fields: &[LayoutField<'_>],
) -> proc_macro2::TokenStream {
    let handler_fields = allocated_fields.iter().map(gen_handler_field_decl);
    let doc_str = format!("Storage layout for the [`{name}`] precompile.");

    quote! {
        #[doc = #doc_str]
        #vis struct #name<'a> {
            #(#handler_fields,)*
            address: ::alloy_primitives::Address,
            storage: ::base_precompile_storage::StorageCtx<'a>,
        }
    }
}

pub(crate) fn gen_constructor(
    name: &Ident,
    allocated_fields: &[LayoutField<'_>],
    address: Option<&Expr>,
) -> proc_macro2::TokenStream {
    let field_inits = allocated_fields
        .iter()
        .enumerate()
        .map(|(idx, field)| gen_handler_field_init(field, idx, allocated_fields, None));

    let new_fn = address.map(|addr| {
        quote! {
            /// Creates an instance of the precompile.
            ///
            /// Caution: This does not initialize the account, see [`Self::initialize`].
            pub fn new(storage: ::base_precompile_storage::StorageCtx<'a>) -> Self {
                Self::__new(#addr, storage)
            }
        }
    });

    quote! {
        impl<'a> #name<'a> {
            #new_fn

            #[inline(always)]
            fn __new(
                address: ::alloy_primitives::Address,
                storage: ::base_precompile_storage::StorageCtx<'a>,
            ) -> Self {
                #[cfg(debug_assertions)]
                {
                    slots::__check_all_collisions();
                }

                Self {
                    #(#field_inits,)*
                    address,
                    storage,
                }
            }

            #[inline(always)]
            fn __initialize(&mut self) -> ::base_precompile_storage::Result<()> {
                let bytecode = ::revm::state::Bytecode::new_legacy(::alloy_primitives::Bytes::from_static(&[0xef]));
                self.storage.set_code(self.address, bytecode)?;
                Ok(())
            }

            #[inline(always)]
            fn emit_event(&mut self, event: impl ::alloy_primitives::IntoLogData) -> ::base_precompile_storage::Result<()> {
                self.storage.emit_event(self.address, event.into_log_data())
            }

            #[cfg(feature = "test-utils")]
            /// Returns all events emitted by this contract (test-utils only).
            pub fn emitted_events(&self) -> ::std::vec::Vec<::alloy_primitives::LogData> {
                self.storage.get_events(self.address)
            }

            #[cfg(feature = "test-utils")]
            /// Clears all events emitted by this contract (test-utils only).
            pub fn clear_emitted_events(&mut self) {
                self.storage.clear_events(self.address);
            }

            #[cfg(feature = "test-utils")]
            /// Asserts that emitted events match the expected list (test-utils only).
            pub fn assert_emitted_events(&self, expected: ::std::vec::Vec<impl ::alloy_primitives::IntoLogData>) {
                let emitted = self.storage.get_events(self.address);
                assert_eq!(emitted.len(), expected.len());
                for (i, event) in expected.into_iter().enumerate() {
                    assert_eq!(emitted[i], event.into_log_data());
                }
            }
        }
    }
}

pub(crate) fn gen_contract_storage_impl(name: &Ident) -> proc_macro2::TokenStream {
    quote! {
        impl<'a> ::base_precompile_storage::ContractStorage<'a> for #name<'a> {
            #[inline(always)]
            fn address(&self) -> ::alloy_primitives::Address {
                self.address
            }

            #[inline(always)]
            fn storage(&self) -> ::base_precompile_storage::StorageCtx<'a> {
                self.storage
            }
        }
    }
}

pub(crate) fn gen_slots_module(
    allocated_fields: &[LayoutField<'_>],
    namespace: Option<&NamespaceInfo>,
) -> proc_macro2::TokenStream {
    let namespace_constants = namespace.map(gen_namespace_constants);
    let constants = packing::gen_constants_from_ir(allocated_fields, false);
    let collision_checks = gen_collision_checks(allocated_fields);

    quote! {
        /// Storage slot indices and packing constants for this contract.
        pub mod slots {
            use super::*;

            #namespace_constants
            #constants
            #collision_checks
        }
    }
}

fn gen_namespace_constants(namespace: &NamespaceInfo) -> proc_macro2::TokenStream {
    let id = &namespace.id;
    let limbs = *namespace.root.as_limbs();

    quote! {
        /// ERC-7201 namespace identifier for this contract storage layout.
        pub const NAMESPACE_ID: &str = #id;

        /// ERC-7201 namespace root slot for this contract storage layout.
        pub const NAMESPACE_ROOT: ::alloy_primitives::U256 =
            ::alloy_primitives::U256::from_limbs([#(#limbs),*]);
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
