use std::num::NonZeroUsize;

use alloy_primitives::{Address, B256, Bloom, Bytes, U128, U256};
use ssz::{Decode, DecodeError, Encode};
use ssz_derive::{Decode, Encode};

mod round_trip {
    use std::{
        collections::{BTreeMap, BTreeSet},
        iter::FromIterator,
        sync::Arc,
    };

    use super::*;

    fn round_trip<T: Encode + Decode + std::fmt::Debug + PartialEq>(items: Vec<T>) {
        assert_eq!(<T as Encode>::is_ssz_fixed_len(), <T as Decode>::is_ssz_fixed_len());
        assert_eq!(<T as Encode>::ssz_fixed_len(), <T as Decode>::ssz_fixed_len());

        for item in items {
            let encoded = &item.as_ssz_bytes();
            assert_eq!(item.ssz_bytes_len(), encoded.len());
            assert_eq!(T::from_ssz_bytes(encoded), Ok(item));
        }
    }

    #[test]
    fn bool() {
        let items: Vec<bool> = vec![true, false];

        round_trip(items);
    }

    #[test]
    fn option_u16() {
        let items: Vec<Option<u16>> = vec![None, Some(2u16)];

        round_trip(items);
    }

    #[test]
    fn u8_array_4() {
        let items: Vec<[u8; 4]> = vec![[0, 0, 0, 0], [1, 0, 0, 0], [1, 2, 3, 4], [1, 2, 0, 4]];

        round_trip(items);
    }

    #[test]
    fn address() {
        let items: Vec<Address> =
            vec![Address::repeat_byte(0), Address::from([1; 20]), Address::random()];

        round_trip(items);
    }

    #[test]
    fn vec_of_address() {
        let items: Vec<Vec<Address>> =
            vec![vec![], vec![Address::repeat_byte(0), Address::from([1; 20]), Address::random()]];

        round_trip(items);
    }

    #[test]
    fn h256() {
        let items: Vec<B256> = vec![B256::repeat_byte(0), B256::from([1; 32]), B256::random()];

        round_trip(items);
    }

    #[test]
    fn vec_of_b256() {
        let items: Vec<Vec<B256>> =
            vec![vec![], vec![B256::ZERO, B256::from([1; 32]), B256::random()]];

        round_trip(items);
    }

    #[test]
    fn option_vec_b256() {
        let items: Vec<Option<Vec<B256>>> =
            vec![None, Some(vec![]), Some(vec![B256::ZERO, B256::from([1; 32]), B256::random()])];

        round_trip(items);
    }

    #[test]
    fn vec_u16() {
        let items: Vec<Vec<u16>> =
            vec![vec![], vec![255], vec![0, 1, 2], vec![100; 64], vec![255, 0, 255]];

        round_trip(items);
    }

    #[test]
    fn vec_of_vec_u16() {
        let items: Vec<Vec<Vec<u16>>> = vec![
            vec![],
            vec![vec![]],
            vec![vec![1, 2, 3]],
            vec![vec![], vec![]],
            vec![vec![], vec![1, 2, 3]],
            vec![vec![1, 2, 3], vec![1, 2, 3]],
            vec![vec![1, 2, 3], vec![], vec![1, 2, 3]],
            vec![vec![], vec![], vec![1, 2, 3]],
            vec![vec![], vec![1], vec![1, 2, 3]],
            vec![vec![], vec![1], vec![1, 2, 3]],
        ];

        round_trip(items);
    }

    #[derive(Debug, PartialEq, Encode, Decode)]
    struct FixedLen {
        a: u16,
        b: u64,
        c: u32,
    }

    #[test]
    #[allow(clippy::zero_prefixed_literal)]
    fn fixed_len_struct_encoding() {
        let items: Vec<FixedLen> = vec![
            FixedLen { a: 0, b: 0, c: 0 },
            FixedLen { a: 1, b: 1, c: 1 },
            FixedLen { a: 1, b: 0, c: 1 },
        ];

        let expected_encodings = [
            //  | u16--| u64----------------------------| u32----------|
            vec![00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00],
            vec![01, 00, 01, 00, 00, 00, 00, 00, 00, 00, 01, 00, 00, 00],
            vec![01, 00, 00, 00, 00, 00, 00, 00, 00, 00, 01, 00, 00, 00],
        ];

        for i in 0..items.len() {
            assert_eq!(items[i].as_ssz_bytes(), expected_encodings[i], "Failed on {}", i);
        }
    }

    #[test]
    fn fixed_len_excess_bytes() {
        let fixed = FixedLen { a: 1, b: 2, c: 3 };

        let mut bytes = fixed.as_ssz_bytes();
        bytes.append(&mut vec![0]);

        assert_eq!(
            FixedLen::from_ssz_bytes(&bytes),
            Err(DecodeError::InvalidByteLength { len: 15, expected: 14 })
        );
    }

    #[test]
    fn vec_of_fixed_len_struct() {
        let items: Vec<FixedLen> = vec![
            FixedLen { a: 0, b: 0, c: 0 },
            FixedLen { a: 1, b: 1, c: 1 },
            FixedLen { a: 1, b: 0, c: 1 },
        ];

        round_trip(items);
    }

