//! Zstandard archive streams, including a framed layout that decompresses in parallel.
//!
//! A framed archive is a sequence of independently compressed zstd frames, each preceded by
//! the 12-byte skippable frame written by `pzstd`: the skippable-frame magic `0x184D2A50`, a
//! little-endian content length of `4`, then the little-endian compressed length of the next
//! frame. Standard zstd decoders skip the skippable frames and decode the concatenated frames
//! sequentially, so framed archives stay readable by existing clients and by `zstd -d`, while
//! [`FramedZstdDecoder`] and `pzstd -d` decode frames concurrently.

use std::{
    collections::VecDeque,
    fmt,
    io::{self, BufRead, Read, Write},
};

use rayon::prelude::*;
use zstd::zstd_safe::CParameter;

/// Uncompressed bytes per independently compressed frame in a framed archive.
///
/// Smaller frames parallelize better but lose some compression context at every boundary.
pub const FRAMED_ZSTD_CHUNK_SIZE: usize = 16 * 1024 * 1024;

/// Largest frame content size decoded into a single up-front allocation.
///
/// Frames declaring a larger size, or no size, are decoded through a growing buffer so an
/// untrusted header cannot request an arbitrarily large allocation.
const MAX_PREALLOCATED_FRAME_SIZE: u64 = 1024 * 1024 * 1024;

/// Largest compressed frame accepted by [`FramedZstdDecoder`].
///
/// Frames written by [`FramedZstdEncoder`] never exceed zstd's compression bound for one
/// [`FRAMED_ZSTD_CHUNK_SIZE`] chunk. The limit leaves room for frames produced by `pzstd` at high
/// compression levels while rejecting corrupted headers before they drive large reads.
const MAX_COMPRESSED_FRAME_LEN: usize = 256 * 1024 * 1024;

/// Zstd compression level used for snapshot archives (`0` selects zstd's default level).
const ARCHIVE_COMPRESSION_LEVEL: i32 = 0;

/// Compression layout used when writing a snapshot archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveCompression {
    /// A single zstd frame, optionally compressed by zstd's native worker threads.
    ///
    /// Decompression of a single frame is inherently sequential.
    Stream {
        /// Number of zstd worker threads; `0` compresses on the calling thread.
        workers: u32,
    },
    /// Independent `pzstd`-compatible frames compressed on the Rayon pool, which can be
    /// decompressed in parallel.
    Framed,
}

/// The skippable frame that precedes every zstd frame in a framed archive.
#[derive(Debug)]
pub struct FramedZstdHeader;

impl FramedZstdHeader {
    /// Encoded header length in bytes.
    pub const LEN: usize = 12;

    /// Skippable-frame magic number used by `pzstd`.
    pub const MAGIC: u32 = 0x184D_2A50;

    /// Length of the skippable frame's content: one little-endian `u32` frame length.
    const CONTENT_LEN: u32 = 4;

    /// Encodes the header announcing a zstd frame of `frame_len` compressed bytes.
    pub fn encode(frame_len: u32) -> [u8; Self::LEN] {
        let mut header = [0; Self::LEN];
        header[..4].copy_from_slice(&Self::MAGIC.to_le_bytes());
        header[4..8].copy_from_slice(&Self::CONTENT_LEN.to_le_bytes());
        header[8..].copy_from_slice(&frame_len.to_le_bytes());
        header
    }

    /// Returns `true` when `prefix` begins with a framed-archive header.
    pub fn matches(prefix: &[u8]) -> bool {
        prefix.len() >= 8
            && prefix[..4] == Self::MAGIC.to_le_bytes()
            && prefix[4..8] == Self::CONTENT_LEN.to_le_bytes()
    }

    /// Decodes the compressed length of the frame announced by `header`.
    pub fn frame_len(header: &[u8; Self::LEN]) -> io::Result<usize> {
        if !Self::matches(header) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected a framed zstd archive header",
            ));
        }
        let frame_len = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
        usize::try_from(frame_len)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame length overflow"))
    }
}

/// Writes a framed zstd archive, compressing batches of frames on the Rayon pool.
///
/// Input is split into [`FRAMED_ZSTD_CHUNK_SIZE`] chunks. Up to `batch_frames` chunks are
/// buffered, compressed concurrently, and written in input order, so peak memory is roughly
/// `2 * batch_frames * FRAMED_ZSTD_CHUNK_SIZE`.
///
/// Call [`Self::finish`] to write the final frames: dropping the encoder discards any input that
/// has not been emitted yet, matching [`zstd::Encoder`].
#[derive(Debug)]
pub struct FramedZstdEncoder<W: Write> {
    inner: W,
    batch_frames: usize,
    chunks: Vec<Vec<u8>>,
    frames_written: u64,
}

