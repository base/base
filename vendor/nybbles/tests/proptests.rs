#![cfg(feature = "arbitrary")]

use nybbles::Nibbles;
use proptest::{collection::vec, prelude::*};

fn valid_nibbles(nibbles: &Nibbles) -> bool {
    nibbles.to_vec().iter().all(|&nibble| nibble <= 0xf)
}

// Basic operations group - creation, conversion, basic manipulation
proptest! {
    #[test]
    #[cfg_attr(miri, ignore = "no proptest")]
    fn pack_unpack_roundtrip(input in vec(any::<u8>(), 0..=32)) {
        let nibbles = Nibbles::unpack(&input);
        prop_assert!(valid_nibbles(&nibbles));
        let packed = nibbles.pack();
        prop_assert_eq!(&packed[..], input);
    }

    #[test]
    #[cfg_attr(miri, ignore = "no proptest")]
    fn from_nibbles_roundtrip(nibbles_data in vec(0u8..16, 0..=64)) {
        let nibbles = Nibbles::from_nibbles(&nibbles_data);
        prop_assert_eq!(nibbles.to_vec(), &nibbles_data[..]);
        prop_assert_eq!(nibbles.len(), nibbles_data.len());
    }

    #[test]
    #[cfg_attr(miri, ignore = "no proptest")]
    fn push_pop_roundtrip(
        initial_nibbles in vec(0u8..16, 0..64),
        extra_nibble in 0u8..16
    ) {
        let mut nibbles = Nibbles::from_nibbles(&initial_nibbles);
        let original_len = nibbles.len();

        nibbles.push(extra_nibble);
        prop_assert_eq!(nibbles.len(), original_len + 1);
        prop_assert_eq!(nibbles.last(), Some(extra_nibble));

        let popped = nibbles.pop();
        prop_assert_eq!(popped, Some(extra_nibble));
        prop_assert_eq!(nibbles.len(), original_len);
        prop_assert_eq!(nibbles.to_vec(), &initial_nibbles[..]);
    }

    #[test]
    #[cfg_attr(miri, ignore = "no proptest")]
    fn get_byte_consistency(nibbles_data in vec(0u8..16, 2..=64)) {
        let nibbles = Nibbles::from_nibbles(&nibbles_data);
        for i in 0..nibbles_data.len()-1 {
            let expected = (nibbles_data[i] << 4) | nibbles_data[i + 1];
            prop_assert_eq!(nibbles.get_byte(i), Some(expected));
        }

        // Test boundary conditions
        // Last valid index (requires at least 2 nibbles)
        if nibbles_data.len() >= 2 {
            prop_assert!(nibbles.get_byte(nibbles_data.len()-2).is_some());
        }
        // First invalid index
        prop_assert_eq!(nibbles.get_byte(nibbles_data.len()-1), None);
    }

    #[test]
    #[cfg_attr(miri, ignore = "no proptest")]
    fn first_last_properties(nibbles_data in vec(0u8..16, 1..=64)) {
        let nibbles = Nibbles::from_nibbles(&nibbles_data);

        prop_assert_eq!(nibbles.first(), Some(nibbles_data[0]));
        prop_assert_eq!(nibbles.last(), Some(*nibbles_data.last().unwrap()));

        // Empty nibbles should return None
        let empty = Nibbles::new();
        prop_assert_eq!(empty.first(), None);
        prop_assert_eq!(empty.last(), None);
    }

    #[test]
    #[cfg_attr(miri, ignore = "no proptest")]
    fn set_at_properties(
        nibbles_data in vec(0u8..16, 1..=64),
        new_value in 0u8..16
    ) {
        let len = nibbles_data.len();

        // Test setting at first index
        let mut nibbles = Nibbles::from_nibbles(&nibbles_data);
        nibbles.set_at(0, new_value);
        prop_assert_eq!(nibbles.get_unchecked(0), new_value);
        prop_assert_eq!(nibbles.len(), len);

        // Test setting at last index
        if len > 1 {
            let mut nibbles = Nibbles::from_nibbles(&nibbles_data);
            nibbles.set_at(len - 1, new_value);
            prop_assert_eq!(nibbles.get_unchecked(len - 1), new_value);
            prop_assert_eq!(nibbles.len(), len);

            // Other elements should remain unchanged
            #[allow(clippy::needless_range_loop)]
            for i in 0..len-1 {
                prop_assert_eq!(nibbles.get_unchecked(i), nibbles_data[i]);
            }
        }

        // Test setting at middle index
        if len > 2 {
            let mid = len / 2;
            let mut nibbles = Nibbles::from_nibbles(&nibbles_data);
            nibbles.set_at(mid, new_value);
            prop_assert_eq!(nibbles.get_unchecked(mid),  new_value);
            prop_assert_eq!(nibbles.len(), len);

            // Other elements should remain unchanged
            for (i, &original) in nibbles_data.iter().enumerate() {
                if i != mid {
                    prop_assert_eq!(nibbles.get_unchecked(i), original);
                }
            }
        }
    }
}

