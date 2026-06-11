//! Bytes-like (`Bytes`, `String`) implementation for the storage traits.
//!
//! # Storage Layout
//!
//! **Short strings (≤31 bytes)** are stored inline in a single slot:
//! - Bytes 0..len: data (left-aligned)
//! - Byte 31 (LSB): length * 2 (bit 0 = 0 indicates short string)
//!
//! **Long strings (≥32 bytes)** use keccak256-based storage:
//! - Base slot: stores `length * 2 + 1` (bit 0 = 1 indicates long string)
//! - Data slots: stored at `keccak256(main_slot) + i` for each 32-byte chunk

use alloc::{format, string::String, vec::Vec};
use core::marker::PhantomData;

use alloy_primitives::{Address, Bytes, U256, keccak256};

use crate::{
    error::{BasePrecompileError, Result},
    provider::{
        Handler, Layout, LayoutCtx, Storable, StorableType, StorageKey, StorageOps,
        sealed::OnlyPrimitives,
    },
    types::Slot,
};

impl StorableType for Bytes {
    const LAYOUT: Layout = Layout::Slots(1);
    const IS_DYNAMIC: bool = true;
    type Handler<'a> = BytesLikeHandler<'a, Self>;
    fn handle<'a>(
        slot: U256,
        _ctx: LayoutCtx,
        address: Address,
        storage: crate::StorageCtx<'a>,
    ) -> Self::Handler<'a> {
        BytesLikeHandler::new(slot, address, storage)
    }
}

impl StorableType for String {
    const LAYOUT: Layout = Layout::Slots(1);
    const IS_DYNAMIC: bool = true;
    type Handler<'a> = BytesLikeHandler<'a, Self>;
    fn handle<'a>(
        slot: U256,
        _ctx: LayoutCtx,
        address: Address,
        storage: crate::StorageCtx<'a>,
    ) -> Self::Handler<'a> {
        BytesLikeHandler::new(slot, address, storage)
    }
}

/// Handler for bytes-like types providing efficient length queries.
#[derive(Debug, Clone)]
pub struct BytesLikeHandler<'a, T> {
    base_slot: U256,
    address: Address,
    storage: crate::StorageCtx<'a>,
    _ty: PhantomData<T>,
}

impl<'a, T: Storable> BytesLikeHandler<'a, T> {
    /// Creates a new handler for the bytes-like value at the given base slot.
    #[inline]
    pub const fn new(base_slot: U256, address: Address, storage: crate::StorageCtx<'a>) -> Self {
        Self { base_slot, address, storage, _ty: PhantomData }
    }

    #[inline]
    const fn as_slot(&self) -> Slot<'a, T> {
        Slot::new(self.base_slot, self.address, self.storage)
    }

    /// Returns the byte length without loading all data (reads only the base slot).
    #[inline]
    pub fn len(&self) -> Result<usize> {
        let base_value = Slot::<U256>::new(self.base_slot, self.address, self.storage).read()?;
        let is_long = is_long_string(base_value);
        calc_string_length(base_value, is_long)
    }

    /// Returns whether the stored value is empty.
    #[inline]
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }
}

impl<T: Storable> Handler<T> for BytesLikeHandler<'_, T> {
    #[inline]
    fn read(&self) -> Result<T> {
        self.as_slot().read()
    }
    #[inline]
    fn write(&mut self, value: T) -> Result<()> {
        self.as_slot().write(value)
    }
    #[inline]
    fn delete(&mut self) -> Result<()> {
        self.as_slot().delete()
    }
    #[inline]
    fn t_read(&self) -> Result<T> {
        self.as_slot().t_read()
    }
    #[inline]
    fn t_write(&mut self, value: T) -> Result<()> {
        self.as_slot().t_write(value)
    }
    #[inline]
    fn t_delete(&mut self) -> Result<()> {
        self.as_slot().t_delete()
    }
}

impl Storable for Bytes {
    #[inline]
    fn load<S: StorageOps>(storage: &S, slot: U256, ctx: LayoutCtx) -> Result<Self> {
        debug_assert_eq!(ctx, LayoutCtx::FULL, "Bytes cannot be packed");
        load_bytes_like(storage, slot, |data| Ok(Self::from(data)))
    }

