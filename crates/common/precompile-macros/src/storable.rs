//! Implementation of the `#[derive(Storable)]` macro.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Attribute, Data, DataEnum, DataStruct, DeriveInput, Fields, Ident, Type};

use crate::{
    FieldInfo,
    layout::{gen_handler_field_decl, gen_handler_field_init},
    packing::{self, LayoutField, PackingConstants},
    storable_primitives::gen_struct_arrays,
    utils::{extract_mapping_types, extract_storable_array_sizes, to_snake_case},
};

/// Entry point called from `lib.rs` — parses input and converts errors to compile errors.
pub(crate) fn derive(input: DeriveInput) -> proc_macro::TokenStream {
    match derive_impl(input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

pub(crate) fn derive_impl(input: DeriveInput) -> syn::Result<TokenStream> {
    match &input.data {
        Data::Struct(data_struct) => derive_struct_impl(&input, data_struct),
        Data::Enum(data_enum) => derive_unit_enum_impl(&input, data_enum),
        _ => Err(syn::Error::new_spanned(
            &input.ident,
            "`Storable` can only be derived for structs with named fields or unit enums",
        )),
    }
}

fn derive_struct_impl(input: &DeriveInput, data_struct: &DataStruct) -> syn::Result<TokenStream> {
    let strukt = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let fields = match &data_struct.fields {
        Fields::Named(fields_named) => &fields_named.named,
        _ => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "`Storable` can only be derived for structs with named fields",
            ));
        }
    };

    if fields.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "`Storable` cannot be derived for empty structs",
        ));
    }

    let field_infos: Vec<_> = fields
        .iter()
        .map(|f| FieldInfo {
            name: f.ident.as_ref().unwrap().clone(),
            ty: f.ty.clone(),
            slot: None,
            base_slot: None,
        })
        .collect();

    let layout_fields = packing::allocate_slots(&field_infos)?;

    let mod_ident = format_ident!("__packing_{}", to_snake_case(&strukt.to_string()));
    let packing_module = gen_packing_module_from_ir(&layout_fields, &mod_ident);

    let len = fields.len();
    let (direct_fields, direct_names, mapping_names) = field_infos.iter().fold(
        (Vec::with_capacity(len), Vec::with_capacity(len), Vec::new()),
        |mut out, field_info| {
            if extract_mapping_types(&field_info.ty).is_none() {
                out.0.push((&field_info.name, &field_info.ty));
                out.1.push(&field_info.name);
            } else {
                out.2.push(&field_info.name);
            }
            out
        },
    );

    let direct_tys: Vec<_> = direct_fields.iter().map(|(_, ty)| *ty).collect();

    let load_impl = gen_load_impl(&direct_fields, &mod_ident);
    let store_impl = gen_store_impl(&direct_fields, &mod_ident);
    let delete_impl = gen_delete_impl(&direct_fields, &mod_ident);

    let handler_struct = gen_handler_struct(strukt, &layout_fields, &mod_ident);
    let handler_name = format_ident!("{}Handler", strukt);

    let expanded = quote! {
        #packing_module
        #handler_struct

        impl #impl_generics ::base_precompile_storage::StorableType for #strukt #ty_generics #where_clause {
            const LAYOUT: ::base_precompile_storage::Layout = ::base_precompile_storage::Layout::Slots(#mod_ident::SLOT_COUNT);

            const IS_DYNAMIC: bool = #(
                <#direct_tys as ::base_precompile_storage::StorableType>::IS_DYNAMIC
            )||*;

            type Handler<'a> = #handler_name<'a>;

            fn handle<'a>(
                slot: ::alloy_primitives::U256,
                _ctx: ::base_precompile_storage::LayoutCtx,
                address: ::alloy_primitives::Address,
                storage: ::base_precompile_storage::StorageCtx<'a>,
            ) -> Self::Handler<'a> {
                #handler_name::new(slot, address, storage)
            }
        }

        impl #impl_generics ::base_precompile_storage::Storable for #strukt #ty_generics #where_clause {
            fn load<S: ::base_precompile_storage::StorageOps>(
                storage: &S,
                base_slot: ::alloy_primitives::U256,
                ctx: ::base_precompile_storage::LayoutCtx
            ) -> ::base_precompile_storage::Result<Self> {
                use ::base_precompile_storage::Storable;
                debug_assert_eq!(ctx, ::base_precompile_storage::LayoutCtx::FULL, "Struct types can only be loaded with LayoutCtx::FULL");

                #load_impl

                Ok(Self {
                    #(#direct_names),*,
                    #(#mapping_names: Default::default()),*
                })
            }

            fn store<S: ::base_precompile_storage::StorageOps>(
                &self,
                storage: &mut S,
                base_slot: ::alloy_primitives::U256,
                ctx: ::base_precompile_storage::LayoutCtx
            ) -> ::base_precompile_storage::Result<()> {
                use ::base_precompile_storage::Storable;
                debug_assert_eq!(ctx, ::base_precompile_storage::LayoutCtx::FULL, "Struct types can only be stored with LayoutCtx::FULL");

                #store_impl

                Ok(())
            }

            fn delete<S: ::base_precompile_storage::StorageOps>(
                storage: &mut S,
                base_slot: ::alloy_primitives::U256,
                ctx: ::base_precompile_storage::LayoutCtx
            ) -> ::base_precompile_storage::Result<()> {
                use ::base_precompile_storage::Storable;
                debug_assert_eq!(ctx, ::base_precompile_storage::LayoutCtx::FULL, "Struct types can only be deleted with LayoutCtx::FULL");

                #delete_impl

                Ok(())
            }
        }
    };

    let array_impls = extract_storable_array_sizes(&input.attrs)?.map_or_else(
        || quote! {},
        |sizes| {
            let struct_type = quote! { #strukt #ty_generics };
            gen_struct_arrays(struct_type, &sizes)
        },
    );

    Ok(quote! {
        #expanded
        #array_impls
    })
}

