//! Code generation for storage trait property tests.

use proc_macro2::TokenStream;
use quote::quote;

use crate::storable_primitives::{ALLOY_INT_SIZES, RUST_INT_SIZES};

const FIXED_BYTES_SIZES: &[usize] = &[
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
    27, 28, 29, 30, 31, 32,
];

pub(crate) fn gen_storable_tests() -> TokenStream {
    let rust_unsigned_arb = gen_rust_unsigned_arbitrary();
    let rust_signed_arb = gen_rust_signed_arbitrary();
    let alloy_unsigned_arb = gen_alloy_unsigned_arbitrary();
    let alloy_signed_arb = gen_alloy_signed_arbitrary();
    let fixed_bytes_arb = gen_fixed_bytes_arbitrary();

    let rust_unsigned_tests = gen_rust_unsigned_tests();
    let rust_signed_tests = gen_rust_signed_tests();
    let alloy_unsigned_tests = gen_alloy_unsigned_tests();
    let alloy_signed_tests = gen_alloy_signed_tests();
    let fixed_bytes_tests = gen_fixed_bytes_tests();

    quote! {
        #rust_unsigned_arb
        #rust_signed_arb
        #alloy_unsigned_arb
        #alloy_signed_arb
        #fixed_bytes_arb

        #rust_unsigned_tests
        #rust_signed_tests
        #alloy_unsigned_tests
        #alloy_signed_tests
        #fixed_bytes_tests
    }
}

fn gen_rust_unsigned_arbitrary() -> TokenStream {
    quote! {}
}
fn gen_rust_signed_arbitrary() -> TokenStream {
    quote! {}
}

fn gen_alloy_unsigned_arbitrary() -> TokenStream {
    let funcs: Vec<_> = ALLOY_INT_SIZES
        .iter()
        .map(|&size| {
            let type_name = quote::format_ident!("U{size}");
            let fn_name = quote::format_ident!("arb_u{size}_alloy");
            quote! {
                fn #fn_name() -> impl Strategy<Value = ::alloy_primitives::aliases::#type_name> {
                    Just(()).prop_perturb(|_, _| ::alloy_primitives::aliases::#type_name::random())
                }
            }
        })
        .collect();
    quote! { #(#funcs)* }
}

fn gen_alloy_signed_arbitrary() -> TokenStream {
    let funcs: Vec<_> = ALLOY_INT_SIZES.iter().flat_map(|&size| {
        let signed_type = quote::format_ident!("I{size}");
        let unsigned_type = quote::format_ident!("U{size}");
        let arb_any_fn = quote::format_ident!("arb_i{size}_alloy");
        let arb_pos_fn = quote::format_ident!("arb_positive_i{size}_alloy");
        let arb_neg_fn = quote::format_ident!("arb_negative_i{size}_alloy");
        let arb_unsigned_fn = quote::format_ident!("arb_u{size}_alloy");

        vec![
            quote! {
                fn #arb_any_fn() -> impl Strategy<Value = ::alloy_primitives::aliases::#signed_type> {
                    #arb_unsigned_fn().prop_map(|u| ::alloy_primitives::aliases::#signed_type::from_raw(u))
                }
            },
            quote! {
                fn #arb_pos_fn() -> impl Strategy<Value = ::alloy_primitives::aliases::#signed_type> {
                    #arb_unsigned_fn().prop_map(|u| {
                        ::alloy_primitives::aliases::#signed_type::from_raw(
                            u & (::alloy_primitives::aliases::#unsigned_type::MAX >> 1)
                        )
                    })
                }
            },
            quote! {
                fn #arb_neg_fn() -> impl Strategy<Value = ::alloy_primitives::aliases::#signed_type> {
                    #arb_pos_fn().prop_map(|i| -i)
                }
            },
        ]
    }).collect();
    quote! { #(#funcs)* }
}