    #[inline]
    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, ctx: LayoutCtx) -> Result<()> {
        debug_assert_eq!(ctx, LayoutCtx::FULL, "Bytes cannot be packed");
        store_bytes_like(self.as_ref(), storage, slot)
    }

    #[inline]
    fn delete<S: StorageOps>(storage: &mut S, slot: U256, ctx: LayoutCtx) -> Result<()> {
        debug_assert_eq!(ctx, LayoutCtx::FULL, "Bytes cannot be packed");
        delete_bytes_like(storage, slot)
    }
}

impl Storable for String {
    #[inline]
    fn load<S: StorageOps>(storage: &S, slot: U256, ctx: LayoutCtx) -> Result<Self> {
        debug_assert_eq!(ctx, LayoutCtx::FULL, "String cannot be packed");
        load_bytes_like(storage, slot, |data| {
            Self::from_utf8(data).map_err(|e| {
                BasePrecompileError::Fatal(format!("Invalid UTF-8 in stored string: {e}"))
            })
        })
    }

    #[inline]
    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, ctx: LayoutCtx) -> Result<()> {
        debug_assert_eq!(ctx, LayoutCtx::FULL, "String cannot be packed");
        store_bytes_like(self.as_bytes(), storage, slot)
    }

    #[inline]
    fn delete<S: StorageOps>(storage: &mut S, slot: U256, ctx: LayoutCtx) -> Result<()> {
        debug_assert_eq!(ctx, LayoutCtx::FULL, "String cannot be packed");
        delete_bytes_like(storage, slot)
    }
}

impl OnlyPrimitives for String {}

impl StorageKey for String {
    #[inline]
    fn as_storage_bytes(&self) -> impl AsRef<[u8]> {
        self.as_bytes()
    }

    #[inline]
    fn mapping_slot(&self, slot: U256) -> U256 {
        let mut buf = Vec::with_capacity(self.len() + 32);
        buf.extend_from_slice(self.as_bytes());
        buf.extend_from_slice(&slot.to_be_bytes::<32>());
        U256::from_be_bytes(keccak256(buf).0)
    }
}

// -- HELPER FUNCTIONS ---------------------------------------------------------

#[inline]
fn load_bytes_like<T, S, F>(storage: &S, base_slot: U256, into: F) -> Result<T>
where
    S: StorageOps,
    F: FnOnce(Vec<u8>) -> Result<T>,
{
    let base_value = storage.load(base_slot)?;
    let is_long = is_long_string(base_value);
    let length = calc_string_length(base_value, is_long)?;

    if is_long {
        let slot_start = calc_data_slot(base_slot);
        let chunks = calc_chunks(length);
        let mut data = Vec::new();

        for i in 0..chunks {
            let slot =
                slot_start.checked_add(U256::from(i)).ok_or(BasePrecompileError::SlotOverflow)?;
            let chunk_value = storage.load(slot)?;
            let chunk_bytes = chunk_value.to_be_bytes::<32>();
            let bytes_to_take = if i == chunks - 1 { length - (i * 32) } else { 32 };
            data.extend_from_slice(&chunk_bytes[..bytes_to_take]);
        }

        into(data)
    } else {
        let bytes = base_value.to_be_bytes::<32>();
        into(bytes[..length].to_vec())
    }
}

#[inline]
fn store_bytes_like<S: StorageOps>(bytes: &[u8], storage: &mut S, base_slot: U256) -> Result<()> {
    let length = bytes.len();
    if length <= 31 {
        storage.store(base_slot, encode_short_string(bytes))
    } else {
        storage.store(base_slot, encode_long_string_length(length))?;
        let slot_start = calc_data_slot(base_slot);
        let chunks = calc_chunks(length);

        for i in 0..chunks {
            let slot =
                slot_start.checked_add(U256::from(i)).ok_or(BasePrecompileError::SlotOverflow)?;
            let chunk_start = i * 32;
            let chunk_end = (chunk_start + 32).min(length);
            let chunk = &bytes[chunk_start..chunk_end];
            let mut chunk_bytes = [0u8; 32];
            chunk_bytes[..chunk.len()].copy_from_slice(chunk);
            storage.store(slot, U256::from_be_bytes(chunk_bytes))?;
        }

        Ok(())
    }
}

