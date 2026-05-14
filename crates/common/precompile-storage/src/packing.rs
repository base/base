//! Shared utilities for packing and unpacking values in EVM storage slots.
//!
//! This module provides helper functions for bit-level manipulation of storage slots,
//! enabling efficient packing of multiple small values into single 32-byte slots.
//!
//! Packing only applies to primitive types where `LAYOUT::Bytes(count) && count < 32`.
//! Non-primitives (structs, fixed-size arrays, dynamic types) have `LAYOUT = Layout::Slot`.
//!
//! ## Solidity Compatibility
//!
//! This implementation matches Solidity's value packing convention:
//! - Values are right-aligned within their byte range
//! - Types smaller than 32 bytes can pack multiple per slot when dimensions align

use alloy_primitives::U256;

use crate::{
    error::Result,
    provider::{FromWord, Layout, StorableType, StorageOps},
};

/// A helper struct to support packing elements into a single slot. Represents an
/// in-memory storage slot value.
///
/// We used it when we operate on elements that are guaranteed to be packable.
/// To avoid doing multiple storage reads/writes when packing those elements, we
/// use this as an intermediate [`StorageOps`] implementation that can be passed to
/// `Storable::store` and `Storable::load`.
#[derive(Debug)]
pub struct PackedSlot(pub U256);

impl StorageOps for PackedSlot {
    fn load(&self, _slot: U256) -> Result<U256> {
        Ok(self.0)
    }

    fn store(&mut self, _slot: U256, value: U256) -> Result<()> {
        self.0 = value;
        Ok(())
    }
}

/// Location information for a packed field within a storage slot.
#[derive(Debug, Clone, Copy)]
pub struct FieldLocation {
    /// Offset in slots from the base slot
    pub offset_slots: usize,
    /// Offset in bytes within the target slot
    pub offset_bytes: usize,
    /// Size of the field in bytes
    pub size: usize,
}

impl FieldLocation {
    /// Create a new field location
    #[inline]
    pub const fn new(offset_slots: usize, offset_bytes: usize, size: usize) -> Self {
        Self { offset_slots, offset_bytes, size }
    }
}

/// Create a bit mask for a value of the given byte size.
///
/// For values less than 32 bytes, returns a mask with the appropriate number of bits set.
/// For 32-byte values, returns `U256::MAX`.
#[inline]
pub fn create_element_mask(byte_count: usize) -> U256 {
    if byte_count >= 32 { U256::MAX } else { (U256::ONE << (byte_count * 8)) - U256::ONE }
}

/// Extract a packed value from a storage slot at a given byte offset.
#[inline]
pub fn extract_from_word<T: FromWord + StorableType>(
    slot_value: U256,
    offset: usize,
    bytes: usize,
) -> Result<T> {
    debug_assert!(
        matches!(T::LAYOUT, Layout::Bytes(..)),
        "Packing is only supported by primitive types"
    );

    if offset + bytes > 32 {
        return Err(crate::error::BasePrecompileError::Fatal(format!(
            "Value of {} bytes at offset {} would span slot boundary (max offset: {})",
            bytes,
            offset,
            32 - bytes
        )));
    }

    let shift_bits = offset * 8;
    let mask = create_element_mask(bytes);

    T::from_word((slot_value >> shift_bits) & mask)
}

/// Insert a packed value into a storage slot at a given byte offset.
#[inline]
pub fn insert_into_word<T: FromWord + StorableType>(
    current: U256,
    value: &T,
    offset: usize,
    bytes: usize,
) -> Result<U256> {
    debug_assert!(
        matches!(T::LAYOUT, Layout::Bytes(..)),
        "Packing is only supported by primitive types"
    );

    if offset + bytes > 32 {
        return Err(crate::error::BasePrecompileError::Fatal(format!(
            "Value of {} bytes at offset {} would span slot boundary (max offset: {})",
            bytes,
            offset,
            32 - bytes
        )));
    }

    let field_value = value.to_word();
    let shift_bits = offset * 8;
    let mask = create_element_mask(bytes);
    let clear_mask = !(mask << shift_bits);
    let cleared = current & clear_mask;
    let positioned = (field_value & mask) << shift_bits;
    Ok(cleared | positioned)
}