fn derive_unit_enum_impl(input: &DeriveInput, data_enum: &DataEnum) -> syn::Result<TokenStream> {
    if extract_storable_array_sizes(&input.attrs)?.is_some() {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "`storable_arrays` is only supported for structs",
        ));
    }

    if !has_repr_u8(&input.attrs)? {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "`Storable` unit enums must be annotated with `#[repr(u8)]`",
        ));
    }

    if data_enum.variants.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "`Storable` cannot be derived for empty enums",
        ));
    }

    for variant in &data_enum.variants {
        if !matches!(variant.fields, Fields::Unit) {
            return Err(syn::Error::new_spanned(
                variant,
                "`Storable` enums must use unit variants only",
            ));
        }
    }

    validate_sequential_discriminants(data_enum)?;

    let enum_name = &input.ident;
    let variant_names: Vec<_> = data_enum.variants.iter().map(|variant| &variant.ident).collect();
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    Ok(quote! {
        impl #impl_generics ::base_precompile_storage::StorableType for #enum_name #ty_generics #where_clause {
            const LAYOUT: ::base_precompile_storage::Layout = ::base_precompile_storage::Layout::Bytes(1);
            type Handler<'a> = ::base_precompile_storage::Slot<'a, Self>;

            fn handle<'a>(
                slot: ::alloy_primitives::U256,
                ctx: ::base_precompile_storage::LayoutCtx,
                address: ::alloy_primitives::Address,
                storage: ::base_precompile_storage::StorageCtx<'a>,
            ) -> Self::Handler<'a> {
                ::base_precompile_storage::Slot::new_with_ctx(slot, ctx, address, storage)
            }
        }

        impl #impl_generics ::base_precompile_storage::Storable for #enum_name #ty_generics #where_clause {
            #[inline]
            fn load<S: ::base_precompile_storage::StorageOps>(
                storage: &S,
                slot: ::alloy_primitives::U256,
                ctx: ::base_precompile_storage::LayoutCtx
            ) -> ::base_precompile_storage::Result<Self> {
                let value = <u8 as ::base_precompile_storage::Storable>::load(storage, slot, ctx)?;
                match value {
                    #(discriminant if discriminant == Self::#variant_names as u8 => Ok(Self::#variant_names),)*
                    _ => Err(::base_precompile_storage::BasePrecompileError::enum_conversion_error()),
                }
            }

            #[inline]
            fn store<S: ::base_precompile_storage::StorageOps>(
                &self,
                storage: &mut S,
                slot: ::alloy_primitives::U256,
                ctx: ::base_precompile_storage::LayoutCtx
            ) -> ::base_precompile_storage::Result<()> {
                let value = match self {
                    #(Self::#variant_names => Self::#variant_names as u8,)*
                };
                <u8 as ::base_precompile_storage::Storable>::store(&value, storage, slot, ctx)
            }
        }
    })
}

