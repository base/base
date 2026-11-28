//! Plain helpers for the Solidity storage layouts the Base native precompiles use.
//!
//! [`StorageLayout`] derives slots (ERC-7201 namespace fields, mapping entries, dynamic data) and
//! packs fields into words; the [`StorageCtx`] methods here read and write values at those slots.
//! Precompile storage runs in consensus, so each method's SLOAD/SSTORE sequence (and therefore its
//! gas) is part of its contract and is documented on it.

use alloc::{format, string::String, vec::Vec};

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};

use crate::{
    error::{BasePrecompileError, Result},
    neutral::Bytecode,
    storage_ctx::StorageCtx,
};

/// Slot derivation and word packing for Solidity storage layouts.
#[derive(Debug)]
pub struct StorageLayout;

impl StorageLayout {
    /// Bytes of a `uint64` field or dynamic-array element.
    pub const U64_BYTES: usize = 8;

    /// Returns the ERC-7201 root of namespace `id`: `keccak256(keccak256(id) - 1) & ~0xff`.
    pub fn erc7201_root(id: &str) -> U256 {
        let inner = U256::from_be_bytes(keccak256(id.as_bytes()).0) - U256::ONE;
        U256::from_be_bytes(keccak256(inner.to_be_bytes::<32>()).0) & !U256::from(0xff)
    }

    /// Returns the slot of field `offset` in a layout rooted at `root`.
    ///
    /// ERC-7201 roots end in a zero byte, so offsets below 256 never carry.
    pub const fn field(root: U256, offset: u64) -> U256 {
        let mut limbs = *root.as_limbs();
        limbs[0] += offset;
        U256::from_limbs(limbs)
    }

    /// Returns the slot of `mapping[key]` for a value-type key of at most 32 big-endian bytes:
    /// `keccak256(lpad32(key) ‖ slot)`, Solidity's derivation for unsigned integers, addresses and
    /// `bytes32`.
    pub fn mapping(key: &[u8], slot: U256) -> U256 {
        let mut buf = [0u8; 64];
        buf[32 - key.len()..32].copy_from_slice(key);
        buf[32..].copy_from_slice(&slot.to_be_bytes::<32>());
        U256::from_be_bytes(keccak256(buf).0)
    }

    /// Returns the slot of `mapping[key]` for a string key: `keccak256(bytes(key) ‖ slot)`.
    pub fn string_mapping(key: &str, slot: U256) -> U256 {
        let mut buf = Vec::with_capacity(key.len() + 32);
        buf.extend_from_slice(key.as_bytes());
        buf.extend_from_slice(&slot.to_be_bytes::<32>());
        U256::from_be_bytes(keccak256(buf).0)
    }

    /// Returns the first data slot of the long string or dynamic array at `slot`:
    /// `keccak256(slot)`.
    pub fn dynamic_data(slot: U256) -> U256 {
        U256::from_be_bytes(keccak256(slot.to_be_bytes::<32>()).0)
    }

    /// Returns `slot + index`.
    pub fn nth(slot: U256, index: usize) -> Result<U256> {
        slot.checked_add(U256::from(index)).ok_or(BasePrecompileError::SlotOverflow)
    }

    /// Returns the mask covering the low `bytes` bytes of a word.
    pub fn mask(bytes: usize) -> U256 {
        if bytes >= 32 { U256::MAX } else { (U256::ONE << (bytes * 8)) - U256::ONE }
    }

    /// Returns the `bytes`-wide field at byte `offset` (counted from the low end) of `word`.
    pub fn extract(word: U256, offset: usize, bytes: usize) -> U256 {
        (word >> (offset * 8)) & Self::mask(bytes)
    }

    /// Returns `word` with the `bytes`-wide field at byte `offset` replaced by `value`.
    pub fn insert(word: U256, offset: usize, bytes: usize, value: U256) -> U256 {
        let mask = Self::mask(bytes);
        (word & !(mask << (offset * 8))) | ((value & mask) << (offset * 8))
    }