/// Zero out a packed value in a storage slot at a given byte offset.
#[inline]
pub fn delete_from_word(current: U256, offset: usize, bytes: usize) -> Result<U256> {
    if offset + bytes > 32 {
        return Err(crate::error::BasePrecompileError::Fatal(format!(
            "Value of {} bytes at offset {} would span slot boundary (max offset: {})",
            bytes,
            offset,
            32 - bytes
        )));
    }

    let mask = create_element_mask(bytes);
    let shifted_mask = mask << (offset * 8);
    Ok(current & !shifted_mask)
}

/// Calculate which slot an array element at index `idx` starts in.
#[inline]
pub const fn calc_element_slot(idx: usize, elem_bytes: usize) -> usize {
    let elems_per_slot = 32 / elem_bytes;
    idx / elems_per_slot
}

/// Calculate the byte offset within a slot for an array element at index `idx`.
#[inline]
pub const fn calc_element_offset(idx: usize, elem_bytes: usize) -> usize {
    let elems_per_slot = 32 / elem_bytes;
    (idx % elems_per_slot) * elem_bytes
}

/// Calculate the element location within a slot for an array element at index `idx`.
#[inline]
pub const fn calc_element_loc(idx: usize, elem_bytes: usize) -> FieldLocation {
    FieldLocation::new(
        calc_element_slot(idx, elem_bytes),
        calc_element_offset(idx, elem_bytes),
        elem_bytes,
    )
}

/// Calculate the total number of slots needed for an array.
#[inline]
pub const fn calc_packed_slot_count(n: usize, elem_bytes: usize) -> usize {
    let elems_per_slot = 32 / elem_bytes;
    n.div_ceil(elems_per_slot)
}

