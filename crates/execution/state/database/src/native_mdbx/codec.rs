use std::{borrow::Cow, slice};

use derive_more::{Debug, Deref, DerefMut};

use crate::native_mdbx::{Error, TransactionKind, ffi};

/// Implement this to be able to decode data values
pub trait TableObject: Sized {
    /// Decodes the object from the given bytes.
    fn decode(data_val: &[u8]) -> Result<Self, Error>;

    /// Decodes the value directly from the given MDBX_val pointer.
    ///
    /// # Safety
    ///
    /// This should only in the context of an MDBX transaction.
    #[doc(hidden)]
    unsafe fn decode_val<K: TransactionKind>(
        _: *const ffi::MDBX_txn,
        data_val: ffi::MDBX_val,
    ) -> Result<Self, Error> {
        let s = unsafe { slice::from_raw_parts(data_val.iov_base as *const u8, data_val.iov_len) };
        Self::decode(s)
    }
}

impl TableObject for Cow<'_, [u8]> {
    fn decode(_: &[u8]) -> Result<Self, Error> {
        unreachable!()
    }

    #[doc(hidden)]
    unsafe fn decode_val<K: TransactionKind>(
        _txn: *const ffi::MDBX_txn,
        data_val: ffi::MDBX_val,
    ) -> Result<Self, Error> {
        let s = unsafe { slice::from_raw_parts(data_val.iov_base as *const u8, data_val.iov_len) };

        Ok(Cow::Borrowed(s))
    }
}

impl TableObject for Vec<u8> {
    fn decode(data_val: &[u8]) -> Result<Self, Error> {
        Ok(data_val.to_vec())
    }
}

impl TableObject for () {
    fn decode(_: &[u8]) -> Result<Self, Error> {
        Ok(())
    }

    unsafe fn decode_val<K: TransactionKind>(
        _: *const ffi::MDBX_txn,
        _: ffi::MDBX_val,
    ) -> Result<Self, Error> {
        Ok(())
    }
}

/// If you don't need the data itself, just its length.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Deref, DerefMut)]
pub struct ObjectLength(pub usize);

impl TableObject for ObjectLength {
    fn decode(data_val: &[u8]) -> Result<Self, Error> {
        Ok(Self(data_val.len()))
    }
}

impl<const LEN: usize> TableObject for [u8; LEN] {
    fn decode(data_val: &[u8]) -> Result<Self, Error> {
        if data_val.len() != LEN {
            return Err(Error::DecodeErrorLenDiff);
        }
        let mut a = [0; LEN];
        a[..].copy_from_slice(data_val);
        Ok(a)
    }
}
