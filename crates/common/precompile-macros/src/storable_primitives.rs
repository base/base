//! Code generation for primitive type storage implementations.

use proc_macro2::TokenStream;
use quote::quote;

pub(crate) const RUST_INT_SIZES: &[usize] = &[8, 16, 32, 64, 128];
pub(crate) const ALLOY_INT_SIZES: &[usize] = &[8, 16, 32, 64, 96, 128, 256];

// -- CONFIGURATION TYPES ------------------------------------------------------

#[derive(Debug, Clone)]
enum StorableConversionStrategy {
    UnsignedRust,
    UnsignedAlloy(proc_macro2::Ident),
    SignedRust(proc_macro2::Ident),
    SignedAlloy(proc_macro2::Ident),
    FixedBytes(usize),
}

#[derive(Debug, Clone)]
enum StorageKeyStrategy {
    Simple,
    WithSize(usize),
    SignedRaw(usize),
    AsSlice,
}

#[derive(Debug, Clone)]
struct TypeConfig {
    type_path: TokenStream,
    byte_count: usize,
    storable_strategy: StorableConversionStrategy,
    storage_key_strategy: StorageKeyStrategy,
}

// -- IMPLEMENTATION GENERATORS ------------------------------------------------

fn gen_storable_layout_impl(type_path: &TokenStream, byte_count: usize) -> TokenStream {
    quote! {
        impl ::base_precompile_storage::StorableType for #type_path {
            const LAYOUT: ::base_precompile_storage::Layout = ::base_precompile_storage::Layout::Bytes(#byte_count);
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
    }
}

fn gen_storage_key_impl(type_path: &TokenStream, strategy: &StorageKeyStrategy) -> TokenStream {
    let conversion = match strategy {
        StorageKeyStrategy::Simple => quote! { self.to_be_bytes() },
        StorageKeyStrategy::WithSize(size) => quote! { self.to_be_bytes::<#size>() },
        StorageKeyStrategy::SignedRaw(size) => quote! { self.into_raw().to_be_bytes::<#size>() },
        StorageKeyStrategy::AsSlice => quote! { self.as_slice() },
    };

    quote! {
        impl ::base_precompile_storage::StorageKey for #type_path {
            #[inline]
            fn as_storage_bytes(&self) -> impl AsRef<[u8]> {
                #conversion
            }
        }
    }
}

fn gen_to_word_impl(type_path: &TokenStream, strategy: &StorableConversionStrategy) -> TokenStream {
    match strategy {
        StorableConversionStrategy::UnsignedRust => quote! {
            impl ::base_precompile_storage::FromWord for #type_path {
                #[inline]
                fn to_word(&self) -> ::alloy_primitives::U256 {
                    ::alloy_primitives::U256::from(*self)
                }
                #[inline]
                fn from_word(word: ::alloy_primitives::U256) -> ::base_precompile_storage::Result<Self> {
                    word.try_into().map_err(|_| ::base_precompile_storage::BasePrecompileError::under_overflow())
                }
            }
        },
        StorableConversionStrategy::UnsignedAlloy(ty) => quote! {
            impl ::base_precompile_storage::FromWord for #type_path {
                #[inline]
                fn to_word(&self) -> ::alloy_primitives::U256 {
                    ::alloy_primitives::U256::from(*self)
                }
                #[inline]
                fn from_word(word: ::alloy_primitives::U256) -> ::base_precompile_storage::Result<Self> {
                    if word > ::alloy_primitives::U256::from(::alloy_primitives::aliases::#ty::MAX) {
                        return Err(::base_precompile_storage::BasePrecompileError::under_overflow());
                    }
                    Ok(word.to::<Self>())
                }
            }
        },
        StorableConversionStrategy::SignedRust(unsigned_type) => quote! {
            impl ::base_precompile_storage::FromWord for #type_path {
                #[inline]
                fn to_word(&self) -> ::alloy_primitives::U256 {
                    ::alloy_primitives::U256::from(*self as #unsigned_type)
                }
                #[inline]
                fn from_word(word: ::alloy_primitives::U256) -> ::base_precompile_storage::Result<Self> {
                    let unsigned: #unsigned_type = word.try_into()
                        .map_err(|_| ::base_precompile_storage::BasePrecompileError::under_overflow())?;
                    Ok(unsigned as Self)
                }
            }
        },
        StorableConversionStrategy::SignedAlloy(unsigned_type) => quote! {
            impl ::base_precompile_storage::FromWord for #type_path {
                #[inline]
                fn to_word(&self) -> ::alloy_primitives::U256 {
                    ::alloy_primitives::U256::from(self.into_raw())
                }
                #[inline]
                fn from_word(word: ::alloy_primitives::U256) -> ::base_precompile_storage::Result<Self> {
                    if word > ::alloy_primitives::U256::from(::alloy_primitives::aliases::#unsigned_type::MAX) {
                        return Err(::base_precompile_storage::BasePrecompileError::under_overflow());
                    }
                    let unsigned_val = word.to::<::alloy_primitives::aliases::#unsigned_type>();
                    Ok(Self::from_raw(unsigned_val))
                }
            }
        },
        StorableConversionStrategy::FixedBytes(size) => quote! {
            impl ::base_precompile_storage::FromWord for #type_path {
                #[inline]
                fn to_word(&self) -> ::alloy_primitives::U256 {
                    let mut bytes = [0u8; 32];
                    bytes[32 - #size..].copy_from_slice(&self[..]);
                    ::alloy_primitives::U256::from_be_bytes(bytes)
                }
                #[inline]
                fn from_word(word: ::alloy_primitives::U256) -> ::base_precompile_storage::Result<Self> {
                    let bytes = word.to_be_bytes::<32>();
                    let mut fixed_bytes = [0u8; #size];
                    fixed_bytes.copy_from_slice(&bytes[32 - #size..]);
                    Ok(Self::from(fixed_bytes))
                }
            }
        },
    }
}

fn gen_complete_impl_set(config: &TypeConfig) -> TokenStream {
    let type_path = &config.type_path;
    let storable_type_impl = gen_storable_layout_impl(type_path, config.byte_count);
    let storage_key_impl = gen_storage_key_impl(type_path, &config.storage_key_strategy);
    let to_word_impl = gen_to_word_impl(type_path, &config.storable_strategy);

    let full_word_storable_impl = if config.byte_count < 32 {
        quote! {
            impl ::base_precompile_storage::sealed::OnlyPrimitives for #type_path {}
            impl ::base_precompile_storage::Packable for #type_path {}
        }
    } else {
        quote! {
            impl ::base_precompile_storage::sealed::OnlyPrimitives for #type_path {}
            impl ::base_precompile_storage::Storable for #type_path {
                #[inline]
                fn load<S: ::base_precompile_storage::StorageOps>(
                    storage: &S,
                    slot: ::alloy_primitives::U256,
                    _ctx: ::base_precompile_storage::LayoutCtx
                ) -> ::base_precompile_storage::Result<Self> {
                    storage.load(slot).and_then(<Self as ::base_precompile_storage::FromWord>::from_word)
                }
                #[inline]
                fn store<S: ::base_precompile_storage::StorageOps>(
                    &self,
                    storage: &mut S,
                    slot: ::alloy_primitives::U256,
                    _ctx: ::base_precompile_storage::LayoutCtx
                ) -> ::base_precompile_storage::Result<()> {
                    storage.store(slot, <Self as ::base_precompile_storage::FromWord>::to_word(self))
                }
            }
        }
    };

    quote! {
        #storable_type_impl
        #to_word_impl
        #storage_key_impl
        #full_word_storable_impl
    }
}

pub(crate) fn gen_storable_rust_ints() -> TokenStream {
    let mut impls = Vec::with_capacity(RUST_INT_SIZES.len() * 2);

    for size in RUST_INT_SIZES {
        let unsigned_type = quote::format_ident!("u{}", size);
        let signed_type = quote::format_ident!("i{}", size);
        let byte_count = size / 8;

        let unsigned_config = TypeConfig {
            type_path: quote! { #unsigned_type },
            byte_count,
            storable_strategy: StorableConversionStrategy::UnsignedRust,
            storage_key_strategy: StorageKeyStrategy::Simple,
        };
        impls.push(gen_complete_impl_set(&unsigned_config));

        let signed_config = TypeConfig {
            type_path: quote! { #signed_type },
            byte_count,
            storable_strategy: StorableConversionStrategy::SignedRust(unsigned_type.clone()),
            storage_key_strategy: StorageKeyStrategy::Simple,
        };
        impls.push(gen_complete_impl_set(&signed_config));
    }

    quote! { #(#impls)* }
}

fn gen_alloy_integers() -> Vec<TokenStream> {
    let mut impls = Vec::with_capacity(ALLOY_INT_SIZES.len() * 2);

    for &size in ALLOY_INT_SIZES {
        let unsigned_type = quote::format_ident!("U{}", size);
        let signed_type = quote::format_ident!("I{}", size);
        let byte_count = size / 8;

        let unsigned_config = TypeConfig {
            type_path: quote! { ::alloy_primitives::aliases::#unsigned_type },
            byte_count,
            storable_strategy: StorableConversionStrategy::UnsignedAlloy(unsigned_type.clone()),
            storage_key_strategy: StorageKeyStrategy::WithSize(byte_count),
        };
        impls.push(gen_complete_impl_set(&unsigned_config));

        let signed_config = TypeConfig {
            type_path: quote! { ::alloy_primitives::aliases::#signed_type },
            byte_count,
            storable_strategy: StorableConversionStrategy::SignedAlloy(unsigned_type.clone()),
            storage_key_strategy: StorageKeyStrategy::SignedRaw(byte_count),
        };
        impls.push(gen_complete_impl_set(&signed_config));
    }

    impls
}

fn gen_fixed_bytes(sizes: &[usize]) -> Vec<TokenStream> {
    sizes
        .iter()
        .map(|&size| {
            let config = TypeConfig {
                type_path: quote! { ::alloy_primitives::FixedBytes<#size> },
                byte_count: size,
                storable_strategy: StorableConversionStrategy::FixedBytes(size),
                storage_key_strategy: StorageKeyStrategy::AsSlice,
            };
            gen_complete_impl_set(&config)
        })
        .collect()
}

pub(crate) fn gen_storable_alloy_bytes() -> TokenStream {
    let sizes: Vec<usize> = (1..=32).collect();
    let impls = gen_fixed_bytes(&sizes);
    quote! { #(#impls)* }
}

pub(crate) fn gen_storable_alloy_ints() -> TokenStream {
    let impls = gen_alloy_integers();
    quote! { #(#impls)* }
}

// -- ARRAY IMPLEMENTATIONS ----------------------------------------------------

#[derive(Debug, Clone)]
struct ArrayConfig {
    elem_type: TokenStream,
    array_size: usize,
    elem_byte_count: usize,
    elem_is_packable: bool,
}

const fn is_packable(byte_count: usize) -> bool {
    byte_count < 32
}

fn gen_array_impl(config: &ArrayConfig) -> TokenStream {
    let ArrayConfig { elem_type, array_size, elem_byte_count, elem_is_packable } = config;

    let slot_count_expr = if *elem_is_packable {
        quote! { ::base_precompile_storage::calc_packed_slot_count(#array_size, #elem_byte_count) }
    } else {
        quote! { #array_size }
    };

    let load_impl = if *elem_is_packable {
        gen_packed_array_load(array_size, elem_byte_count)
    } else {
        gen_unpacked_array_load(array_size)
    };

    let store_impl = if *elem_is_packable {
        gen_packed_array_store(array_size, elem_byte_count)
    } else {
        gen_unpacked_array_store()
    };

    quote! {
        impl ::base_precompile_storage::StorableType for [#elem_type; #array_size] {
            const LAYOUT: ::base_precompile_storage::Layout = ::base_precompile_storage::Layout::Slots(#slot_count_expr);
            type Handler<'a> = ::base_precompile_storage::ArrayHandler<'a, #elem_type, #array_size>;

            fn handle<'a>(
                slot: ::alloy_primitives::U256,
                ctx: ::base_precompile_storage::LayoutCtx,
                address: ::alloy_primitives::Address,
                storage: ::base_precompile_storage::StorageCtx<'a>,
            ) -> Self::Handler<'a> {
                debug_assert_eq!(ctx, ::base_precompile_storage::LayoutCtx::FULL, "Arrays cannot be packed");
                Self::Handler::new(slot, address, storage)
            }
        }

        impl ::base_precompile_storage::Storable for [#elem_type; #array_size] {
            #[inline]
            fn load<S: ::base_precompile_storage::StorageOps>(storage: &S, slot: ::alloy_primitives::U256, ctx: ::base_precompile_storage::LayoutCtx) -> ::base_precompile_storage::Result<Self> {
                debug_assert_eq!(ctx, ::base_precompile_storage::LayoutCtx::FULL, "Arrays can only be loaded with LayoutCtx::FULL");
                use ::base_precompile_storage::{calc_element_slot, calc_element_offset, extract_from_word};
                let base_slot = slot;
                #load_impl
            }

            #[inline]
            fn store<S: ::base_precompile_storage::StorageOps>(&self, storage: &mut S, slot: ::alloy_primitives::U256, ctx: ::base_precompile_storage::LayoutCtx) -> ::base_precompile_storage::Result<()> {
                debug_assert_eq!(ctx, ::base_precompile_storage::LayoutCtx::FULL, "Arrays can only be stored with LayoutCtx::FULL");
                use ::base_precompile_storage::{calc_element_slot, calc_element_offset, insert_into_word};
                let base_slot = slot;
                #store_impl
            }
        }
    }
}

fn gen_packed_array_load(array_size: &usize, elem_byte_count: &usize) -> TokenStream {
    quote! {
        let mut result = [Default::default(); #array_size];
        for i in 0..#array_size {
            let slot_idx = calc_element_slot(i, #elem_byte_count);
            let offset = calc_element_offset(i, #elem_byte_count);
            let slot_addr = base_slot + ::alloy_primitives::U256::from(slot_idx);
            let slot_value = storage.load(slot_addr)?;
            result[i] = extract_from_word(slot_value, offset, #elem_byte_count)?;
        }
        Ok(result)
    }
}

fn gen_packed_array_store(array_size: &usize, elem_byte_count: &usize) -> TokenStream {
    quote! {
        let slot_count = ::base_precompile_storage::calc_packed_slot_count(#array_size, #elem_byte_count);
        for slot_idx in 0..slot_count {
            let slot_addr = base_slot + ::alloy_primitives::U256::from(slot_idx);
            let mut slot_value = ::alloy_primitives::U256::ZERO;
            for i in 0..#array_size {
                let elem_slot = calc_element_slot(i, #elem_byte_count);
                if elem_slot == slot_idx {
                    let offset = calc_element_offset(i, #elem_byte_count);
                    slot_value = insert_into_word(slot_value, &self[i], offset, #elem_byte_count)?;
                }
            }
            storage.store(slot_addr, slot_value)?;
        }
        Ok(())
    }
}

fn gen_unpacked_array_load(array_size: &usize) -> TokenStream {
    quote! {
        let mut result = [Default::default(); #array_size];
        for i in 0..#array_size {
            let elem_slot = base_slot + ::alloy_primitives::U256::from(i);
            result[i] = ::base_precompile_storage::Storable::load(storage, elem_slot, ::base_precompile_storage::LayoutCtx::FULL)?;
        }
        Ok(result)
    }
}

fn gen_unpacked_array_store() -> TokenStream {
    quote! {
        for (i, elem) in self.iter().enumerate() {
            let elem_slot = base_slot + ::alloy_primitives::U256::from(i);
            ::base_precompile_storage::Storable::store(elem, storage, elem_slot, ::base_precompile_storage::LayoutCtx::FULL)?;
        }
        Ok(())
    }
}

fn gen_arrays_for_type(
    elem_type: TokenStream,
    elem_byte_count: usize,
    sizes: &[usize],
) -> Vec<TokenStream> {
    let elem_is_packable = is_packable(elem_byte_count);
    sizes
        .iter()
        .map(|&size| {
            let config = ArrayConfig {
                elem_type: elem_type.clone(),
                array_size: size,
                elem_byte_count,
                elem_is_packable,
            };
            gen_array_impl(&config)
        })
        .collect()
}

pub(crate) fn gen_storable_arrays() -> TokenStream {
    let mut all_impls = Vec::new();
    let sizes: Vec<usize> = (1..=32).collect();

    for &bit_size in RUST_INT_SIZES {
        let type_ident = quote::format_ident!("u{}", bit_size);
        all_impls.extend(gen_arrays_for_type(quote! { #type_ident }, bit_size / 8, &sizes));
    }
    for &bit_size in RUST_INT_SIZES {
        let type_ident = quote::format_ident!("i{}", bit_size);
        all_impls.extend(gen_arrays_for_type(quote! { #type_ident }, bit_size / 8, &sizes));
    }
    for &bit_size in ALLOY_INT_SIZES {
        let type_ident = quote::format_ident!("U{}", bit_size);
        all_impls.extend(gen_arrays_for_type(
            quote! { ::alloy_primitives::aliases::#type_ident },
            bit_size / 8,
            &sizes,
        ));
    }
    for &bit_size in ALLOY_INT_SIZES {
        let type_ident = quote::format_ident!("I{}", bit_size);
        all_impls.extend(gen_arrays_for_type(
            quote! { ::alloy_primitives::aliases::#type_ident },
            bit_size / 8,
            &sizes,
        ));
    }
    all_impls.extend(gen_arrays_for_type(quote! { ::alloy_primitives::Address }, 20, &sizes));
    for &byte_size in &[20usize, 32] {
        all_impls.extend(gen_arrays_for_type(
            quote! { ::alloy_primitives::FixedBytes<#byte_size> },
            byte_size,
            &sizes,
        ));
    }

    quote! { #(#all_impls)* }
}

pub(crate) fn gen_nested_arrays() -> TokenStream {
    let mut all_impls = Vec::new();

    for inner in &[2usize, 4, 8, 16] {
        let inner_slots = inner.div_ceil(32);
        let max_outer = 32 / inner_slots.max(1);
        for outer in 1..=max_outer.min(32) {
            all_impls.extend(gen_arrays_for_type(
                quote! { [u8; #inner] },
                inner_slots * 32,
                &[outer],
            ));
        }
    }
    for inner in &[2usize, 4, 8] {
        let inner_slots = (inner * 2).div_ceil(32);
        let max_outer = 32 / inner_slots.max(1);
        for outer in 1..=max_outer.min(16) {
            all_impls.extend(gen_arrays_for_type(
                quote! { [u16; #inner] },
                inner_slots * 32,
                &[outer],
            ));
        }
    }

    quote! { #(#all_impls)* }
}

// -- STRUCT ARRAY IMPLEMENTATIONS ---------------------------------------------

pub(crate) fn gen_struct_arrays(struct_type: TokenStream, array_sizes: &[usize]) -> TokenStream {
    let impls: Vec<_> =
        array_sizes.iter().map(|&size| gen_struct_array_impl(&struct_type, size)).collect();
    quote! { #(#impls)* }
}

fn gen_struct_array_impl(struct_type: &TokenStream, array_size: usize) -> TokenStream {
    let struct_type_str =
        struct_type.to_string().replace("::", "_").replace(['<', '>', ' ', '[', ']', ';'], "_");
    let mod_ident = quote::format_ident!("__array_{}_{}", struct_type_str, array_size);

    let load_impl = gen_struct_array_load(struct_type, array_size);
    let store_impl = gen_struct_array_store(struct_type);

    quote! {
        mod #mod_ident {
            use super::*;
            pub const ELEM_SLOTS: usize = <#struct_type as ::base_precompile_storage::StorableType>::SLOTS;
            pub const ARRAY_LEN: usize = #array_size;
            pub const SLOT_COUNT: usize = ARRAY_LEN * ELEM_SLOTS;
        }

        impl ::base_precompile_storage::StorableType for [#struct_type; #array_size] {
            const LAYOUT: ::base_precompile_storage::Layout = ::base_precompile_storage::Layout::Slots(#mod_ident::SLOT_COUNT);
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

        impl ::base_precompile_storage::Storable for [#struct_type; #array_size] {
            #[inline]
            fn load<S: ::base_precompile_storage::StorageOps>(storage: &S, slot: ::alloy_primitives::U256, ctx: ::base_precompile_storage::LayoutCtx) -> ::base_precompile_storage::Result<Self> {
                debug_assert_eq!(ctx, ::base_precompile_storage::LayoutCtx::FULL, "Struct arrays can only be loaded with LayoutCtx::FULL");
                let base_slot = slot;
                #load_impl
            }

            #[inline]
            fn store<S: ::base_precompile_storage::StorageOps>(&self, storage: &mut S, slot: ::alloy_primitives::U256, ctx: ::base_precompile_storage::LayoutCtx) -> ::base_precompile_storage::Result<()> {
                debug_assert_eq!(ctx, ::base_precompile_storage::LayoutCtx::FULL, "Struct arrays can only be stored with LayoutCtx::FULL");
                let base_slot = slot;
                #store_impl
            }
        }
    }
}

fn gen_struct_array_load(struct_type: &TokenStream, array_size: usize) -> TokenStream {
    quote! {
        let mut result = [Default::default(); #array_size];
        for i in 0..#array_size {
            let elem_slot = base_slot.checked_add(
                ::alloy_primitives::U256::from(i).checked_mul(
                    ::alloy_primitives::U256::from(<#struct_type as ::base_precompile_storage::StorableType>::SLOTS)
                ).ok_or(::base_precompile_storage::BasePrecompileError::SlotOverflow)?
            ).ok_or(::base_precompile_storage::BasePrecompileError::SlotOverflow)?;
            result[i] = <#struct_type as ::base_precompile_storage::Storable>::load(storage, elem_slot, ::base_precompile_storage::LayoutCtx::FULL)?;
        }
        Ok(result)
    }
}

fn gen_struct_array_store(struct_type: &TokenStream) -> TokenStream {
    quote! {
        for (i, elem) in self.iter().enumerate() {
            let elem_slot = base_slot.checked_add(
                ::alloy_primitives::U256::from(i).checked_mul(
                    ::alloy_primitives::U256::from(<#struct_type as ::base_precompile_storage::StorableType>::SLOTS)
                ).ok_or(::base_precompile_storage::BasePrecompileError::SlotOverflow)?
            ).ok_or(::base_precompile_storage::BasePrecompileError::SlotOverflow)?;
            <#struct_type as ::base_precompile_storage::Storable>::store(elem, storage, elem_slot, ::base_precompile_storage::LayoutCtx::FULL)?;
        }
        Ok(())
    }
}