// Slice and manipulation operations group
proptest! {
    #[test]
    #[cfg_attr(miri, ignore = "no proptest")]
    fn slice_consistency(nibbles_data in vec(0u8..16, 1..=64)) {
        let nibbles = Nibbles::from_nibbles(&nibbles_data);
        let len = nibbles_data.len();

        // Test specific slice cases to avoid rejection issues
        // Test full slice
        let full_slice = nibbles.slice(..);
        prop_assert_eq!(full_slice.to_vec(), &nibbles_data[..]);

        // Test first half
        if len > 1 {
            let mid = len / 2;
            let first_half = nibbles.slice(..mid);
            prop_assert_eq!(first_half.to_vec(), &nibbles_data[..mid]);

            // Test second half
            let second_half = nibbles.slice(mid..);
            prop_assert_eq!(second_half.to_vec(), &nibbles_data[mid..]);

            // Test middle slice
            if mid + 1 < len {
                let middle_slice = nibbles.slice(mid..mid+1);
                prop_assert_eq!(middle_slice.to_vec(), &nibbles_data[mid..mid+1]);
            }
        }

        // Test empty slice
        let empty_slice = nibbles.slice(0..0);
        prop_assert_eq!(empty_slice.len(), 0);
    }

    #[test]
    #[cfg_attr(miri, ignore = "no proptest")]
    fn extend_consistency(
        nibbles1 in vec(0u8..16, 0..=32),
        nibbles2 in vec(0u8..16, 0..=32)
    ) {
        let mut result = Nibbles::from_nibbles(&nibbles1);
        let other = Nibbles::from_nibbles(&nibbles2);

        result.extend(&other);

        let expected: Vec<u8> = nibbles1.into_iter().chain(nibbles2.into_iter()).collect();
        prop_assert_eq!(result.to_vec(), &expected[..]);
    }

    #[test]
    #[cfg_attr(miri, ignore = "no proptest")]
    fn join_consistency(
        nibbles1 in vec(0u8..16, 0..=32),
        nibbles2 in vec(0u8..16, 0..=32)
    ) {
        let n1 = Nibbles::from_nibbles(&nibbles1);
        let n2 = Nibbles::from_nibbles(&nibbles2);

        let joined = n1.join(&n2);

        let expected: Vec<u8> = nibbles1.into_iter().chain(nibbles2.into_iter()).collect();
        prop_assert_eq!(joined.to_vec(), &expected[..]);
        prop_assert_eq!(joined.len(), n1.len() + n2.len());
    }

    #[test]
    #[cfg_attr(miri, ignore = "no proptest")]
    fn truncate_consistency(nibbles_data in vec(0u8..16, 1..=64)) {
        let original_len = nibbles_data.len();

        // Test truncating to zero
        let mut nibbles = Nibbles::from_nibbles(&nibbles_data);
        nibbles.truncate(0);
        prop_assert_eq!(nibbles.len(), 0);

        // Test truncating to half length
        if original_len > 1 {
            let mut nibbles = Nibbles::from_nibbles(&nibbles_data);
            let new_len = original_len / 2;
            nibbles.truncate(new_len);
            prop_assert_eq!(nibbles.len(), new_len);
            prop_assert_eq!(nibbles.to_vec(), &nibbles_data[..new_len]);
        }

        // Test truncating to same length (no-op)
        let mut nibbles = Nibbles::from_nibbles(&nibbles_data);
        nibbles.truncate(original_len);
        prop_assert_eq!(nibbles.len(), original_len);
        prop_assert_eq!(nibbles.to_vec(), &nibbles_data[..]);
    }

    #[test]
    #[cfg_attr(miri, ignore = "no proptest")]
    fn increment_properties(nibbles_data in vec(0u8..16, 1..=32)) {
        let nibbles = Nibbles::from_nibbles(&nibbles_data);

        if let Some(incremented) = nibbles.increment() {
            // Length should remain the same
            prop_assert_eq!(incremented.len(), nibbles.len());

            // Should be greater than original (lexicographically)
            prop_assert!(incremented > nibbles);

            // Check that it's the minimal increment
            // (this is more complex to verify, so we'll do basic checks)

            // If original wasn't all 0xF, then increment should exist
            let all_f = nibbles_data.iter().all(|&x| x == 0xf);
            if !all_f {
                // increment succeeded when it shouldn't have failed
                prop_assert!(true); // We already got Some
            }
        } else {
            // increment failed, original should have been all 0xF
            let all_f = nibbles_data.iter().all(|&x| x == 0xf);
            prop_assert!(all_f, "increment() returned None but input wasn't all 0xF");
        }
    }
}

