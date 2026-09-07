//! Compression and decompression traits and implementations.

use alloc::{boxed::Box, vec::Vec};
use alloy_primitives::{Address, Bytes, B256};
use core::{
    error::Error,
    fmt::{self, Debug},
};

#[cfg(feature = "std")]
mod scale;

/// Error type returned by [`Decompress`] trait.
pub struct DecompressError(Box<dyn core::error::Error + Send + Sync>);

impl DecompressError {
    /// Creates a new `AnyError` wrapping the given error value.
    pub fn new<E>(error: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self(Box::new(error))
    }

    /// Returns a reference to the underlying error value.
    pub fn as_error(&self) -> &(dyn Error + Send + Sync + 'static) {
        self.0.as_ref()
    }
}

impl fmt::Debug for DecompressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for DecompressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl Error for DecompressError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.0.source()
    }
}

/// Trait that will transform the data to be saved in the DB in a (ideally) compressed format.
pub trait Compress: Send + Sync + Sized + Debug {
    /// Compressed type.
    type Compressed: bytes::BufMut
        + AsRef<[u8]>
        + AsMut<[u8]>
        + Into<Vec<u8>>
        + Default
        + Send
        + Sync
        + Debug;

    /// If the type cannot be compressed, return its inner reference as `Some(self.as_ref())`
    fn uncompressable_ref(&self) -> Option<&[u8]> {
        None
    }

    /// Compresses data going into the database.
    fn compress(self) -> Self::Compressed {
        let mut buf = Self::Compressed::default();
        self.compress_to_buf(&mut buf);
        buf
    }

    /// Compresses data to a given buffer.
    fn compress_to_buf<B: bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B);
}

/// Trait that will transform the data to be read from the DB.
pub trait Decompress: Send + Sync + Sized + Debug {
    /// Decompresses data coming from the database.
    fn decompress(value: &[u8]) -> Result<Self, DecompressError>;

    /// Decompresses owned data coming from the database.
    fn decompress_owned(value: Vec<u8>) -> Result<Self, DecompressError> {
        Self::decompress(&value)
    }
}

/// Implements [`Compress`] and [`Decompress`] for types that implement [`crate::Compact`].
#[macro_export]
macro_rules! impl_compression_for_compact {
    ($($name:ident$(<$($generic:ident),*>)?),+) => {
        $(
            impl$(<$($generic: core::fmt::Debug + Send + Sync + $crate::Compact),*>)? $crate::Compress for $name$(<$($generic),*>)? {
                type Compressed = alloc::vec::Vec<u8>;

                fn compress_to_buf<B: bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
                    let _ = $crate::Compact::to_compact(self, buf);
                }
            }

            impl$(<$($generic: core::fmt::Debug + Send + Sync + $crate::Compact),*>)? $crate::Decompress for $name$(<$($generic),*>)? {
                fn decompress(value: &[u8]) -> Result<$name$(<$($generic),*>)?, $crate::DecompressError> {
                    let (obj, _) = $crate::Compact::from_compact(value, value.len());
                    Ok(obj)
                }
            }
        )+
    };
}

/// Implements [`Compress`] and [`Decompress`] for types that implement [`crate::Compact`] and have
/// a fixed-size uncompressable representation.
#[macro_export]
macro_rules! impl_compression_fixed_compact {
    ($($name:tt),+) => {
        $(
            impl $crate::Compress for $name {
                type Compressed = alloc::vec::Vec<u8>;

                fn uncompressable_ref(&self) -> Option<&[u8]> {
                    Some(self.as_ref())
                }

                fn compress_to_buf<B: bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
                    let _ = $crate::Compact::to_compact(self, buf);
                }
            }

            impl $crate::Decompress for $name {
                fn decompress(value: &[u8]) -> Result<$name, $crate::DecompressError> {
                    let (obj, _) = $crate::Compact::from_compact(value, value.len());
                    Ok(obj)
                }
            }
        )+
    };
}

impl_compression_fixed_compact!(B256, Address);

impl_compression_for_compact!(Bytes);

#[cfg(feature = "alloy")]
mod alloy {
    use alloy_consensus::{EthereumReceipt, EthereumTxEnvelope, Header, TxEip4844, TxType};
    use alloy_genesis::GenesisAccount;
    use alloy_primitives::Log;
    use alloy_trie::BranchNodeCompact;

    type TransactionSigned = EthereumTxEnvelope<TxEip4844>;

    impl_compression_for_compact!(
        Header,
        Log,
        TxType,
        BranchNodeCompact,
        GenesisAccount,
        EthereumReceipt<T>,
        TransactionSigned
    );
}