impl<W: Write> FramedZstdEncoder<W> {
    /// Creates an encoder that compresses up to `batch_frames` frames at a time.
    pub fn new(inner: W, batch_frames: usize) -> Self {
        Self { inner, batch_frames: batch_frames.max(1), chunks: Vec::new(), frames_written: 0 }
    }

    /// Compresses all buffered input and returns the inner writer.
    ///
    /// An archive with no input still receives one empty frame so the output is a valid zstd
    /// stream.
    pub fn finish(mut self) -> io::Result<W> {
        if self.frames_written == 0 && self.chunks.is_empty() {
            self.chunks.push(Vec::new());
        }
        self.write_pending_frames()?;
        self.inner.flush()?;
        Ok(self.inner)
    }

    /// Compresses the buffered chunks concurrently and writes them in input order.
    fn write_pending_frames(&mut self) -> io::Result<()> {
        let frames = self
            .chunks
            .par_iter()
            .map(|chunk| Self::compress_chunk(chunk))
            .collect::<io::Result<Vec<_>>>()?;
        self.chunks.clear();

        for frame in frames {
            let frame_len = u32::try_from(frame.len()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "compressed frame exceeds u32::MAX")
            })?;
            self.inner.write_all(&FramedZstdHeader::encode(frame_len))?;
            self.inner.write_all(&frame)?;
            self.frames_written += 1;
        }
        Ok(())
    }

    /// Compresses one chunk into a checksummed zstd frame that records its content size.
    fn compress_chunk(chunk: &[u8]) -> io::Result<Vec<u8>> {
        let mut compressor = zstd::bulk::Compressor::new(ARCHIVE_COMPRESSION_LEVEL)?;
        compressor.set_parameter(CParameter::ChecksumFlag(true))?;
        compressor.compress(chunk)
    }
}

impl<W: Write> Write for FramedZstdEncoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.chunks.last().is_none_or(|chunk| chunk.len() == FRAMED_ZSTD_CHUNK_SIZE) {
            if self.chunks.len() == self.batch_frames {
                self.write_pending_frames()?;
            }
            self.chunks.push(Vec::with_capacity(FRAMED_ZSTD_CHUNK_SIZE));
        }
        let Some(chunk) = self.chunks.last_mut() else {
            return Err(io::Error::other("framed encoder has no active chunk"));
        };
        let len = buf.len().min(FRAMED_ZSTD_CHUNK_SIZE - chunk.len());
        chunk.extend_from_slice(&buf[..len]);
        Ok(len)
    }

    /// Emits every buffered byte as frames, then flushes the inner writer.
    ///
    /// This honors the [`Write::flush`] contract, so a flush mid-stream ends the current frame
    /// early. `tar::Builder` only flushes when a caller flushes an entry writer, which snapshot
    /// packaging never does.
    fn flush(&mut self) -> io::Result<()> {
        self.write_pending_frames()?;
        self.inner.flush()
    }
}

/// Reads a framed zstd archive, decompressing batches of frames on the Rayon pool.
///
/// Up to `batch_frames` frames are read and decompressed concurrently, then served in order.
#[derive(Debug)]
pub struct FramedZstdDecoder<R: Read> {
    inner: R,
    batch_frames: usize,
    decoded: VecDeque<Vec<u8>>,
    offset: usize,
    finished: bool,
}

impl<R: Read> FramedZstdDecoder<R> {
    /// Creates a decoder that decompresses up to `batch_frames` frames at a time.
    pub fn new(inner: R, batch_frames: usize) -> Self {
        Self {
            inner,
            batch_frames: batch_frames.max(1),
            decoded: VecDeque::new(),
            offset: 0,
            finished: false,
        }
    }

    /// Reads and decompresses the next batch of frames.
    fn decode_batch(&mut self) -> io::Result<()> {
        let mut frames = Vec::with_capacity(self.batch_frames);
        while frames.len() < self.batch_frames {
            match self.read_frame()? {
                Some(frame) => frames.push(frame),
                None => {
                    self.finished = true;
                    break;
                }
            }
        }
        let decoded = frames
            .par_iter()
            .map(|frame| Self::decompress_frame(frame))
            .collect::<io::Result<Vec<_>>>()?;
        self.decoded.extend(decoded);
        Ok(())
    }