/// Test helper: constructs a U256 slot from hex string literals, left-padded to 32 bytes.
///
/// Takes an array of hex strings (with or without "0x" prefix), concatenates them
/// left-to-right, left-pads with zeros to 32 bytes, and returns a U256.
#[cfg(any(test, feature = "test-utils"))]
pub fn gen_word_from(values: &[&str]) -> U256 {
    let mut bytes = Vec::new();

    for value in values {
        let hex_str = value.strip_prefix("0x").unwrap_or(value);

        assert!(hex_str.len() % 2 == 0, "Hex string '{value}' has odd length");

        for i in (0..hex_str.len()).step_by(2) {
            let byte_str = &hex_str[i..i + 2];
            let byte = u8::from_str_radix(byte_str, 16)
                .unwrap_or_else(|e| panic!("Invalid hex in '{value}': {e}"));
            bytes.push(byte);
        }
    }

    assert!(bytes.len() <= 32, "Total bytes ({}) exceed 32-byte slot limit", bytes.len());

    let mut slot_bytes = [0u8; 32];
    let start_idx = 32 - bytes.len();
    slot_bytes[start_idx..].copy_from_slice(&bytes);

    U256::from_be_bytes(slot_bytes)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;

    use super::*;
    use crate::{
        provider::{Handler, LayoutCtx},
        storage_ctx::StorageCtx,
        types::Slot,
    };

    // -- HELPER FUNCTION TESTS ----------------------------------------------------

    #[test]
    fn test_calc_element_slot() {
        assert_eq!(calc_element_slot(0, 1), 0);
        assert_eq!(calc_element_slot(31, 1), 0);
        assert_eq!(calc_element_slot(32, 1), 1);
        assert_eq!(calc_element_slot(63, 1), 1);
        assert_eq!(calc_element_slot(64, 1), 2);

        assert_eq!(calc_element_slot(0, 2), 0);
        assert_eq!(calc_element_slot(15, 2), 0);
        assert_eq!(calc_element_slot(16, 2), 1);

        assert_eq!(calc_element_slot(0, 20), 0);
        assert_eq!(calc_element_slot(1, 20), 1);
        assert_eq!(calc_element_slot(2, 20), 2);
    }

    #[test]
    fn test_calc_element_offset() {
        assert_eq!(calc_element_offset(0, 1), 0);
        assert_eq!(calc_element_offset(1, 1), 1);
        assert_eq!(calc_element_offset(31, 1), 31);
        assert_eq!(calc_element_offset(32, 1), 0);

        assert_eq!(calc_element_offset(0, 2), 0);
        assert_eq!(calc_element_offset(1, 2), 2);
        assert_eq!(calc_element_offset(15, 2), 30);
        assert_eq!(calc_element_offset(16, 2), 0);

        assert_eq!(calc_element_offset(0, 20), 0);
        assert_eq!(calc_element_offset(1, 20), 0);
        assert_eq!(calc_element_offset(2, 20), 0);
    }

    #[test]
    fn test_calc_packed_slot_count() {
        assert_eq!(calc_packed_slot_count(10, 1), 1);
        assert_eq!(calc_packed_slot_count(32, 1), 1);
        assert_eq!(calc_packed_slot_count(33, 1), 2);
        assert_eq!(calc_packed_slot_count(100, 1), 4);

        assert_eq!(calc_packed_slot_count(16, 2), 1);
        assert_eq!(calc_packed_slot_count(17, 2), 2);

        assert_eq!(calc_packed_slot_count(1, 20), 1);
        assert_eq!(calc_packed_slot_count(2, 20), 2);
        assert_eq!(calc_packed_slot_count(3, 20), 3);
    }

    #[test]
    fn test_calc_element_loc_non_divisor_sizes() {
        assert_eq!(calc_element_slot(0, 11), 0);
        assert_eq!(calc_element_slot(1, 11), 0);
        assert_eq!(calc_element_slot(2, 11), 1);
        assert_eq!(calc_element_slot(3, 11), 1);
        assert_eq!(calc_element_slot(4, 11), 2);

        assert_eq!(calc_element_offset(0, 11), 0);
        assert_eq!(calc_element_offset(1, 11), 11);
        assert_eq!(calc_element_offset(2, 11), 0);
        assert_eq!(calc_element_offset(3, 11), 11);
        assert_eq!(calc_element_offset(4, 11), 0);

        assert_eq!(calc_packed_slot_count(1, 11), 1);
        assert_eq!(calc_packed_slot_count(2, 11), 1);
        assert_eq!(calc_packed_slot_count(3, 11), 2);
        assert_eq!(calc_packed_slot_count(4, 11), 2);
        assert_eq!(calc_packed_slot_count(5, 11), 3);
    }

    #[test]
    fn test_offset_never_exceeds_slot_boundary() {
        for elem_bytes in 1..=32 {
            for idx in 0..10 {
                let offset = calc_element_offset(idx, elem_bytes);
                assert!(
                    offset + elem_bytes <= 32,
                    "elem_bytes={elem_bytes}, idx={idx}, offset={offset} would cross slot boundary"
                );
            }
        }
    }

    #[test]
    fn test_create_element_mask() {
        assert_eq!(create_element_mask(1), U256::from(0xff));
        assert_eq!(create_element_mask(2), U256::from(0xffff));
        assert_eq!(create_element_mask(4), U256::from(0xffffffffu32));
        assert_eq!(create_element_mask(8), U256::from(u64::MAX));
        assert_eq!(create_element_mask(16), U256::from(u128::MAX));
        assert_eq!(create_element_mask(32), U256::MAX);
        assert_eq!(create_element_mask(64), U256::MAX);
    }

    #[test]
    fn test_delete_from_word() {
        let slot = gen_word_from(&["0xff", "0x56", "0x34", "0x12"]);

        let cleared = delete_from_word(slot, 1, 1).unwrap();
        let expected = gen_word_from(&["0xff", "0x56", "0x00", "0x12"]);
        assert_eq!(cleared, expected, "Should zero offset 1");

        let slot = gen_word_from(&["0x5678", "0x1234"]);
        let cleared = delete_from_word(slot, 0, 2).unwrap();
        let expected = gen_word_from(&["0x5678", "0x0000"]);
        assert_eq!(cleared, expected, "Should zero u16 at offset 0");

        let slot = gen_word_from(&["0xff"]);
        let cleared = delete_from_word(slot, 0, 1).unwrap();
        assert_eq!(cleared, U256::ZERO, "Should zero entire slot");
    }

    #[test]
    fn test_boundary_validation_rejects_spanning() {
        let addr = Address::random();
        let result = insert_into_word(U256::ZERO, &addr, 13, 20);
        assert!(result.is_err(), "Should reject address at offset 13");

        let val: u16 = 42;
        let result = insert_into_word(U256::ZERO, &val, 31, 2);
        assert!(result.is_err(), "Should reject u16 at offset 31");

        let val: u32 = 42;
        let result = insert_into_word(U256::ZERO, &val, 29, 4);
        assert!(result.is_err(), "Should reject u32 at offset 29");

        let result = extract_from_word::<Address>(U256::ZERO, 13, 20);
        assert!(result.is_err(), "Should reject extracting address from offset 13");
    }

    #[test]
    fn test_boundary_validation_accepts_valid() {
        let addr = Address::random();
        assert!(insert_into_word(U256::ZERO, &addr, 12, 20).is_ok());

        let val: u16 = 42;
        assert!(insert_into_word(U256::ZERO, &val, 30, 2).is_ok());

        let val: u8 = 42;
        assert!(insert_into_word(U256::ZERO, &val, 31, 1).is_ok());

        let val = U256::from(42);
        assert!(insert_into_word(U256::ZERO, &val, 0, 32).is_ok());
    }

    #[test]
    fn test_bool() {
        let expected = gen_word_from(&["0x01"]);
        let slot = insert_into_word(U256::ZERO, &true, 0, 1).unwrap();
        assert_eq!(slot, expected);
        assert!(extract_from_word::<bool>(slot, 0, 1).unwrap());

        let expected = gen_word_from(&["0x01", "0x01"]);
        let mut slot = U256::ZERO;
        slot = insert_into_word(slot, &true, 0, 1).unwrap();
        slot = insert_into_word(slot, &true, 1, 1).unwrap();
        assert_eq!(slot, expected);
        assert!(extract_from_word::<bool>(slot, 0, 1).unwrap());
        assert!(extract_from_word::<bool>(slot, 1, 1).unwrap());
    }

    #[test]
    fn test_u8_packing() {
        let v1: u8 = 0x12;
        let v2: u8 = 0x34;
        let v3: u8 = 0x56;
        let v4: u8 = u8::MAX;

        let expected = gen_word_from(&["0xff", "0x56", "0x34", "0x12"]);

        let mut slot = U256::ZERO;
        slot = insert_into_word(slot, &v1, 0, 1).unwrap();
        slot = insert_into_word(slot, &v2, 1, 1).unwrap();
        slot = insert_into_word(slot, &v3, 2, 1).unwrap();
        slot = insert_into_word(slot, &v4, 3, 1).unwrap();

        assert_eq!(slot, expected);
        assert_eq!(extract_from_word::<u8>(slot, 0, 1).unwrap(), v1);
        assert_eq!(extract_from_word::<u8>(slot, 1, 1).unwrap(), v2);
        assert_eq!(extract_from_word::<u8>(slot, 2, 1).unwrap(), v3);
        assert_eq!(extract_from_word::<u8>(slot, 3, 1).unwrap(), v4);
    }

    #[test]
    fn test_u16_packing() {
        let v1: u16 = 0x1234;
        let v2: u16 = 0x5678;
        let v3: u16 = u16::MAX;

        let expected = gen_word_from(&["0xffff", "0x5678", "0x1234"]);

        let mut slot = U256::ZERO;
        slot = insert_into_word(slot, &v1, 0, 2).unwrap();
        slot = insert_into_word(slot, &v2, 2, 2).unwrap();
        slot = insert_into_word(slot, &v3, 4, 2).unwrap();

        assert_eq!(slot, expected);
        assert_eq!(extract_from_word::<u16>(slot, 0, 2).unwrap(), v1);
        assert_eq!(extract_from_word::<u16>(slot, 2, 2).unwrap(), v2);
        assert_eq!(extract_from_word::<u16>(slot, 4, 2).unwrap(), v3);
    }

    #[test]
    fn test_u32_packing() {
        let v1: u32 = 0x12345678;
        let v2: u32 = u32::MAX;

        let expected = gen_word_from(&["0xffffffff", "0x12345678"]);

        let mut slot = U256::ZERO;
        slot = insert_into_word(slot, &v1, 0, 4).unwrap();
        slot = insert_into_word(slot, &v2, 4, 4).unwrap();

        assert_eq!(slot, expected);
        assert_eq!(extract_from_word::<u32>(slot, 0, 4).unwrap(), v1);
        assert_eq!(extract_from_word::<u32>(slot, 4, 4).unwrap(), v2);
    }

    #[test]
    fn test_u64_packing() {
        let v1: u64 = 0x123456789abcdef0;
        let v2: u64 = u64::MAX;

        let expected = gen_word_from(&["0xffffffffffffffff", "0x123456789abcdef0"]);

        let mut slot = U256::ZERO;
        slot = insert_into_word(slot, &v1, 0, 8).unwrap();
        slot = insert_into_word(slot, &v2, 8, 8).unwrap();

        assert_eq!(slot, expected);
        assert_eq!(extract_from_word::<u64>(slot, 0, 8).unwrap(), v1);
        assert_eq!(extract_from_word::<u64>(slot, 8, 8).unwrap(), v2);
    }

    #[test]
    fn test_u128_packing() {
        let v1: u128 = 0x123456789abcdef0fedcba9876543210;
        let v2: u128 = u128::MAX;

        let expected = gen_word_from(&[
            "0xffffffffffffffffffffffffffffffff",
            "0x123456789abcdef0fedcba9876543210",
        ]);

        let mut slot = U256::ZERO;
        slot = insert_into_word(slot, &v1, 0, 16).unwrap();
        slot = insert_into_word(slot, &v2, 16, 16).unwrap();

        assert_eq!(slot, expected);
        assert_eq!(extract_from_word::<u128>(slot, 0, 16).unwrap(), v1);
        assert_eq!(extract_from_word::<u128>(slot, 16, 16).unwrap(), v2);
    }

    #[test]
    fn test_mixed_type_packing() {
        let addr = Address::from([0x11; 20]);
        let number: u8 = 0x2a;

        let expected =
            gen_word_from(&["0x2a", "0x1111111111111111111111111111111111111111", "0x01"]);

        let mut slot = U256::ZERO;
        slot = insert_into_word(slot, &true, 0, 1).unwrap();
        slot = insert_into_word(slot, &addr, 1, 20).unwrap();
        slot = insert_into_word(slot, &number, 21, 1).unwrap();
        assert_eq!(slot, expected);
        assert!(extract_from_word::<bool>(slot, 0, 1).unwrap());
        assert_eq!(extract_from_word::<Address>(slot, 1, 20).unwrap(), addr);
        assert_eq!(extract_from_word::<u8>(slot, 21, 1).unwrap(), number);
    }

    #[test]
    fn test_packed_at_multiple_types() -> Result<()> {
        let (mut storage, address) = crate::hashmap::setup_storage();
        StorageCtx::enter(&mut storage, || {
            let struct_base = U256::from(0x2000);

            let flag = true;
            let timestamp: u64 = 1234567890;
            let amount: u128 = 999888777666;

            let mut flag_slot =
                Slot::<bool>::new_with_ctx(struct_base, LayoutCtx::packed(0), address);
            flag_slot.write(flag)?;
            assert_eq!(flag_slot.read()?, flag);

            let mut ts_slot = Slot::<u64>::new_with_ctx(struct_base, LayoutCtx::packed(1), address);
            ts_slot.write(timestamp)?;
            assert_eq!(ts_slot.read()?, timestamp);

            let mut amount_slot =
                Slot::<u128>::new_with_ctx(struct_base, LayoutCtx::packed(9), address);
            amount_slot.write(amount)?;
            assert_eq!(amount_slot.read()?, amount);

            amount_slot.delete()?;
            assert_eq!(flag_slot.read()?, flag);
            assert_eq!(amount_slot.read()?, 0);
            assert_eq!(ts_slot.read()?, timestamp);

            Ok(())
        })
    }

    use proptest::prelude::*;

    fn arb_address() -> impl Strategy<Value = Address> {
        any::<[u8; 20]>().prop_map(Address::from)
    }

    fn arb_u256() -> impl Strategy<Value = U256> {
        any::<[u64; 4]>().prop_map(U256::from_limbs)
    }

    fn arb_offset(bytes: usize) -> impl Strategy<Value = usize> {
        0..=(32 - bytes)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(500))]

        #[test]
        fn proptest_roundtrip_u8(value: u8, offset in arb_offset(1)) {
            let slot = insert_into_word(U256::ZERO, &value, offset, 1)?;
            let extracted: u8 = extract_from_word(slot, offset, 1)?;
            prop_assert_eq!(extracted, value);
        }

        #[test]
        fn proptest_roundtrip_u16(value: u16, offset in arb_offset(2)) {
            let slot = insert_into_word(U256::ZERO, &value, offset, 2)?;
            let extracted: u16 = extract_from_word(slot, offset, 2)?;
            prop_assert_eq!(extracted, value);
        }

        #[test]
        fn proptest_roundtrip_u32(value: u32, offset in arb_offset(4)) {
            let slot = insert_into_word(U256::ZERO, &value, offset, 4)?;
            let extracted: u32 = extract_from_word(slot, offset, 4)?;
            prop_assert_eq!(extracted, value);
        }

        #[test]
        fn proptest_roundtrip_u64(value: u64, offset in arb_offset(8)) {
            let slot = insert_into_word(U256::ZERO, &value, offset, 8)?;
            let extracted: u64 = extract_from_word(slot, offset, 8)?;
            prop_assert_eq!(extracted, value);
        }

        #[test]
        fn proptest_roundtrip_u128(value: u128, offset in arb_offset(16)) {
            let slot = insert_into_word(U256::ZERO, &value, offset, 16)?;
            let extracted: u128 = extract_from_word(slot, offset, 16)?;
            prop_assert_eq!(extracted, value);
        }

        #[test]
        fn proptest_roundtrip_address(addr in arb_address(), offset in arb_offset(20)) {
            let slot = insert_into_word(U256::ZERO, &addr, offset, 20)?;
            let extracted: Address = extract_from_word(slot, offset, 20)?;
            prop_assert_eq!(extracted, addr);
        }

        #[test]
        fn proptest_roundtrip_u256(value in arb_u256()) {
            let slot = insert_into_word(U256::ZERO, &value, 0, 32)?;
            let extracted: U256 = extract_from_word(slot, 0, 32)?;
            prop_assert_eq!(extracted, value);
        }

        #[test]
        fn proptest_roundtrip_bool(value: bool, offset in arb_offset(1)) {
            let slot = insert_into_word(U256::ZERO, &value, offset, 1)?;
            let extracted: bool = extract_from_word(slot, offset, 1)?;
            prop_assert_eq!(extracted, value);
        }

        #[test]
        fn proptest_roundtrip_i8(value: i8, offset in arb_offset(1)) {
            let slot = insert_into_word(U256::ZERO, &value, offset, 1)?;
            let extracted: i8 = extract_from_word(slot, offset, 1)?;
            prop_assert_eq!(extracted, value);
        }

        #[test]
        fn proptest_roundtrip_i16(value: i16, offset in arb_offset(2)) {
            let slot = insert_into_word(U256::ZERO, &value, offset, 2)?;
            let extracted: i16 = extract_from_word(slot, offset, 2)?;
            prop_assert_eq!(extracted, value);
        }

        #[test]
        fn proptest_roundtrip_i32(value: i32, offset in arb_offset(4)) {
            let slot = insert_into_word(U256::ZERO, &value, offset, 4)?;
            let extracted: i32 = extract_from_word(slot, offset, 4)?;
            prop_assert_eq!(extracted, value);
        }

        #[test]
        fn proptest_roundtrip_i64(value: i64, offset in arb_offset(8)) {
            let slot = insert_into_word(U256::ZERO, &value, offset, 8)?;
            let extracted: i64 = extract_from_word(slot, offset, 8)?;
            prop_assert_eq!(extracted, value);
        }

        #[test]
        fn proptest_roundtrip_i128(value: i128, offset in arb_offset(16)) {
            let slot = insert_into_word(U256::ZERO, &value, offset, 16)?;
            let extracted: i128 = extract_from_word(slot, offset, 16)?;
            prop_assert_eq!(extracted, value);
        }

        #[test]
        fn proptest_multiple_values_no_interference(v1: u8, v2: u16, v3: u32) {
            let mut slot = U256::ZERO;
            slot = insert_into_word(slot, &v1, 0, 1)?;
            slot = insert_into_word(slot, &v2, 1, 2)?;
            slot = insert_into_word(slot, &v3, 3, 4)?;

            let e1: u8 = extract_from_word(slot, 0, 1)?;
            let e2: u16 = extract_from_word(slot, 1, 2)?;
            let e3: u32 = extract_from_word(slot, 3, 4)?;

            prop_assert_eq!(e1, v1);
            prop_assert_eq!(e2, v2);
            prop_assert_eq!(e3, v3);
        }

        #[test]
        fn proptest_overwrite_preserves_others(v1: u8, v2: u16, v1_new: u8) {
            let mut slot = U256::ZERO;
            slot = insert_into_word(slot, &v1, 0, 1)?;
            slot = insert_into_word(slot, &v2, 1, 2)?;
            slot = insert_into_word(slot, &v1_new, 0, 1)?;

            let e1: u8 = extract_from_word(slot, 0, 1)?;
            let e2: u16 = extract_from_word(slot, 1, 2)?;

            prop_assert_eq!(e1, v1_new);
            prop_assert_eq!(e2, v2);
        }

        #[test]
        fn proptest_element_slot_offset_consistency_u8(idx in 0usize..1000) {
            let slot = calc_element_slot(idx, 1);
            let offset = calc_element_offset(idx, 1);
            prop_assert_eq!(slot * 32 + offset, idx);
            prop_assert!(offset < 32);
        }

        #[test]
        fn proptest_element_slot_offset_consistency_u16(idx in 0usize..1000) {
            let slot = calc_element_slot(idx, 2);
            let offset = calc_element_offset(idx, 2);
            prop_assert_eq!(slot * 32 + offset, idx * 2);
            prop_assert!(offset < 32);
        }

        #[test]
        fn proptest_packed_slot_count_sufficient(n in 1usize..100, elem_bytes in 1usize..=32) {
            let slot_count = calc_packed_slot_count(n, elem_bytes);
            let elems_per_slot = 32 / elem_bytes;
            let expected = n.div_ceil(elems_per_slot);
            prop_assert_eq!(slot_count, expected);
            prop_assert!(slot_count * elems_per_slot >= n);
            if slot_count > 0 {
                prop_assert!(slot_count * elems_per_slot - n < elems_per_slot);
            }
        }
    }
}