fn has_repr_u8(attrs: &[Attribute]) -> syn::Result<bool> {
    let mut repr_u8 = false;
    for attr in attrs {
        if !attr.path().is_ident("repr") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("u8") {
                repr_u8 = true;
            }
            Ok(())
        })?;
    }
    Ok(repr_u8)
}

fn validate_sequential_discriminants(data_enum: &DataEnum) -> syn::Result<()> {
    if data_enum.variants.len() > usize::from(u8::MAX) + 1 {
        return Err(syn::Error::new_spanned(
            &data_enum.variants,
            "`Storable` unit enums must have at most 256 variants",
        ));
    }
    for variant in &data_enum.variants {
        if variant.discriminant.is_some() {
            return Err(syn::Error::new_spanned(
                variant,
                "`Storable` unit enums must not use explicit discriminants; \
                 variants are assigned sequential values starting from 0, matching Solidity enum semantics",
            ));
        }
    }
    Ok(())
}

fn gen_packing_module_from_ir(fields: &[LayoutField<'_>], mod_ident: &Ident) -> TokenStream {
    let last_field = &fields[fields.len() - 1];
    let last_slot_const = PackingConstants::new(last_field.name).slot();
    let packing_constants = packing::gen_constants_from_ir(fields, true);
    let last_type = &last_field.ty;

    quote! {
        pub mod #mod_ident {
            use super::*;

            #packing_constants
            pub const SLOT_COUNT: usize = (#last_slot_const.saturating_add(
                ::alloy_primitives::U256::from_limbs([<#last_type as ::base_precompile_storage::StorableType>::SLOTS as u64, 0, 0, 0])
            )).as_limbs()[0] as usize;
        }
    }
}

fn gen_handler_struct(
    struct_name: &Ident,
    fields: &[LayoutField<'_>],
    mod_ident: &Ident,
) -> TokenStream {
    let handler_name = format_ident!("{}Handler", struct_name);
    let handler_fields = fields.iter().map(gen_handler_field_decl);
    let field_inits = fields
        .iter()
        .enumerate()
        .map(|(idx, field)| gen_handler_field_init(field, idx, fields, Some(mod_ident)));

    quote! {
        /// Type-safe handler for accessing `#struct_name` in storage.
        #[derive(Debug, Clone)]
        pub struct #handler_name<'a> {
            address: ::alloy_primitives::Address,
            base_slot: ::alloy_primitives::U256,
            storage: ::base_precompile_storage::StorageCtx<'a>,
            #(#handler_fields,)*
        }

        impl<'a> #handler_name<'a> {
            #[inline]
            pub fn new(
                base_slot: ::alloy_primitives::U256,
                address: ::alloy_primitives::Address,
                storage: ::base_precompile_storage::StorageCtx<'a>,
            ) -> Self {
                Self {
                    base_slot,
                    storage,
                    #(#field_inits,)*
                    address,
                }
            }

            #[inline]
            pub fn base_slot(&self) -> ::alloy_primitives::U256 {
                self.base_slot
            }

            #[inline]
            fn as_slot(&self) -> ::base_precompile_storage::Slot<'a, #struct_name> {
                ::base_precompile_storage::Slot::<#struct_name>::new(
                    self.base_slot,
                    self.address,
                    self.storage,
                )
            }
        }

        impl ::base_precompile_storage::Handler<#struct_name> for #handler_name<'_> {
            #[inline]
            fn read(&self) -> ::base_precompile_storage::Result<#struct_name> {
                self.as_slot().read()
            }
            #[inline]
            fn write(&mut self, value: #struct_name) -> ::base_precompile_storage::Result<()> {
                self.as_slot().write(value)
            }
            #[inline]
            fn delete(&mut self) -> ::base_precompile_storage::Result<()> {
                self.as_slot().delete()
            }
            #[inline]
            fn t_read(&self) -> ::base_precompile_storage::Result<#struct_name> {
                self.as_slot().t_read()
            }
            #[inline]
            fn t_write(&mut self, value: #struct_name) -> ::base_precompile_storage::Result<()> {
                self.as_slot().t_write(value)
            }
            #[inline]
            fn t_delete(&mut self) -> ::base_precompile_storage::Result<()> {
                self.as_slot().t_delete()
            }
        }
    }
}