    #[derive(Debug, PartialEq, Eq, Encode, Decode)]
    struct VariableLen {
        a: u16,
        b: Vec<u16>,
        c: u32,
    }

    #[test]
    #[allow(clippy::zero_prefixed_literal)]
    fn offset_into_fixed_bytes() {
        let bytes = vec![
            //  1   2   3   4   5   6   7   8   9   10  11  12  13  14  15
            //      | offset        | u32           | variable
            01, 00, 09, 00, 00, 00, 01, 00, 00, 00, 00, 00, 01, 00, 02, 00,
        ];

        assert_eq!(
            VariableLen::from_ssz_bytes(&bytes),
            Err(DecodeError::OffsetIntoFixedPortion(9))
        );
    }

    #[test]
    fn variable_len_excess_bytes() {
        let variable = VariableLen { a: 1, b: vec![2], c: 3 };

        let mut bytes = variable.as_ssz_bytes();
        bytes.append(&mut vec![0]);

        // The error message triggered is not so helpful, it's caught by a side-effect. Just
        // checking there is _some_ error is fine.
        assert!(VariableLen::from_ssz_bytes(&bytes).is_err());
    }

    #[test]
    #[allow(clippy::zero_prefixed_literal)]
    fn first_offset_skips_byte() {
        let bytes = vec![
            //  1   2   3   4   5   6   7   8   9   10  11  12  13  14  15
            //      | offset        | u32           | variable
            01, 00, 11, 00, 00, 00, 01, 00, 00, 00, 00, 00, 01, 00, 02, 00,
        ];

        assert_eq!(
            VariableLen::from_ssz_bytes(&bytes),
            Err(DecodeError::OffsetSkipsVariableBytes(11))
        );
    }

    #[test]
    #[allow(clippy::zero_prefixed_literal)]
    fn variable_len_struct_encoding() {
        let items: Vec<VariableLen> = vec![
            VariableLen { a: 0, b: vec![], c: 0 },
            VariableLen { a: 1, b: vec![0], c: 1 },
            VariableLen { a: 1, b: vec![0, 1, 2], c: 1 },
        ];

        let expected_encodings = [
            //   00..................................09
            //  | u16--| vec offset-----| u32------------| vec payload --------|
            vec![00, 00, 10, 00, 00, 00, 00, 00, 00, 00],
            vec![01, 00, 10, 00, 00, 00, 01, 00, 00, 00, 00, 00],
            vec![01, 00, 10, 00, 00, 00, 01, 00, 00, 00, 00, 00, 01, 00, 02, 00],
        ];

        for i in 0..items.len() {
            assert_eq!(items[i].as_ssz_bytes(), expected_encodings[i], "Failed on {}", i);
        }
    }

    #[test]
    fn vec_of_variable_len_struct() {
        let items: Vec<VariableLen> = vec![
            VariableLen { a: 0, b: vec![], c: 0 },
            VariableLen { a: 255, b: vec![0, 1, 2, 3], c: 99 },
            VariableLen { a: 255, b: vec![0], c: 99 },
            VariableLen { a: 50, b: vec![0], c: 0 },
        ];

        round_trip(items);
    }

    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
    struct ThreeVariableLen {
        a: u16,
        b: Vec<u16>,
        c: Vec<u16>,
        d: Vec<u16>,
    }

    #[test]
    fn three_variable_len() {
        let vec: Vec<ThreeVariableLen> =
            vec![ThreeVariableLen { a: 42, b: vec![0], c: vec![1], d: vec![2] }];

        round_trip(vec);
    }

    #[test]
    #[allow(clippy::zero_prefixed_literal)]
    fn offsets_decreasing() {
        let bytes = vec![
            //  1   2   3   4   5   6   7   8   9   10  11  12  13  14  15
            //      | offset        | offset        | offset        | variable
            01, 00, 14, 00, 00, 00, 15, 00, 00, 00, 14, 00, 00, 00, 00, 00,
        ];

        assert_eq!(
            ThreeVariableLen::from_ssz_bytes(&bytes),
            Err(DecodeError::OffsetsAreDecreasing(14))
        );
    }

    #[test]
    fn tuple_u8_u16() {
        let vec: Vec<(u8, u16)> = vec![
            (0, 0),
            (0, 1),
            (1, 0),
            (u8::MAX, u16::MAX),
            (0, u16::MAX),
            (u8::MAX, 0),
            (42, 12301),
        ];

        round_trip(vec);
    }

    #[test]
    fn tuple_vec_vec() {
        let vec: Vec<(u64, Vec<u8>, Vec<Vec<u16>>)> = vec![
            (0, vec![], vec![vec![]]),
            (99, vec![101], vec![vec![], vec![]]),
            (42, vec![12, 13, 14], vec![vec![99, 98, 97, 96], vec![42, 44, 46, 48, 50]]),
        ];

        round_trip(vec);
    }