    /// Reads one header and its compressed frame, or `None` at a clean end of stream.
    fn read_frame(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut header = [0; FramedZstdHeader::LEN];
        let mut filled = 0;
        while filled < header.len() {
            match self.inner.read(&mut header[filled..]) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        if filled == 0 {
            return Ok(None);
        }
        if filled < header.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated framed zstd archive header",
            ));
        }

        let frame_len = FramedZstdHeader::frame_len(&header)?;
        if frame_len > MAX_COMPRESSED_FRAME_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "framed zstd archive frame exceeds the maximum compressed length",
            ));
        }
        // Grow the buffer with the bytes actually read so a truncated archive fails without
        // allocating the full declared length.
        let mut frame = Vec::new();
        (&mut self.inner).take(frame_len as u64).read_to_end(&mut frame)?;
        if frame.len() < frame_len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated framed zstd archive frame",
            ));
        }
        Ok(Some(frame))
    }

    /// Decompresses one complete zstd frame.
    fn decompress_frame(frame: &[u8]) -> io::Result<Vec<u8>> {
        match zstd::zstd_safe::get_frame_content_size(frame) {
            Ok(Some(size)) if size <= MAX_PREALLOCATED_FRAME_SIZE => {
                let capacity = usize::try_from(size).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "frame content size overflow")
                })?;
                zstd::bulk::decompress(frame, capacity)
            }
            _ => zstd::stream::decode_all(frame),
        }
    }
}

impl<R: Read> Read for FramedZstdDecoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if let Some(front) = self.decoded.front() {
                if self.offset < front.len() {
                    let len = buf.len().min(front.len() - self.offset);
                    buf[..len].copy_from_slice(&front[self.offset..self.offset + len]);
                    self.offset += len;
                    return Ok(len);
                }
                self.decoded.pop_front();
                self.offset = 0;
                continue;
            }
            if self.finished {
                return Ok(0);
            }
            self.decode_batch()?;
        }
    }
}

/// Writes a snapshot archive with the selected [`ArchiveCompression`].
pub enum ZstdArchiveEncoder<'a, W: Write> {
    /// A single zstd frame.
    Stream(zstd::Encoder<'a, W>),
    /// Independent frames that can be decompressed in parallel.
    Framed(FramedZstdEncoder<W>),
}

impl<W: Write + fmt::Debug> fmt::Debug for ZstdArchiveEncoder<'_, W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stream(_) => f.write_str("ZstdArchiveEncoder::Stream"),
            Self::Framed(encoder) => {
                f.debug_tuple("ZstdArchiveEncoder::Framed").field(encoder).finish()
            }
        }
    }
}

impl<W: Write> ZstdArchiveEncoder<'static, W> {
    /// Creates an encoder for `compression` with checksummed frames.
    pub fn new(writer: W, compression: ArchiveCompression) -> io::Result<Self> {
        match compression {
            ArchiveCompression::Stream { workers } => {
                let mut encoder = zstd::Encoder::new(writer, ARCHIVE_COMPRESSION_LEVEL)?;
                encoder.include_checksum(true)?;
                if workers > 0 {
                    encoder.multithread(workers)?;
                }
                Ok(Self::Stream(encoder))
            }
            ArchiveCompression::Framed => {
                Ok(Self::Framed(FramedZstdEncoder::new(writer, rayon::current_num_threads())))
            }
        }
    }
}

impl<W: Write> ZstdArchiveEncoder<'_, W> {
    /// Finalizes the compressed stream and returns the inner writer.
    pub fn finish(self) -> io::Result<W> {
        match self {
            Self::Stream(encoder) => encoder.finish(),
            Self::Framed(encoder) => encoder.finish(),
        }
    }
}

impl<W: Write> Write for ZstdArchiveEncoder<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Stream(encoder) => encoder.write(buf),
            Self::Framed(encoder) => encoder.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Stream(encoder) => encoder.flush(),
            Self::Framed(encoder) => encoder.flush(),
        }
    }
}

/// Reads a snapshot archive, decompressing framed archives in parallel.
///
/// Archives that begin with a [`FramedZstdHeader`] are decoded by [`FramedZstdDecoder`]; any
/// other input is decoded as a standard zstd stream, which also accepts framed archives.
pub enum ZstdArchiveReader<'a, R: BufRead> {
    /// A standard sequential zstd decoder.
    Stream(zstd::Decoder<'a, R>),
    /// A parallel decoder for framed archives.
    Framed(FramedZstdDecoder<R>),
}

impl<R: BufRead + fmt::Debug> fmt::Debug for ZstdArchiveReader<'_, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stream(_) => f.write_str("ZstdArchiveReader::Stream"),
            Self::Framed(decoder) => {
                f.debug_tuple("ZstdArchiveReader::Framed").field(decoder).finish()
            }
        }
    }
}

impl<R: BufRead> ZstdArchiveReader<'static, R> {
    /// Detects the archive layout from the buffered prefix of `reader`.
    ///
    /// Detection only inspects the reader's current buffer, so `reader` must buffer at least
    /// [`FramedZstdHeader::LEN`] bytes on its first fill (any [`std::io::BufReader`] over a file
    /// does). If fewer bytes are buffered, the standard sequential decoder is used.
    pub fn new(mut reader: R) -> io::Result<Self> {
        if FramedZstdHeader::matches(reader.fill_buf()?) {
            Ok(Self::Framed(FramedZstdDecoder::new(reader, rayon::current_num_threads())))
        } else {
            Ok(Self::Stream(zstd::Decoder::with_buffer(reader)?))
        }
    }
}

