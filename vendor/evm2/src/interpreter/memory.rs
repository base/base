use alloc::vec::Vec;
use core::{cmp::min, fmt, hint::cold_path, ops::Range};

use super::{Gas, InstrStop, Result, Word};
use crate::{utils::num_words, version::GasParams};

/// Linear EVM memory.
pub struct Memory {
    data: Vec<u8>,
    memory_limit: u64,
}

impl Default for Memory {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Memory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Memory").field("len", &self.len()).field("data", &self.data).finish()
    }
}

impl Memory {
    /// Creates memory with the default capacity.
    #[inline]
    pub fn new() -> Self {
        Self::with_capacity(4 * 1024)
    }

    /// Creates memory with the requested capacity.
    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        Self { data: Vec::with_capacity(capacity), memory_limit: u64::MAX }
    }

    /// Returns the memory byte limit.
    #[inline]
    pub const fn memory_limit(&self) -> u64 {
        self.memory_limit
    }

    /// Sets the memory byte limit.
    #[inline]
    pub const fn set_memory_limit(&mut self, limit: u64) {
        self.memory_limit = limit;
    }

    /// Returns the memory length in bytes.
    #[inline]
    pub const fn len(&self) -> usize {
        self.data.len()
    }

    /// Returns all initialized memory bytes.
    #[inline]
    pub const fn as_slice(&self) -> &[u8] {
        self.data.as_slice()
    }

    /// Returns a raw pointer to memory bytes.
    #[inline]
    pub const fn as_mut_ptr(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }

    /// Returns whether memory is empty.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Clears memory while retaining the allocation.
    #[inline]
    pub fn clear(&mut self) {
        self.data.clear();
    }

    #[inline]
    fn resize_to(&mut self, new_size: usize) {
        self.data.resize(new_size, 0);
    }

    /// Resizes memory to cover `offset..offset + len`.
    ///
    /// Does not account for gas.
    #[inline]
    pub fn resize(&mut self, offset: usize, len: usize) -> Result {
        let Some(end) = offset.checked_add(len) else {
            return Err(InstrStop::MemoryOOG);
        };
        if end > self.data.len() {
            self.resize_to(end);
        }
        Ok(())
    }

    /// Resizes memory to cover `offset..offset + len` and charges EVM expansion gas.
    #[inline]
    pub fn resize_evm(
        &mut self,
        gas: &mut Gas,
        gas_params: &GasParams,
        offset: usize,
        len: usize,
    ) -> Result {
        let new_num_words = num_words(offset.saturating_add(len));
        if new_num_words > gas.memory().words_num {
            return self.resize_evm_cold(gas, gas_params, new_num_words);
        }

        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn resize_evm_cold(
        &mut self,
        gas: &mut Gas,
        gas_params: &GasParams,
        new_num_words: usize,
    ) -> Result {
        let Some(new_size) = new_num_words.checked_mul(32) else {
            cold_path();
            return Err(InstrStop::MemoryOOG);
        };

        if self.limit_reached(new_num_words) {
            cold_path();
            return Err(InstrStop::MemoryLimitOOG);
        }

        let cost = gas_params.memory_cost(new_num_words);
        let cost =
            unsafe { gas.memory_mut().set_words_num(new_num_words, cost).unwrap_unchecked() };

        gas.spend(cost).map_err(|_| InstrStop::MemoryOOG)?;
        self.resize_to(new_size);
        Ok(())
    }

    /// Returns whether `new_words` exceeds the memory limit.
    #[inline]
    pub const fn limit_reached(&self, new_words: usize) -> bool {
        new_words.saturating_mul(32) as u64 > self.memory_limit
    }

    #[inline]
    #[cfg_attr(debug_assertions, track_caller)]
    fn slice_range(&self, range: Range<usize>) -> &[u8] {
        match self.data.get(range.clone()) {
            Some(slice) => slice,
            None => debug_unreachable!("slice OOB: {range:?}; len: {}", self.len()),
        }
    }

    #[inline]
    #[cfg_attr(debug_assertions, track_caller)]
    fn slice_range_mut(&mut self, range: Range<usize>) -> &mut [u8] {
        let len = self.len();
        match self.data.get_mut(range.clone()) {
            Some(slice) => slice,
            None => debug_unreachable!("slice OOB: {range:?}; len: {len}"),
        }
    }

    /// Reads a word from memory.
    ///
    /// # Panics
    ///
    /// Panics if the range is out of bounds.
    #[inline]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn get_word(&self, offset: usize) -> Word {
        Word::from_be_slice(self.slice_range(offset..offset + 32))
    }

    /// Writes bytes into memory.
    ///
    /// # Panics
    ///
    /// Panics if the range is out of bounds.
    #[inline]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn set(&mut self, offset: usize, value: &[u8]) {
        if !value.is_empty() {
            self.slice_range_mut(offset..offset + value.len()).copy_from_slice(value);
        }
    }

    /// Writes bytes into memory without bounds checks.
    ///
    /// # Safety
    ///
    /// If `value` is non-empty, `offset..offset + value.len()` must be in bounds.
    #[inline]
    pub(crate) unsafe fn set_unchecked(&mut self, offset: usize, value: &[u8]) {
        if !value.is_empty() {
            unsafe {
                self.data.get_unchecked_mut(offset..offset + value.len()).copy_from_slice(value);
            }
        }
    }

    /// Writes a data slice into memory with zero padding.
    ///
    /// # Panics
    ///
    /// Panics if the range is out of bounds.
    #[inline]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn set_data(&mut self, memory_offset: usize, data_offset: usize, len: usize, data: &[u8]) {
        unsafe { set_data(&mut self.data, data, memory_offset, data_offset, len) };
    }

    /// Copies bytes within memory.
    ///
    /// # Panics
    ///
    /// Panics if the range is out of bounds.
    #[inline]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn copy(&mut self, dst: usize, src: usize, len: usize) {
        self.data.copy_within(src..src + len, dst);
    }

    /// Returns a memory slice.
    ///
    /// # Panics
    ///
    /// Panics if the range is out of bounds.
    #[inline]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn slice(&self, offset: usize, len: usize) -> &[u8] {
        if len == 0 {
            return &[];
        }
        self.slice_range(offset..offset + len)
    }
}