#[inline]
fn delete_bytes_like<S: StorageOps>(storage: &mut S, base_slot: U256) -> Result<()> {
    let base_value = storage.load(base_slot)?;
    let is_long = is_long_string(base_value);

    if is_long {
        let length = calc_string_length(base_value, true)?;
        let slot_start = calc_data_slot(base_slot);
        let chunks = calc_chunks(length);
        for i in 0..chunks {
            storage.store(
                slot_start.checked_add(U256::from(i)).ok_or(BasePrecompileError::SlotOverflow)?,
                U256::ZERO,
            )?;
        }
    }

    storage.store(base_slot, U256::ZERO)
}

#[inline]
fn calc_data_slot(base_slot: U256) -> U256 {
    U256::from_be_bytes(keccak256(base_slot.to_be_bytes::<32>()).0)
}

#[inline]
const fn is_long_string(slot_value: U256) -> bool {
    (slot_value.as_limbs()[0] as u8 & 1) != 0
}

#[inline]
fn calc_string_length(slot_value: U256, is_long: bool) -> Result<usize> {
    if is_long {
        let length_times_two: U256 = slot_value - U256::ONE;
        let length_u256: U256 = length_times_two >> 1;
        if length_u256 > U256::from(u32::MAX) {
            return Err(BasePrecompileError::under_overflow());
        }
        Ok(length_u256.to::<usize>())
    } else {
        let bytes = slot_value.to_be_bytes::<32>();
        let length = (bytes[31] / 2) as usize;
        if length > 31 {
            return Err(BasePrecompileError::Fatal(format!(
                "short string length {length} exceeds maximum of 31 bytes"
            )));
        }
        Ok(length)
    }
}

#[inline]
const fn calc_chunks(byte_length: usize) -> usize {
    byte_length.div_ceil(32)
}

#[inline]
fn encode_short_string(bytes: &[u8]) -> U256 {
    let mut storage_bytes = [0u8; 32];
    storage_bytes[..bytes.len()].copy_from_slice(bytes);
    storage_bytes[31] = (bytes.len() * 2) as u8;
    U256::from_be_bytes(storage_bytes)
}