// Query and comparison operations group
proptest! {
    #[test]
    #[cfg_attr(miri, ignore = "no proptest")]
    fn common_prefix_length_properties(
        nibbles1 in vec(0u8..16, 0..=64),
        nibbles2 in vec(0u8..16, 0..=64)
    ) {
        let n1 = Nibbles::from_nibbles(&nibbles1);
        let n2 = Nibbles::from_nibbles(&nibbles2);

        let common_len = n1.common_prefix_length(&n2);

        // Should be symmetric
        prop_assert_eq!(common_len, n2.common_prefix_length(&n1));

        // Should not exceed either length
        prop_assert!(common_len <= n1.len());
        prop_assert!(common_len <= n2.len());

        // Verify the common prefix matches
        if common_len > 0 && !nibbles1.is_empty() && !nibbles2.is_empty() {
            for i in 0..common_len {
                prop_assert_eq!(nibbles1[i], nibbles2[i]);
            }
        }

        // If there's a difference, it should be at the position right after common prefix
        if common_len < n1.len() && common_len < n2.len() {
            prop_assert_ne!(nibbles1[common_len], nibbles2[common_len]);
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "no proptest")]
    fn starts_with_consistency(nibbles_data in vec(0u8..16, 1..=64)) {
        let nibbles = Nibbles::from_nibbles(&nibbles_data);
        let len = nibbles_data.len();

        // Test empty prefix
        prop_assert!(nibbles.starts_with(&Nibbles::default()));

        // Test full prefix
        prop_assert!(nibbles.starts_with(&nibbles));

        // Test first half prefix
        if len > 1 {
            let prefix_len = len / 2;
            let prefix = nibbles.slice(..prefix_len);
            prop_assert!(nibbles.starts_with(&prefix));
            prop_assert!(nibbles.starts_with(&prefix));

            // Test with different prefix
            if prefix_len > 0 {
                let mut different_prefix = prefix;
                different_prefix.set_at(0, (different_prefix.get_unchecked(0) + 1) % 16);
                if different_prefix != prefix {
                    prop_assert!(!nibbles.starts_with(&different_prefix));
                }
            }
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "no proptest")]
    fn ordering_properties(
        nibbles1 in vec(0u8..16, 0..=32),
        nibbles2 in vec(0u8..16, 0..=32)
    ) {
        let n1 = Nibbles::from_nibbles(&nibbles1);
        let n2 = Nibbles::from_nibbles(&nibbles2);

        // Test ordering consistency with slice comparison
        let slice_cmp = nibbles1.cmp(&nibbles2);
        let nibbles_cmp = n1.cmp(&n2);
        prop_assert_eq!(slice_cmp, nibbles_cmp);

        // Test reflexivity
        prop_assert_eq!(n1.cmp(&n1), core::cmp::Ordering::Equal);

        // Test antisymmetry
        prop_assert_eq!(n1.cmp(&n2), n2.cmp(&n1).reverse());
    }
}