fn gen_fixed_bytes_arbitrary() -> TokenStream {
    let funcs: Vec<_> = FIXED_BYTES_SIZES
        .iter()
        .map(|&size| {
            let fn_name = quote::format_ident!("arb_fixed_bytes_{size}");
            quote! {
                fn #fn_name() -> impl Strategy<Value = ::alloy_primitives::FixedBytes<#size>> {
                    Just(()).prop_perturb(|_, _| ::alloy_primitives::FixedBytes::<#size>::random())
                }
            }
        })
        .collect();
    quote! { #(#funcs)* }
}

fn gen_rust_unsigned_tests() -> TokenStream {
    let tests: Vec<_> = RUST_INT_SIZES.iter().map(|&size| {
        let type_name = quote::format_ident!("u{size}");
        let test_name = quote::format_ident!("test_u{size}_storage_roundtrip");
        let label = format!("u{size}");
        quote! {
            #[test]
            fn #test_name(value in any::<#type_name>(), base_slot in arb_safe_slot()) {
                let (mut storage, address) = setup_storage();
                ::base_precompile_storage::StorageCtx::enter(&mut storage, || {
                    let mut slot = ::base_precompile_storage::Slot::<#type_name>::new(base_slot, address);
                    slot.write(value).unwrap();
                    let loaded = slot.read().unwrap();
                    assert_eq!(value, loaded, concat!(#label, " roundtrip failed"));
                    slot.delete().unwrap();
                    let after_delete = slot.read().unwrap();
                    assert_eq!(after_delete, 0, concat!(#label, " not zero after delete"));
                    let word = value.to_word();
                    let recovered = #type_name::from_word(word).unwrap();
                    assert_eq!(value, recovered, concat!(#label, " EVM word roundtrip failed"));
                });
            }
        }
    }).collect();
    quote! {
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(500))]
            #(#tests)*
        }
    }
}

fn gen_rust_signed_tests() -> TokenStream {
    let tests: Vec<_> = RUST_INT_SIZES.iter().flat_map(|&size| {
        let type_name = quote::format_ident!("i{size}");
        let pos_test_name = quote::format_ident!("test_i{size}_positive_storage_roundtrip");
        let neg_test_name = quote::format_ident!("test_i{size}_negative_storage_roundtrip");
        let label = format!("i{size}");
        vec![
            quote! {
                #[test]
                fn #pos_test_name(value in 0 as #type_name..=#type_name::MAX, base_slot in arb_safe_slot()) {
                    let (mut storage, address) = setup_storage();
                    ::base_precompile_storage::StorageCtx::enter(&mut storage, || {
                        let mut slot = ::base_precompile_storage::Slot::<#type_name>::new(base_slot, address);
                        slot.write(value).unwrap();
                        let loaded = slot.read().unwrap();
                        assert_eq!(value, loaded, concat!(#label, " positive roundtrip failed"));
                        slot.delete().unwrap();
                        let after_delete = slot.read().unwrap();
                        assert_eq!(after_delete, 0, concat!(#label, " not zero after delete"));
                        let word = value.to_word();
                        let recovered = #type_name::from_word(word).unwrap();
                        assert_eq!(value, recovered, concat!(#label, " positive EVM word roundtrip failed"));
                    });
                }
            },
            quote! {
                #[test]
                fn #neg_test_name(value in #type_name::MIN..0 as #type_name, base_slot in arb_safe_slot()) {
                    let (mut storage, address) = setup_storage();
                    ::base_precompile_storage::StorageCtx::enter(&mut storage, || {
                        let mut slot = ::base_precompile_storage::Slot::<#type_name>::new(base_slot, address);
                        slot.write(value).unwrap();
                        let loaded = slot.read().unwrap();
                        assert_eq!(value, loaded, concat!(#label, " negative roundtrip failed"));
                        slot.delete().unwrap();
                        let after_delete = slot.read().unwrap();
                        assert_eq!(after_delete, 0, concat!(#label, " not zero after delete"));
                        let word = value.to_word();
                        let recovered = #type_name::from_word(word).unwrap();
                        assert_eq!(value, recovered, concat!(#label, " negative EVM word roundtrip failed"));
                    });
                }
            },
        ]
    }).collect();
    quote! {
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(500))]
            #(#tests)*
        }
    }
}

fn gen_alloy_unsigned_tests() -> TokenStream {
    let tests: Vec<_> = ALLOY_INT_SIZES.iter().map(|&size| {
        let type_name = quote::format_ident!("U{size}");
        let test_name = quote::format_ident!("test_u{size}_alloy_storage_roundtrip");
        let arb_fn = quote::format_ident!("arb_u{size}_alloy");
        let label = format!("U{size}");
        quote! {
            #[test]
            fn #test_name(value in #arb_fn(), base_slot in arb_safe_slot()) {
                let (mut storage, address) = setup_storage();
                ::base_precompile_storage::StorageCtx::enter(&mut storage, || {
                    let mut slot = ::base_precompile_storage::Slot::<::alloy_primitives::aliases::#type_name>::new(base_slot, address);
                    slot.write(value).unwrap();
                    let loaded = slot.read().unwrap();
                    assert_eq!(value, loaded, concat!(#label, " roundtrip failed"));
                    slot.delete().unwrap();
                    let after_delete = slot.read().unwrap();
                    assert_eq!(
                        after_delete,
                        ::alloy_primitives::aliases::#type_name::ZERO,
                        concat!(#label, " not zero after delete")
                    );
                    let word = value.to_word();
                    let recovered = ::alloy_primitives::aliases::#type_name::from_word(word).unwrap();
                    assert_eq!(value, recovered, concat!(#label, " EVM word roundtrip failed"));
                });
            }
        }
    }).collect();
    quote! {
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(500))]
            #(#tests)*
        }
    }
}

fn gen_alloy_signed_tests() -> TokenStream {
    let tests: Vec<_> = ALLOY_INT_SIZES.iter().flat_map(|&size| {
        let type_name = quote::format_ident!("I{size}");
        let pos_test_name = quote::format_ident!("test_i{size}_alloy_positive_storage_roundtrip");
        let neg_test_name = quote::format_ident!("test_i{size}_alloy_negative_storage_roundtrip");
        let arb_pos_fn = quote::format_ident!("arb_positive_i{size}_alloy");
        let arb_neg_fn = quote::format_ident!("arb_negative_i{size}_alloy");
        let label = format!("I{size}");
        vec![
            quote! {
                #[test]
                fn #pos_test_name(value in #arb_pos_fn(), base_slot in arb_safe_slot()) {
                    let (mut storage, address) = setup_storage();
                    ::base_precompile_storage::StorageCtx::enter(&mut storage, || {
                        let mut slot = ::base_precompile_storage::Slot::<::alloy_primitives::aliases::#type_name>::new(base_slot, address);
                        slot.write(value).unwrap();
                        let loaded = slot.read().unwrap();
                        assert_eq!(value, loaded, concat!(#label, " positive roundtrip failed"));
                        slot.delete().unwrap();
                        let after_delete = slot.read().unwrap();
                        assert_eq!(
                            after_delete,
                            ::alloy_primitives::aliases::#type_name::ZERO,
                            concat!(#label, " not zero after delete")
                        );
                        let word = value.to_word();
                        let recovered = ::alloy_primitives::aliases::#type_name::from_word(word).unwrap();
                        assert_eq!(value, recovered, concat!(#label, " positive EVM word roundtrip failed"));
                    });
                }
            },
            quote! {
                #[test]
                fn #neg_test_name(value in #arb_neg_fn(), base_slot in arb_safe_slot()) {
                    let (mut storage, address) = setup_storage();
                    ::base_precompile_storage::StorageCtx::enter(&mut storage, || {
                        let mut slot = ::base_precompile_storage::Slot::<::alloy_primitives::aliases::#type_name>::new(base_slot, address);
                        slot.write(value).unwrap();
                        let loaded = slot.read().unwrap();
                        assert_eq!(value, loaded, concat!(#label, " negative roundtrip failed"));
                        slot.delete().unwrap();
                        let after_delete = slot.read().unwrap();
                        assert_eq!(
                            after_delete,
                            ::alloy_primitives::aliases::#type_name::ZERO,
                            concat!(#label, " not zero after delete")
                        );
                        let word = value.to_word();
                        let recovered = ::alloy_primitives::aliases::#type_name::from_word(word).unwrap();
                        assert_eq!(value, recovered, concat!(#label, " negative EVM word roundtrip failed"));
                    });
                }
            },
        ]
    }).collect();
    quote! {
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(500))]
            #(#tests)*
        }
    }
}

fn gen_fixed_bytes_tests() -> TokenStream {
    let tests: Vec<_> = FIXED_BYTES_SIZES.iter().map(|&size| {
        let test_name = quote::format_ident!("test_fixed_bytes_{size}_storage_roundtrip");
        let arb_fn = quote::format_ident!("arb_fixed_bytes_{size}");
        quote! {
            #[test]
            fn #test_name(value in #arb_fn(), base_slot in arb_safe_slot()) {
                let (mut storage, address) = setup_storage();
                ::base_precompile_storage::StorageCtx::enter(&mut storage, || {
                    let mut slot = ::base_precompile_storage::Slot::<::alloy_primitives::FixedBytes<#size>>::new(base_slot, address);
                    slot.write(value).unwrap();
                    let loaded = slot.read().unwrap();
                    assert_eq!(
                        value, loaded,
                        concat!("FixedBytes<", stringify!(#size), "> roundtrip failed")
                    );
                    slot.delete().unwrap();
                    let after_delete = slot.read().unwrap();
                    assert_eq!(
                        after_delete,
                        ::alloy_primitives::FixedBytes::<#size>::ZERO,
                        concat!("FixedBytes<", stringify!(#size), "> not zero after delete")
                    );
                    let word = value.to_word();
                    let recovered = ::alloy_primitives::FixedBytes::<#size>::from_word(word).unwrap();
                    assert_eq!(
                        value, recovered,
                        concat!("FixedBytes<", stringify!(#size), "> EVM word roundtrip failed")
                    );
                });
            }
        }
    }).collect();
    quote! {
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(500))]
            #(#tests)*
        }
    }
}