unsafe fn set_data(dst: &mut [u8], src: &[u8], dst_offset: usize, src_offset: usize, len: usize) {
    if len == 0 {
        return;
    }
    if src_offset >= src.len() {
        dst.get_mut(dst_offset..dst_offset + len).unwrap().fill(0);
        return;
    }
    let src_end = min(src_offset + len, src.len());
    let src_len = src_end - src_offset;
    debug_assert!(src_offset < src.len() && src_end <= src.len());
    let data = unsafe { src.get_unchecked(src_offset..src_end) };
    unsafe { dst.get_unchecked_mut(dst_offset..dst_offset + src_len).copy_from_slice(data) };
    unsafe { dst.get_unchecked_mut(dst_offset + src_len..dst_offset + len).fill(0) };
}

#[cfg(test)]
mod tests {
    use core::assert_matches;

    use super::*;
    use crate::{SpecId, Version};

    #[test]
    fn test_num_words() {
        assert_eq!(num_words(0), 0);
        assert_eq!(num_words(1), 1);
        assert_eq!(num_words(31), 1);
        assert_eq!(num_words(32), 1);
        assert_eq!(num_words(33), 2);
        assert_eq!(num_words(63), 2);
        assert_eq!(num_words(64), 2);
        assert_eq!(num_words(65), 3);
        assert_eq!(num_words(usize::MAX - 31), usize::MAX / 32);
        assert_eq!(num_words(usize::MAX - 30), (usize::MAX / 32) + 1);
        assert_eq!(num_words(usize::MAX), (usize::MAX / 32) + 1);
    }

    #[test]
    fn resize_evm_accounts_expansion_gas() {
        let mut gas = Gas::new(100);
        let mut memory = Memory::new();

        let gas_params = Version::new(SpecId::FRONTIER).gas_params;
        memory.resize_evm(&mut gas, &gas_params, 0, 32).unwrap();
        assert_eq!(gas.remaining(), 97);
        assert_eq!(gas.memory().words_num, 1);
        assert_eq!(memory.len(), 32);

        let gas_params = Version::new(SpecId::FRONTIER).gas_params;
        memory.resize_evm(&mut gas, &gas_params, 0, 1).unwrap();
        assert_eq!(gas.remaining(), 97);
        assert_eq!(memory.len(), 32);

        let gas_params = Version::new(SpecId::FRONTIER).gas_params;
        memory.resize_evm(&mut gas, &gas_params, 0, 64).unwrap();
        assert_eq!(gas.remaining(), 94);
        assert_eq!(gas.memory().words_num, 2);
        assert_eq!(memory.len(), 64);
    }

    #[test]
    fn resize_evm_respects_memory_limit() {
        let mut gas = Gas::new(100_000);
        let mut memory = Memory::new();
        memory.set_memory_limit(64);

        let gas_params = Version::new(SpecId::FRONTIER).gas_params;
        memory.resize_evm(&mut gas, &gas_params, 0, 32).unwrap();
        assert_eq!(memory.len(), 32);

        memory.resize_evm(&mut gas, &gas_params, 0, 64).unwrap();
        assert_eq!(memory.len(), 64);

        assert_matches!(
            memory.resize_evm(&mut gas, &gas_params, 0, 96),
            Err(InstrStop::MemoryLimitOOG)
        );
        assert_eq!(memory.len(), 64);
        assert_eq!(gas.memory().words_num, 2);
    }
}