    /// Returns the byte length of the Solidity string whose head word is `head`.
    ///
    /// The head holds `len * 2 + 1` for strings of 32 bytes or more (the data then lives at
    /// [`Self::dynamic_data`]), and otherwise the data left-aligned with `len * 2` in its last
    /// byte.
    pub fn string_len(head: U256) -> Result<usize> {
        if Self::is_long_string(head) {
            let len: U256 = (head - U256::ONE) >> 1;
            if len > U256::from(u32::MAX) {
                return Err(BasePrecompileError::under_overflow());
            }
            Ok(len.to::<usize>())
        } else {
            let len = (head.to_be_bytes::<32>()[31] / 2) as usize;
            if len > 31 {
                return Err(BasePrecompileError::Fatal(format!(
                    "short string length {len} exceeds maximum of 31 bytes"
                )));
            }
            Ok(len)
        }
    }

    /// Returns whether `head` encodes a long (32 bytes or more) Solidity string.
    pub const fn is_long_string(head: U256) -> bool {
        head.as_limbs()[0] & 1 != 0
    }

    /// Returns the length of the dynamic array whose length word is `word`.
    pub fn array_len(word: U256) -> Result<usize> {
        if word > U256::from(u32::MAX) {
            return Err(BasePrecompileError::under_overflow());
        }
        Ok(word.to::<usize>())
    }
}

/// A value stored right-aligned in a full storage word.
pub trait StorageValue: Sized {
    /// Encodes the value as a word.
    fn to_word(self) -> U256;
    /// Decodes a word, rejecting values out of the type's range.
    fn from_word(word: U256) -> Result<Self>;
}

macro_rules! uint_storage_value {
    ($($ty:ty),*) => {$(
        impl StorageValue for $ty {
            fn to_word(self) -> U256 {
                U256::from(self)
            }
            fn from_word(word: U256) -> Result<Self> {
                word.try_into().map_err(|_| BasePrecompileError::under_overflow())
            }
        }
    )*};
}
uint_storage_value!(u8, u32, u64, u128);

impl StorageValue for U256 {
    fn to_word(self) -> U256 {
        self
    }
    fn from_word(word: U256) -> Result<Self> {
        Ok(word)
    }
}

impl StorageValue for bool {
    fn to_word(self) -> U256 {
        U256::from(self as u8)
    }
    fn from_word(word: U256) -> Result<Self> {
        match word.as_limbs() {
            [0, 0, 0, 0] => Ok(false),
            [1, 0, 0, 0] => Ok(true),
            _ => Err(BasePrecompileError::enum_conversion_error()),
        }
    }
}

impl StorageValue for Address {
    fn to_word(self) -> U256 {
        U256::from_be_slice(self.as_slice())
    }
    fn from_word(word: U256) -> Result<Self> {
        Ok(Self::from_slice(&word.to_be_bytes::<32>()[12..]))
    }
}

impl StorageValue for B256 {
    fn to_word(self) -> U256 {
        U256::from_be_bytes(self.0)
    }
    fn from_word(word: U256) -> Result<Self> {
        Ok(Self::from(word.to_be_bytes::<32>()))
    }
}

