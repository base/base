use crate::{Compress, Decompress};

/// Implements compression for SCALE type.
macro_rules! impl_compression_for_scale {
    ($($name:ty),+) => {
        $(
            impl Compress for $name {
                type Compressed = alloc::vec::Vec<u8>;

                fn compress(self) -> Self::Compressed {
                    parity_scale_codec::Encode::encode(&self)
                }

                fn compress_to_buf<B: bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
                    parity_scale_codec::Encode::encode_to(&self, OutputCompat::wrap_mut(buf));
                }
            }

            impl Decompress for $name {
                fn decompress(mut value: &[u8]) -> Result<Self, $crate::DecompressError> {
                    parity_scale_codec::Decode::decode(&mut value).map_err($crate::DecompressError::new)
                }
            }
        )+
    };
}

impl_compression_for_scale!(u8, u32, u16, u64, alloc::vec::Vec<u8>);

#[repr(transparent)]
struct OutputCompat<B>(B);

impl<B> OutputCompat<B> {
    fn wrap_mut(buf: &mut B) -> &mut Self {
        unsafe { core::mem::transmute(buf) }
    }
}

impl<B: bytes::BufMut> parity_scale_codec::Output for OutputCompat<B> {
    fn write(&mut self, bytes: &[u8]) {
        self.0.put_slice(bytes);
    }

    fn push_byte(&mut self, byte: u8) {
        self.0.put_u8(byte);
    }
}
