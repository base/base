use alloc::{borrow::Cow, vec::Vec};
use core::{cmp::Ordering, fmt, hash, hint::cold_path, marker::PhantomData};

use alloy_primitives::hex;

/// A table of valid `jump` destinations.
///
/// It is immutable and memory efficient, with one bit per byte in the bytecode.
#[derive(Clone)]
pub struct JumpTable {
    table: Cow<'static, [u8]>,
    bit_len: usize,
}

/// Borrowed table of valid `jump` destinations.
#[derive(Clone, Copy)]
pub struct JumpTableRef<'a> {
    base: *const u8,
    bit_len: usize,
    _marker: PhantomData<&'a [u8]>,
}

impl fmt::Debug for JumpTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JumpTable").field("map", &hex::encode(self.as_slice())).finish()
    }
}

impl fmt::Debug for JumpTableRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JumpTableRef").field("map", &hex::encode(self.as_slice())).finish()
    }
}

impl PartialEq for JumpTable {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.as_slice().eq(other.as_slice())
    }
}

impl Eq for JumpTable {}

impl PartialOrd for JumpTable {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for JumpTable {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_slice().cmp(other.as_slice())
    }
}

impl hash::Hash for JumpTable {
    #[inline]
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}

impl Default for JumpTable {
    #[inline]
    fn default() -> Self {
        Self::from_vec(Vec::new(), 0)
    }
}

impl JumpTable {
    #[inline]
    pub(crate) fn new(bit_len: usize) -> Self {
        let bytes = bit_len.div_ceil(8);
        Self::from_vec(alloc::vec![0; bytes], bit_len)
    }

    #[inline]
    pub(crate) fn set(&mut self, pc: usize) {
        debug_assert!(pc < self.bit_len, "jump table bit index exceeds bit length");
        let (byte, bit) = (pc / 8, pc % 8);
        // SAFETY: callers only set PCs inside the bytecode length, and debug
        // builds assert the bit length above.
        unsafe {
            *self.table.to_mut().get_unchecked_mut(byte) |= 1 << bit;
        }
    }

    /// Constructs a jump map from raw bytes and length.
    ///
    /// Bit length represents number of used bits inside slice.
    ///
    /// # Panics
    ///
    /// Panics if number of bits in slice is less than bit_len.
    #[inline]
    pub fn from_slice(slice: &[u8], bit_len: usize) -> Self {
        Self::size_assert(slice.len(), bit_len);
        Self::from_vec(slice.to_vec(), bit_len)
    }

    #[inline]
    fn from_vec(slice: Vec<u8>, bit_len: usize) -> Self {
        #[cfg(debug_assertions)]
        Self::size_assert(slice.len(), bit_len);
        Self { table: slice.into(), bit_len }
    }

    /// Constructs a jump map from raw bytes and length.
    ///
    /// Bit length represents number of used bits inside slice.
    ///
    /// # Panics
    ///
    /// Panics if number of bits in slice is less than bit_len.
    #[inline]
    pub const fn from_static_slice(slice: &'static [u8], bit_len: usize) -> Self {
        Self::size_assert(slice.len(), bit_len);
        Self { table: Cow::Borrowed(slice), bit_len }
    }

    #[inline]
    pub(crate) const fn size_assert(len: usize, bit_len: usize) {
        const BYTE_LEN: usize = 8;
        assert!(len * BYTE_LEN >= bit_len, "slice bit length is less than bit_len");
    }

    /// Gets the raw bytes of the jump map.
    #[inline]
    pub const fn as_slice(&self) -> &[u8] {
        self.as_ref().as_slice()
    }

    /// Gets the bit length of the jump map.
    #[inline]
    pub const fn len(&self) -> usize {
        self.as_ref().len()
    }