    #[test]
    fn btree_map_fixed() {
        let data =
            vec![BTreeMap::new(), BTreeMap::from_iter(vec![(0u8, 0u16), (1, 2), (2, 4), (4, 6)])];
        round_trip(data);
    }

    #[test]
    fn btree_map_variable_value() {
        let data = vec![
            BTreeMap::new(),
            BTreeMap::from_iter(vec![
                (0u64, ThreeVariableLen { a: 1, b: vec![3, 5, 7], c: vec![], d: vec![0, 0] }),
                (
                    1,
                    ThreeVariableLen {
                        a: 99,
                        b: vec![1],
                        c: vec![2, 3, 4, 5, 6, 7, 8, 9, 10],
                        d: vec![4, 5, 6, 7, 8],
                    },
                ),
                (2, ThreeVariableLen { a: 0, b: vec![], c: vec![], d: vec![] }),
            ]),
        ];
        round_trip(data);
    }

    #[test]
    fn btree_set_fixed() {
        let data = vec![BTreeSet::new(), BTreeSet::from_iter(vec![0u16, 2, 4, 6])];
        round_trip(data);
    }

    #[test]
    fn btree_set_variable_len() {
        let data = vec![
            BTreeSet::new(),
            BTreeSet::from_iter(vec![
                ThreeVariableLen { a: 1, b: vec![3, 5, 7], c: vec![], d: vec![0, 0] },
                ThreeVariableLen {
                    a: 99,
                    b: vec![1],
                    c: vec![2, 3, 4, 5, 6, 7, 8, 9, 10],
                    d: vec![4, 5, 6, 7, 8],
                },
                ThreeVariableLen { a: 0, b: vec![], c: vec![], d: vec![] },
            ]),
        ];
        round_trip(data);
    }

    #[test]
    fn non_zero_usize() {
        let data = vec![
            NonZeroUsize::new(1).unwrap(),
            NonZeroUsize::new(u16::MAX as usize).unwrap(),
            NonZeroUsize::new(usize::MAX).unwrap(),
        ];
        round_trip(data);
    }

    #[test]
    fn arc_u64() {
        let data = vec![Arc::new(0u64), Arc::new(u64::MAX)];
        round_trip(data);
    }

    #[test]
    fn arc_vec_u64() {
        let data = vec![Arc::new(vec![0u64]), Arc::new(vec![u64::MAX; 10])];
        round_trip(data);
    }

    #[test]
    fn alloy_u128() {
        let data =
            vec![U128::from(0), U128::from(u128::MAX), U128::from(u64::MAX), U128::from(255)];
        round_trip(data);
    }

    #[test]
    fn vec_of_option_alloy_u128() {
        let data = vec![
            vec![Some(U128::from(u128::MAX)), Some(U128::from(0)), None],
            vec![None],
            vec![],
            vec![Some(U128::from(0))],
        ];
        round_trip(data);
    }

    #[test]
    fn u256() {
        let data = vec![U256::from(0), U256::MAX, U256::from(u64::MAX), U256::from(255)];
        round_trip(data);
    }

    #[test]
    fn vec_of_option_u256() {
        let data = vec![
            vec![Some(U256::MAX), Some(U256::from(0)), None],
            vec![None],
            vec![],
            vec![Some(U256::from(0))],
        ];
        round_trip(data);
    }

    #[test]
    fn alloy_bytes() {
        let data = vec![
            Bytes::new(),
            Bytes::from_static(&[1, 2, 3]),
            Bytes::from_static(&[0; 32]),
            Bytes::from_static(&[0]),
        ];
        round_trip(data);
    }

    #[test]
    fn tuple_option() {
        let data = vec![(48u8, Some(0u64)), (0u8, None), (u8::MAX, Some(u64::MAX))];
        round_trip(data);
    }

    #[test]
    fn bloom() {
        let data = vec![Bloom::ZERO, Bloom::with_last_byte(5), Bloom::repeat_byte(73)];
        round_trip(data);
    }

    #[test]
    fn vec_bloom() {
        let data = vec![
            vec![Bloom::ZERO, Bloom::ZERO, Bloom::with_last_byte(5)],
            vec![],
            vec![Bloom::repeat_byte(73), Bloom::repeat_byte(72)],
        ];
        round_trip(data);
    }
}

/// Decode tests that are expected to fail.
mod decode_fail {
    use super::*;

    #[test]
    fn non_zero_usize() {
        let zero_bytes = 0usize.as_ssz_bytes();
        assert!(NonZeroUsize::from_ssz_bytes(&zero_bytes).is_err());
    }

    #[test]
    fn hash160() {
        let long_bytes = B256::repeat_byte(0xff).as_ssz_bytes();
        assert!(Address::from_ssz_bytes(&long_bytes).is_err());
    }

    #[test]
    fn hash256() {
        let long_bytes = vec![0xff; 33];
        assert!(B256::from_ssz_bytes(&long_bytes).is_err());
    }

    #[test]
    fn bloom() {
        let long_bytes = vec![0xff; 257];
        assert!(Bloom::from_ssz_bytes(&long_bytes).is_err());
    }
}