#[inline]
fn encode_long_string_length(byte_length: usize) -> U256 {
    U256::from(byte_length * 2 + 1)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::{hashmap::setup_storage, provider::Handler, storage_ctx::StorageCtx};

    fn arb_safe_slot() -> impl Strategy<Value = U256> {
        any::<[u64; 4]>()
            .prop_map(|limbs| U256::from_limbs(limbs) % (U256::MAX - U256::from(10000u64)))
    }

    fn arb_short_string() -> impl Strategy<Value = String> {
        prop_oneof![
            Just(String::new()),
            "[a-zA-Z0-9]{1,31}",
            "[\u{0041}-\u{005A}\u{4E00}-\u{4E19}]{1,10}",
        ]
    }

    fn arb_32byte_string() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9]{32}"
    }

    fn arb_long_string() -> impl Strategy<Value = String> {
        prop_oneof!["[a-zA-Z0-9]{33,100}", "[\u{0041}-\u{005A}\u{4E00}-\u{4E19}]{11,30}",]
    }

    fn arb_short_bytes() -> impl Strategy<Value = Bytes> {
        prop::collection::vec(any::<u8>(), 0..=31).prop_map(Bytes::from)
    }

    fn arb_long_bytes() -> impl Strategy<Value = Bytes> {
        prop::collection::vec(any::<u8>(), 33..=100).prop_map(Bytes::from)
    }

    #[test]
    fn test_calc_data_slot_matches_manual_keccak() {
        let base_slot = U256::from(42u64);
        let data_slot = calc_data_slot(base_slot);
        let expected = U256::from_be_bytes(keccak256(base_slot.to_be_bytes::<32>()).0);
        assert_eq!(data_slot, expected);
    }

    #[test]
    fn test_is_long_string_boundaries() {
        let short_31_bytes = encode_short_string(&[b'a'; 31]);
        assert!(!is_long_string(short_31_bytes));

        let long_32_bytes = encode_long_string_length(32);
        assert!(is_long_string(long_32_bytes));

        let empty = encode_short_string(&[]);
        assert!(!is_long_string(empty));
    }

    #[test]
    fn test_calc_chunks_boundaries() {
        assert_eq!(calc_chunks(0), 0);
        assert_eq!(calc_chunks(1), 1);
        assert_eq!(calc_chunks(32), 1);
        assert_eq!(calc_chunks(33), 2);
        assert_eq!(calc_chunks(64), 2);
        assert_eq!(calc_chunks(65), 3);
    }

    #[test]
    fn test_calc_string_length_tampered() {
        let malicious_slot = U256::from(0x0008000000000001u64);
        assert!(is_long_string(malicious_slot));
        assert_eq!(
            calc_string_length(malicious_slot, true),
            Err(BasePrecompileError::under_overflow())
        );

        let at_max = U256::from(u32::MAX as u64 * 2 + 1);
        assert_eq!(calc_string_length(at_max, true), Ok(u32::MAX as usize));

        let above_max = U256::from((u32::MAX as u64 + 1) * 2 + 1);
        assert_eq!(calc_string_length(above_max, true), Err(BasePrecompileError::under_overflow()));

        let malicious_short = U256::from(0xFEu64);
        assert!(!is_long_string(malicious_short));
        assert!(calc_string_length(malicious_short, false).is_err());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        #[test]
        fn test_short_strings(s in arb_short_string(), base_slot in arb_safe_slot()) {
            let (mut storage, address) = setup_storage();
            StorageCtx::enter(&mut storage, |ctx| {
                let mut slot = BytesLikeHandler::<String>::new(base_slot, address, ctx);
                slot.write(s.clone()).unwrap();
                let loaded = slot.read().unwrap();
                prop_assert_eq!(&s, &loaded);
                slot.delete().unwrap();
                let after = slot.read().unwrap();
                prop_assert_eq!(after, String::new());
                Ok(())
            }).unwrap();
        }

        #[test]
        fn test_long_strings(s in arb_long_string(), base_slot in arb_safe_slot()) {
            let (mut storage, address) = setup_storage();
            StorageCtx::enter(&mut storage, |ctx| {
                let mut slot = BytesLikeHandler::<String>::new(base_slot, address, ctx);
                slot.write(s.clone()).unwrap();
                let loaded = slot.read().unwrap();
                prop_assert_eq!(&s, &loaded);
                slot.delete().unwrap();
                let after = slot.read().unwrap();
                prop_assert_eq!(after, String::new());
                Ok(())
            }).unwrap();
        }

        #[test]
        fn test_short_bytes(b in arb_short_bytes(), base_slot in arb_safe_slot()) {
            let (mut storage, address) = setup_storage();
            StorageCtx::enter(&mut storage, |ctx| {
                let mut slot = BytesLikeHandler::<Bytes>::new(base_slot, address, ctx);
                slot.write(b.clone()).unwrap();
                let loaded = slot.read().unwrap();
                prop_assert_eq!(&b, &loaded);
                Ok(())
            }).unwrap();
        }

        #[test]
        fn test_long_bytes(b in arb_long_bytes(), base_slot in arb_safe_slot()) {
            let (mut storage, address) = setup_storage();
            StorageCtx::enter(&mut storage, |ctx| {
                let mut slot = BytesLikeHandler::<Bytes>::new(base_slot, address, ctx);
                slot.write(b.clone()).unwrap();
                let loaded = slot.read().unwrap();
                prop_assert_eq!(&b, &loaded);
                Ok(())
            }).unwrap();
        }

        #[test]
        fn test_32byte_strings(s in arb_32byte_string(), base_slot in arb_safe_slot()) {
            let (mut storage, address) = setup_storage();
            StorageCtx::enter(&mut storage, |ctx| {
                let mut slot = BytesLikeHandler::<String>::new(base_slot, address, ctx);
                slot.write(s.clone()).unwrap();
                let loaded = slot.read().unwrap();
                prop_assert_eq!(&s, &loaded);
                Ok(())
            }).unwrap();
        }
    }
}