fn gen_load_impl(fields: &[(&Ident, &Type)], packing: &Ident) -> TokenStream {
    if fields.is_empty() {
        return quote! {};
    }

    let field_loads = fields.iter().enumerate().map(|(idx, (name, ty))| {
        let loc_const = PackingConstants::new(name).location();

        let (prev_slot_ref, _) =
            packing::get_neighbor_slot_refs(idx, fields, packing, |(name, _)| name, false);

        let slot_addr = quote! { base_slot + ::alloy_primitives::U256::from(#packing::#loc_const.offset_slots) };
        let packed_ctx = quote! { ::base_precompile_storage::LayoutCtx::packed(#packing::#loc_const.offset_bytes) };

        prev_slot_ref.map_or_else(
            || quote! {
                let #name = if <#ty as ::base_precompile_storage::StorableType>::IS_PACKABLE {
                    cached_slot = storage.load(#slot_addr)?;
                    let packed = ::base_precompile_storage::PackedSlot(cached_slot);
                    <#ty as ::base_precompile_storage::Storable>::load(&packed, ::alloy_primitives::U256::ZERO, #packed_ctx)?
                } else {
                    <#ty as ::base_precompile_storage::Storable>::load(storage, #slot_addr, ::base_precompile_storage::LayoutCtx::FULL)?
                };
            },
            |prev_slot_ref| quote! {
                let #name = {
                    let curr_offset = #packing::#loc_const.offset_slots;
                    let prev_offset = #prev_slot_ref;

                    if <#ty as ::base_precompile_storage::StorableType>::IS_PACKABLE && curr_offset == prev_offset {
                        let packed = ::base_precompile_storage::PackedSlot(cached_slot);
                        <#ty as ::base_precompile_storage::Storable>::load(&packed, ::alloy_primitives::U256::ZERO, #packed_ctx)?
                    } else if <#ty as ::base_precompile_storage::StorableType>::IS_PACKABLE {
                        cached_slot = storage.load(#slot_addr)?;
                        let packed = ::base_precompile_storage::PackedSlot(cached_slot);
                        <#ty as ::base_precompile_storage::Storable>::load(&packed, ::alloy_primitives::U256::ZERO, #packed_ctx)?
                    } else {
                        <#ty as ::base_precompile_storage::Storable>::load(storage, #slot_addr, ::base_precompile_storage::LayoutCtx::FULL)?
                    }
                };
            },
        )
    });

    quote! {
        let mut cached_slot = ::alloy_primitives::U256::ZERO;
        #(#field_loads)*
    }
}

fn gen_store_impl(fields: &[(&Ident, &Type)], packing: &Ident) -> TokenStream {
    if fields.is_empty() {
        return quote! {};
    }

    let field_stores = fields.iter().enumerate().map(|(idx, (name, ty))| {
        let loc_const = PackingConstants::new(name).location();
        let next_ty = fields.get(idx + 1).map(|(_, ty)| *ty);

        let (prev_slot_ref, next_slot_ref) =
            packing::get_neighbor_slot_refs(idx, fields, packing, |(name, _)| name, false);

        let slot_addr = quote! { base_slot + ::alloy_primitives::U256::from(#packing::#loc_const.offset_slots) };
        let packed_ctx = quote! { ::base_precompile_storage::LayoutCtx::packed(#packing::#loc_const.offset_bytes) };

        let should_store = match (&next_slot_ref, next_ty) {
            (Some(next_slot), Some(next_ty)) => {
                quote! {
                    #packing::#loc_const.offset_slots != #next_slot
                        || !<#next_ty as ::base_precompile_storage::StorableType>::IS_PACKABLE
                }
            }
            _ => quote! { true },
        };

        prev_slot_ref.map_or_else(
            || quote! {{
                if <#ty as ::base_precompile_storage::StorableType>::IS_PACKABLE {
                    // Always SLOAD first (Category 3: is_t4() optimization removed — correct but slightly less efficient)
                    pending_val = storage.load(#slot_addr)?;
                    pending_offset = Some(#packing::#loc_const.offset_slots);
                    let mut packed = ::base_precompile_storage::PackedSlot(pending_val);
                    <#ty as ::base_precompile_storage::Storable>::store(&self.#name, &mut packed, ::alloy_primitives::U256::ZERO, #packed_ctx)?;
                    pending_val = packed.0;

                    if #should_store {
                        storage.store(#slot_addr, pending_val)?;
                        pending_offset = None;
                    }
                } else {
                    <#ty as ::base_precompile_storage::Storable>::store(&self.#name, storage, #slot_addr, ::base_precompile_storage::LayoutCtx::FULL)?;
                }
            }},
            |prev_slot_ref| quote! {{
                let curr_offset = #packing::#loc_const.offset_slots;
                let prev_offset = #prev_slot_ref;

                if <#ty as ::base_precompile_storage::StorableType>::IS_PACKABLE && curr_offset == prev_offset {
                    let mut packed = ::base_precompile_storage::PackedSlot(pending_val);
                    <#ty as ::base_precompile_storage::Storable>::store(&self.#name, &mut packed, ::alloy_primitives::U256::ZERO, #packed_ctx)?;
                    pending_val = packed.0;
                } else if <#ty as ::base_precompile_storage::StorableType>::IS_PACKABLE {
                    if let Some(offset) = pending_offset {
                        storage.store(base_slot + ::alloy_primitives::U256::from(offset), pending_val)?;
                    }
                    // Always SLOAD first (Category 3: is_t4() optimization removed — correct but slightly less efficient)
                    pending_val = storage.load(#slot_addr)?;
                    pending_offset = Some(curr_offset);
                    let mut packed = ::base_precompile_storage::PackedSlot(pending_val);
                    <#ty as ::base_precompile_storage::Storable>::store(&self.#name, &mut packed, ::alloy_primitives::U256::ZERO, #packed_ctx)?;
                    pending_val = packed.0;
                } else {
                    if let Some(offset) = pending_offset {
                        storage.store(base_slot + ::alloy_primitives::U256::from(offset), pending_val)?;
                        pending_offset = None;
                    }
                    <#ty as ::base_precompile_storage::Storable>::store(&self.#name, storage, #slot_addr, ::base_precompile_storage::LayoutCtx::FULL)?;
                }

                if let Some(offset) = pending_offset && (#should_store) {
                    storage.store(base_slot + ::alloy_primitives::U256::from(offset), pending_val)?;
                    pending_offset = None;
                }
            }},
        )
    });

    quote! {
        let mut pending_val = ::alloy_primitives::U256::ZERO;
        let mut pending_offset: Option<usize> = None;
        #(#field_stores)*
    }
}

fn gen_delete_impl(fields: &[(&Ident, &Type)], packing: &Ident) -> TokenStream {
    let dynamic_deletes = fields.iter().map(|(name, ty)| {
        let loc_const = PackingConstants::new(name).location();
        quote! {
            if <#ty as ::base_precompile_storage::StorableType>::IS_DYNAMIC {
                <#ty as ::base_precompile_storage::Storable>::delete(
                    storage,
                    base_slot + ::alloy_primitives::U256::from(#packing::#loc_const.offset_slots),
                    ::base_precompile_storage::LayoutCtx::FULL
                )?;
            }
        }
    });

    let is_static_slot = fields.iter().map(|(name, ty)| {
        let loc_const = PackingConstants::new(name).location();
        quote! {
            ((#packing::#loc_const.offset_slots..#packing::#loc_const.offset_slots + <#ty as ::base_precompile_storage::StorableType>::SLOTS)
                .contains(&slot_offset) &&
             !<#ty as ::base_precompile_storage::StorableType>::IS_DYNAMIC)
        }
    });

    quote! {
        #(#dynamic_deletes)*

        for slot_offset in 0..#packing::SLOT_COUNT {
            if #(#is_static_slot)||* {
                storage.store(
                    base_slot + ::alloy_primitives::U256::from(slot_offset),
                    ::alloy_primitives::U256::ZERO
                )?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use syn::parse_quote;

    use super::*;

    fn parse_enum(input: DeriveInput) -> DataEnum {
        match input.data {
            Data::Enum(data_enum) => data_enum,
            _ => panic!("expected enum input"),
        }
    }

    #[test]
    fn validate_sequential_discriminants_accepts_implicit_variants() {
        let data_enum = parse_enum(parse_quote! {
            enum PackedStatus { Pending, Active, Frozen, }
        });
        validate_sequential_discriminants(&data_enum).unwrap();
    }

    #[test]
    fn validate_sequential_discriminants_rejects_explicit_discriminants() {
        let data_enum = parse_enum(parse_quote! {
            enum PackedStatus { Pending = 0, Active = 1, Frozen = 2, }
        });
        let err = validate_sequential_discriminants(&data_enum).unwrap_err();
        assert!(err.to_string().contains("explicit discriminants"));
    }
}