    /// Returns true if the jump map is empty.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.as_ref().is_empty()
    }

    /// Checks if `pc` is a valid jump destination.
    #[inline]
    pub const fn is_valid(&self, pc: usize) -> bool {
        self.as_ref().is_valid(pc)
    }

    /// Returns the borrowed jump map.
    #[inline]
    pub const fn as_ref(&self) -> JumpTableRef<'_> {
        let base = match &self.table {
            Cow::Borrowed(t) => t.as_ptr(),
            Cow::Owned(t) => t.as_ptr(),
        };
        JumpTableRef { base, bit_len: self.bit_len, _marker: PhantomData }
    }
}

impl<'a> JumpTableRef<'a> {
    /// Gets the raw bytes of the jump map.
    #[inline]
    pub const fn as_slice(&self) -> &'a [u8] {
        // SAFETY: only constructed from a valid `JumpMap`.
        unsafe { core::slice::from_raw_parts(self.base, self.bit_len.div_ceil(8)) }
    }

    /// Gets the bit length of the jump map.
    #[inline]
    pub const fn len(&self) -> usize {
        self.bit_len
    }

    /// Returns true if the jump map is empty.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Checks if `pc` is a valid jump destination.
    #[inline]
    pub const fn is_valid(&self, pc: usize) -> bool {
        if pc >= self.bit_len {
            cold_path();
            return false;
        }
        let (byte, bit) = (pc / 8, pc % 8);
        // SAFETY: `pc` is checked to be within `self.bit_len` above.
        unsafe { *self.base.add(byte) & (1 << bit) != 0 }
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;

    use super::*;

    #[test]
    #[should_panic(expected = "slice bit length is less than bit_len")]
    fn test_jump_table_from_slice_panic() {
        let _ = JumpTable::from_slice(&[0x00], 10);
    }

    #[test]
    fn test_jump_table_from_slice() {
        let jump_table = JumpTable::from_slice(&[0x00], 3);
        assert_eq!(jump_table.len(), 3);
    }

    #[test]
    fn test_jump_table_is_valid() {
        let jump_table = JumpTable::from_slice(&[0x0D, 0x06], 13);

        assert_eq!(jump_table.len(), 13);

        assert!(jump_table.is_valid(0));
        assert!(!jump_table.is_valid(1));
        assert!(jump_table.is_valid(2));
        assert!(jump_table.is_valid(3));
        assert!(!jump_table.is_valid(4));
        assert!(!jump_table.is_valid(5));
        assert!(!jump_table.is_valid(6));
        assert!(!jump_table.is_valid(7));
        assert!(!jump_table.is_valid(8));
        assert!(jump_table.is_valid(9));
        assert!(jump_table.is_valid(10));
        assert!(!jump_table.is_valid(11));
        assert!(!jump_table.is_valid(12));
    }

    #[test]
    fn test_jump_table_ref_is_valid() {
        let jump_table = JumpTable::from_slice(&[0x0D, 0x06], 13);
        let jump_table_ref = jump_table.as_ref();

        assert_eq!(jump_table_ref.as_slice(), &[0x0D, 0x06]);
        assert_eq!(jump_table_ref.len(), 13);
        assert!(!jump_table_ref.is_empty());

        assert!(jump_table_ref.is_valid(0));
        assert!(!jump_table_ref.is_valid(1));
        assert!(jump_table_ref.is_valid(2));
        assert!(jump_table_ref.is_valid(3));
        assert!(!jump_table_ref.is_valid(4));
        assert!(!jump_table_ref.is_valid(5));
        assert!(!jump_table_ref.is_valid(6));
        assert!(!jump_table_ref.is_valid(7));
        assert!(!jump_table_ref.is_valid(8));
        assert!(jump_table_ref.is_valid(9));
        assert!(jump_table_ref.is_valid(10));
        assert!(!jump_table_ref.is_valid(11));
        assert!(!jump_table_ref.is_valid(12));
    }

    #[test]
    fn test_jump_table_debug_prints_hex_map() {
        let jump_table = JumpTable::from_slice(&[0x0D, 0x06], 13);

        assert_eq!(format!("{jump_table:?}"), "JumpTable { map: \"0d06\" }");
        assert_eq!(format!("{:?}", jump_table.as_ref()), "JumpTableRef { map: \"0d06\" }");
    }
}