impl<R: BufRead> Read for ZstdArchiveReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Stream(decoder) => decoder.read(buf),
            Self::Framed(decoder) => decoder.read(buf),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Cursor};

    use super::*;

    /// Deterministic, partially compressible input spanning several frames.
    fn sample_input(len: usize) -> Vec<u8> {
        (0..len).map(|index| ((index / 7) as u8).wrapping_mul(31) ^ (index % 251) as u8).collect()
    }

    fn encode_framed(input: &[u8], batch_frames: usize) -> Vec<u8> {
        let mut encoder = FramedZstdEncoder::new(Vec::new(), batch_frames);
        encoder.write_all(input).unwrap();
        encoder.finish().unwrap()
    }

    fn read_all(mut reader: impl Read) -> Vec<u8> {
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        output
    }

    #[test]
    fn framed_archive_decodes_with_standard_zstd_decoder() {
        let input = sample_input(FRAMED_ZSTD_CHUNK_SIZE * 2 + 12_345);
        let archive = encode_framed(&input, 2);

        assert_eq!(read_all(zstd::Decoder::new(Cursor::new(&archive)).unwrap()), input);
        assert_eq!(zstd::stream::decode_all(Cursor::new(&archive)).unwrap(), input);
    }

    #[test]
    fn framed_archive_round_trips_through_parallel_decoder() {
        let input = sample_input(FRAMED_ZSTD_CHUNK_SIZE * 3 + 1);
        let archive = encode_framed(&input, 2);

        assert!(FramedZstdHeader::matches(&archive));
        assert_eq!(read_all(FramedZstdDecoder::new(Cursor::new(&archive), 2)), input);
    }

    #[test]
    fn framed_archive_uses_one_frame_per_chunk() {
        let input = sample_input(FRAMED_ZSTD_CHUNK_SIZE * 2 + 1);
        let archive = encode_framed(&input, 8);

        let mut offset = 0;
        let mut frames = 0;
        while offset < archive.len() {
            let header: [u8; FramedZstdHeader::LEN] =
                archive[offset..offset + FramedZstdHeader::LEN].try_into().unwrap();
            offset += FramedZstdHeader::LEN + FramedZstdHeader::frame_len(&header).unwrap();
            frames += 1;
        }

        assert_eq!(offset, archive.len());
        assert_eq!(frames, 3);
    }

    #[test]
    fn empty_framed_archive_is_a_valid_zstd_stream() {
        let archive = encode_framed(&[], 4);

        assert!(read_all(zstd::Decoder::new(Cursor::new(&archive)).unwrap()).is_empty());
        assert!(read_all(FramedZstdDecoder::new(Cursor::new(&archive), 4)).is_empty());
    }

    #[test]
    fn archive_reader_detects_framed_and_stream_archives() {
        let input = sample_input(FRAMED_ZSTD_CHUNK_SIZE + 99);
        let framed = encode_framed(&input, 2);
        let stream = zstd::encode_all(Cursor::new(&input), 0).unwrap();

        let reader = ZstdArchiveReader::new(BufReader::new(Cursor::new(&framed))).unwrap();
        assert!(matches!(reader, ZstdArchiveReader::Framed(_)));
        assert_eq!(read_all(reader), input);

        let reader = ZstdArchiveReader::new(BufReader::new(Cursor::new(&stream))).unwrap();
        assert!(matches!(reader, ZstdArchiveReader::Stream(_)));
        assert_eq!(read_all(reader), input);
    }

    #[test]
    fn framed_decoder_rejects_truncated_frames() {
        let input = sample_input(FRAMED_ZSTD_CHUNK_SIZE / 2);
        let archive = encode_framed(&input, 1);
        let truncated = &archive[..archive.len() - 1];

        let error = FramedZstdDecoder::new(Cursor::new(truncated), 1)
            .read_to_end(&mut Vec::new())
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn framed_decoder_rejects_oversized_frame_headers() {
        let header = FramedZstdHeader::encode(u32::MAX);

        let error = FramedZstdDecoder::new(Cursor::new(header), 1)
            .read_to_end(&mut Vec::new())
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn framed_decoder_rejects_non_framed_input() {
        let stream = zstd::encode_all(Cursor::new(sample_input(1024)), 0).unwrap();

        let error = FramedZstdDecoder::new(Cursor::new(&stream), 1)
            .read_to_end(&mut Vec::new())
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