impl StorageCtx<'_> {
    /// Reads the full-word value at `slot`: one SLOAD.
    pub fn read<T: StorageValue>(&self, address: Address, slot: U256) -> Result<T> {
        T::from_word(self.sload(address, slot)?)
    }

    /// Writes a full-word value to `slot`: one SSTORE.
    pub fn write<T: StorageValue>(&self, address: Address, slot: U256, value: T) -> Result<()> {
        self.sstore(address, slot, value.to_word())
    }

    /// Reads the `bytes`-wide field at byte `offset` of `slot`: one SLOAD.
    pub fn read_packed(
        &self,
        address: Address,
        slot: U256,
        offset: usize,
        bytes: usize,
    ) -> Result<U256> {
        Ok(StorageLayout::extract(self.sload(address, slot)?, offset, bytes))
    }

    /// Writes the `bytes`-wide field at byte `offset` of `slot`, keeping the rest of the word: an
    /// SLOAD, then an SSTORE of the merged word.
    pub fn write_packed(
        &self,
        address: Address,
        slot: U256,
        offset: usize,
        bytes: usize,
        value: U256,
    ) -> Result<()> {
        let word = self.sload(address, slot)?;
        self.sstore(address, slot, StorageLayout::insert(word, offset, bytes, value))
    }

    /// Reads the Solidity string at `slot`: the head SLOAD, then one SLOAD per 32-byte data chunk
    /// of a long string.
    pub fn read_string(&self, address: Address, slot: U256) -> Result<String> {
        let head = self.sload(address, slot)?;
        let len = StorageLayout::string_len(head)?;
        let data = if StorageLayout::is_long_string(head) {
            let start = StorageLayout::dynamic_data(slot);
            let mut data = Vec::new();
            for i in 0..len.div_ceil(32) {
                let chunk = self.sload(address, StorageLayout::nth(start, i)?)?.to_be_bytes::<32>();
                data.extend_from_slice(&chunk[..(len - i * 32).min(32)]);
            }
            data
        } else {
            head.to_be_bytes::<32>()[..len].to_vec()
        };
        String::from_utf8(data).map_err(|e| {
            BasePrecompileError::Fatal(format!("Invalid UTF-8 in stored string: {e}"))
        })
    }

    /// Writes the Solidity string at `slot`.
    ///
    /// With [`tail cleanup`] (Cobalt), first checks that writes are allowed, SLOADs the old head
    /// and, if the old value was a long string with more chunks than the new one, zeroes the
    /// retired chunks. Then SSTOREs the head and, for a long string, each chunk.
    ///
    /// [`tail cleanup`]: crate::StorageFeatures::dynamic_storage_tail_cleanup_enabled
    pub fn write_string(&self, address: Address, slot: U256, value: &str) -> Result<()> {
        let bytes = value.as_bytes();
        let new_chunks = if bytes.len() <= 31 { 0 } else { bytes.len().div_ceil(32) };
        let start = StorageLayout::dynamic_data(slot);

        if self.storage_features().dynamic_storage_tail_cleanup_enabled() {
            // Charged on every write so a shrink is detected, also over values written pre-Cobalt.
            self.ensure_writable()?;
            let old_head = self.sload(address, slot)?;
            if StorageLayout::is_long_string(old_head) {
                let old_chunks = StorageLayout::string_len(old_head)?.div_ceil(32);
                for i in new_chunks..old_chunks {
                    self.sstore(address, StorageLayout::nth(start, i)?, U256::ZERO)?;
                }
            }
        }

        if bytes.len() <= 31 {
            let mut head = [0u8; 32];
            head[..bytes.len()].copy_from_slice(bytes);
            head[31] = (bytes.len() * 2) as u8;
            return self.sstore(address, slot, U256::from_be_bytes(head));
        }
        self.sstore(address, slot, U256::from(bytes.len() * 2 + 1))?;
        for (i, chunk) in bytes.chunks(32).enumerate() {
            let mut word = [0u8; 32];
            word[..chunk.len()].copy_from_slice(chunk);
            self.sstore(address, StorageLayout::nth(start, i)?, U256::from_be_bytes(word))?;
        }
        Ok(())
    }

    /// Deletes the Solidity string at `slot`: the head SLOAD, a zero SSTORE per chunk of a long
    /// string, then a zero SSTORE of the head.
    pub fn delete_string(&self, address: Address, slot: U256) -> Result<()> {
        let head = self.sload(address, slot)?;
        if StorageLayout::is_long_string(head) {
            let start = StorageLayout::dynamic_data(slot);
            for i in 0..StorageLayout::string_len(head)?.div_ceil(32) {
                self.sstore(address, StorageLayout::nth(start, i)?, U256::ZERO)?;
            }
        }
        self.sstore(address, slot, U256::ZERO)
    }

    /// Reads the Solidity `uint64[]` at `slot`: the length SLOAD, then one SLOAD per data word
    /// (four elements per word, the first in the low bytes).
    pub fn read_u64_array(&self, address: Address, slot: U256) -> Result<Vec<u64>> {
        let len = StorageLayout::array_len(self.sload(address, slot)?)?;
        let start = StorageLayout::dynamic_data(slot);
        let mut values = Vec::new();
        for i in 0..len.div_ceil(4) {
            let word = self.sload(address, StorageLayout::nth(start, i)?)?;
            for lane in 0..(len - i * 4).min(4) {
                let value = StorageLayout::extract(word, lane * 8, StorageLayout::U64_BYTES);
                values.push(value.to::<u64>());
            }
        }
        Ok(values)
    }

    /// Writes the Solidity `uint64[]` at `slot`.
    ///
    /// With [`tail cleanup`] (Cobalt), first checks that writes are allowed, SLOADs the old length
    /// and zeroes the data words only the old array used. Then SSTOREs the length and each data
    /// word, rebuilt from zero.
    ///
    /// [`tail cleanup`]: crate::StorageFeatures::dynamic_storage_tail_cleanup_enabled
    pub fn write_u64_array(&self, address: Address, slot: U256, values: &[u64]) -> Result<()> {
        let start = StorageLayout::dynamic_data(slot);
        let new_words = values.len().div_ceil(4);
        if self.storage_features().dynamic_storage_tail_cleanup_enabled() {
            // Charged on every write so a shrink is detected, also over values written pre-Cobalt.
            self.ensure_writable()?;
            let old_len = StorageLayout::array_len(self.sload(address, slot)?)?;
            for i in new_words..old_len.div_ceil(4) {
                self.sstore(address, StorageLayout::nth(start, i)?, U256::ZERO)?;
            }
        }
        self.sstore(address, slot, U256::from(values.len()))?;
        for (i, chunk) in values.chunks(4).enumerate() {
            let word = chunk.iter().enumerate().fold(U256::ZERO, |word, (lane, value)| {
                StorageLayout::insert(word, lane * 8, StorageLayout::U64_BYTES, U256::from(*value))
            });
            self.sstore(address, StorageLayout::nth(start, i)?, word)?;
        }
        Ok(())
    }

    /// Returns whether `address` has non-empty code, the mark of an initialized precompile.
    pub fn has_bytecode(&self, address: Address) -> Result<bool> {
        self.with_account_info(address, |info| Ok(!info.is_empty_code_hash()))
    }

    /// Deploys the one-byte `0xef` marker code that marks `address` as initialized.
    pub fn set_marker_code(&self, address: Address) -> Result<()> {
        self.set_code(address, Bytecode::new_legacy(Bytes::from_static(&[0xef])))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{U256, address, uint};

    use super::*;
    use crate::{HashMapStorageProvider, StorageFeatures};

    const ACCOUNT: Address = address!("0x0000000000000000000000000000000000000042");
    const SLOT: U256 = uint!(7_U256);
    /// Each storage feature set and whether it zeroes the tail a shrinking write leaves behind.
    const FEATURES_CLEARING_TAIL: [(StorageFeatures, bool); 2] =
        [(StorageFeatures::Legacy, false), (StorageFeatures::Cobalt, true)];

    fn with_ctx<R>(features: StorageFeatures, f: impl FnOnce(StorageCtx<'_>) -> R) -> R {
        let mut storage = HashMapStorageProvider::new_with_storage_features(1, features);
        StorageCtx::enter(&mut storage, f)
    }

    #[test]
    fn strings_roundtrip_short_and_long() {
        for features in [StorageFeatures::Legacy, StorageFeatures::Cobalt] {
            with_ctx(features, |ctx| {
                for value in ["", "abc", &"x".repeat(31), &"y".repeat(32), &"z".repeat(100)] {
                    ctx.write_string(ACCOUNT, SLOT, value).unwrap();
                    assert_eq!(ctx.read_string(ACCOUNT, SLOT).unwrap(), value);
                }
            });
        }
    }

    #[test]
    fn cobalt_string_shrink_clears_retired_chunks() {
        let data = StorageLayout::dynamic_data(SLOT);
        for (features, cleared) in FEATURES_CLEARING_TAIL {
            with_ctx(features, |ctx| {
                ctx.write_string(ACCOUNT, SLOT, &"a".repeat(70)).unwrap();
                ctx.write_string(ACCOUNT, SLOT, &"b".repeat(33)).unwrap();
                assert_eq!(ctx.sload(ACCOUNT, data + U256::from(2)).unwrap().is_zero(), cleared);
                ctx.delete_string(ACCOUNT, SLOT).unwrap();
                assert_eq!(ctx.sload(ACCOUNT, SLOT).unwrap(), U256::ZERO);
                assert_eq!(ctx.sload(ACCOUNT, data).unwrap(), U256::ZERO);
                assert_eq!(ctx.sload(ACCOUNT, data + U256::ONE).unwrap(), U256::ZERO);
            });
        }
    }

    #[test]
    fn short_string_uses_solidity_encoding() {
        with_ctx(StorageFeatures::Legacy, |ctx| {
            ctx.write_string(ACCOUNT, SLOT, "USD").unwrap();
            let mut word = [0u8; 32];
            word[..3].copy_from_slice(b"USD");
            word[31] = 6;
            assert_eq!(ctx.sload(ACCOUNT, SLOT).unwrap(), U256::from_be_bytes(word));
        });
    }

    #[test]
    fn u64_arrays_pack_four_per_word_and_clear_tail_at_cobalt() {
        let data = StorageLayout::dynamic_data(SLOT);
        for (features, cleared) in FEATURES_CLEARING_TAIL {
            with_ctx(features, |ctx| {
                let values = [1u64, 2, 3, 4, 5, u64::MAX];
                ctx.write_u64_array(ACCOUNT, SLOT, &values).unwrap();
                assert_eq!(ctx.read_u64_array(ACCOUNT, SLOT).unwrap(), values);
                let first = U256::from(1) | U256::from(2) << 64 | U256::from(3) << 128;
                assert_eq!(ctx.sload(ACCOUNT, data).unwrap(), first | U256::from(4) << 192);

                ctx.write_u64_array(ACCOUNT, SLOT, &[9]).unwrap();
                assert_eq!(ctx.read_u64_array(ACCOUNT, SLOT).unwrap(), [9]);
                assert_eq!(ctx.sload(ACCOUNT, data).unwrap(), U256::from(9));
                assert_eq!(ctx.sload(ACCOUNT, data + U256::ONE).unwrap().is_zero(), cleared);
            });
        }
    }

    #[test]
    fn values_reject_out_of_range_words() {
        with_ctx(StorageFeatures::Legacy, |ctx| {
            ctx.sstore(ACCOUNT, SLOT, U256::from(2)).unwrap();
            assert_eq!(
                ctx.read::<bool>(ACCOUNT, SLOT),
                Err(BasePrecompileError::enum_conversion_error())
            );
            ctx.sstore(ACCOUNT, SLOT, U256::from(u64::MAX) + U256::ONE).unwrap();
            assert_eq!(ctx.read::<u64>(ACCOUNT, SLOT), Err(BasePrecompileError::under_overflow()));
        });
    }

    #[test]
    fn packed_write_keeps_other_fields() {
        with_ctx(StorageFeatures::Legacy, |ctx| {
            ctx.sstore(ACCOUNT, SLOT, U256::MAX).unwrap();
            ctx.write_packed(ACCOUNT, SLOT, 8, 8, U256::from(5)).unwrap();
            assert_eq!(ctx.read_packed(ACCOUNT, SLOT, 8, 8).unwrap(), U256::from(5));
            assert_eq!(ctx.read_packed(ACCOUNT, SLOT, 0, 8).unwrap(), U256::from(u64::MAX));
            assert_eq!(ctx.read_packed(ACCOUNT, SLOT, 16, 16).unwrap(), U256::from(u128::MAX));
        });
    }

    #[test]
    fn string_mapping_hashes_raw_key_bytes() {
        let expected = keccak256([b"key".as_slice(), &SLOT.to_be_bytes::<32>()].concat());
        assert_eq!(StorageLayout::string_mapping("key", SLOT), U256::from_be_bytes(expected.0));
    }
}
