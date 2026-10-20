//! Streaming extraction of the proofs archive while it is still downloading.
//!
//! [`ProofsArchiveDownload`](super::ProofsArchiveDownload) publishes the length
//! of the contiguous prefix of the `.part` file that is on disk.
//! [`ProofsArchiveReader`] blocks until more of that prefix is available, so
//! zstd decoding and tar unpacking run concurrently with the download. Decoding
//! and unpacking run on separate threads because unpacking alone is slower than
//! single-threaded zstd decoding.

use std::{
    fs::File,
    io::{self, BufRead, BufReader, Read},
    path::Path,
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    thread,
    time::Instant,
};

use base_reth_cli::ProgressDisplay;
use eyre::Result;
use tracing::info;

use super::PROOFS_PROGRESS_LOG_INTERVAL;

/// Read buffer between the `.part` file and the zstd decoder.
///
/// The decoder's default input buffer measurably slows decoding of multi-GiB archives.
const COMPRESSED_READ_BUFFER: usize = 1 << 20;

/// Size of each decoded chunk handed from the decoder to the tar thread.
const DECODED_CHUNK_SIZE: usize = 1 << 20;

/// Decoded chunks buffered between the decoder and the tar thread.
const DECODED_CHUNK_QUEUE: usize = 16;

/// Length of the proofs archive prefix that is fully written to the `.part` file.
#[derive(Debug)]
pub struct ProofsArchiveAvailability {
    total: u64,
    /// Published prefix length, or `None` once the download stopped before completing.
    available: Mutex<Option<u64>>,
    changed: Condvar,
}

impl ProofsArchiveAvailability {
    /// Creates availability for an archive of `total` bytes with nothing on disk yet.
    pub const fn new(total: u64) -> Self {
        Self { total, available: Mutex::new(Some(0)), changed: Condvar::new() }
    }

    /// Publishes that the first `available` bytes are written and flushed.
    ///
    /// The prefix never shrinks, and nothing is published after an abort.
    pub fn publish(&self, available: u64) {
        let mut state = self.lock();
        if let Some(current) = state.as_mut()
            && available > *current
        {
            *current = available.min(self.total);
            self.changed.notify_all();
        }
    }

    /// Fails readers because no more bytes will arrive.
    ///
    /// Does nothing once the whole archive is published, so a completed
    /// download stays readable.
    pub fn abort(&self) {
        let mut state = self.lock();
        if *state != Some(self.total) {
            *state = None;
            self.changed.notify_all();
        }
    }

    /// Returns a guard that aborts this availability when dropped, so a
    /// blocked reader never hangs after the download fails or is cancelled.
    pub fn abort_on_drop(self: &Arc<Self>) -> ProofsAvailabilityAbortGuard {
        ProofsAvailabilityAbortGuard { availability: Arc::clone(self) }
    }

    /// Returns whether every byte of the archive is published.
    pub fn is_complete(&self) -> bool {
        *self.lock() == Some(self.total)
    }

    /// Blocks until bytes past `position` are available and returns the
    /// available length, which equals `position` only at the end of the archive.
    ///
    /// Fails as soon as the download is aborted, even if published bytes remain
    /// unread, so a doomed extraction stops promptly.
    fn wait_past(&self, position: u64) -> io::Result<u64> {
        let mut state = self.lock();
        loop {
            match *state {
                None => {
                    return Err(io::Error::other(
                        "proofs download stopped before the archive completed",
                    ));
                }
                Some(available) if available > position || available == self.total => {
                    return Ok(available);
                }
                Some(_) => {
                    state =
                        self.changed.wait(state).unwrap_or_else(|poisoned| poisoned.into_inner());
                }
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, Option<u64>> {
        self.available.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Aborts a [`ProofsArchiveAvailability`] when dropped.
#[derive(Debug)]
pub struct ProofsAvailabilityAbortGuard {
    availability: Arc<ProofsArchiveAvailability>,
}

impl Drop for ProofsAvailabilityAbortGuard {
    fn drop(&mut self) {
        self.availability.abort();
    }
}

/// Reads the published prefix of the proofs `.part` file, blocking at its end
/// until more bytes are published.
#[derive(Debug)]
pub struct ProofsArchiveReader {
    file: File,
    position: u64,
    availability: Arc<ProofsArchiveAvailability>,
}

impl ProofsArchiveReader {
    /// Creates a reader over `file` starting at its beginning.
    pub const fn new(file: File, availability: Arc<ProofsArchiveAvailability>) -> Self {
        Self { file, position: 0, availability }
    }
}

impl Read for ProofsArchiveReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let available = self.availability.wait_past(self.position)?;
        let readable = available - self.position;
        if readable == 0 {
            return Ok(0);
        }

        let len = buf.len().min(usize::try_from(readable).unwrap_or(usize::MAX));
        let read = self.file.read(&mut buf[..len])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "proofs archive is shorter than its published length",
            ));
        }
        self.position += read as u64;
        Ok(read)
    }
}

/// Reports proofs extraction progress as compressed archive bytes are consumed.
#[derive(Debug)]
pub struct ProofsExtractionProgress<R> {
    inner: R,
    processed: u64,
    total: u64,
    started: Instant,
    last_log: Instant,
}

impl<R> ProofsExtractionProgress<R> {
    /// Wraps `inner`, an archive of `total` compressed bytes.
    pub fn new(inner: R, total: u64) -> Self {
        let now = Instant::now();
        Self { inner, processed: 0, total, started: now, last_log: now }
    }
}

impl<R: Read> Read for ProofsExtractionProgress<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.processed = self.processed.saturating_add(read as u64).min(self.total);

        if read > 0 && self.last_log.elapsed() >= PROOFS_PROGRESS_LOG_INTERVAL {
            let elapsed = self.started.elapsed();
            let speed = self.processed as f64 / elapsed.as_secs_f64();
            let eta = ProgressDisplay::eta(self.processed, self.total, elapsed)
                .map_or_else(|| "unknown".to_string(), |eta| eta.to_string());
            info!(
                target: "reth::cli",
                progress = %ProgressDisplay::human_byte_progress(self.processed, self.total),
                speed = %ProgressDisplay::speed(speed),
                eta = %eta,
                elapsed = %ProgressDisplay::duration(elapsed),
                "Proofs extraction progress"
            );
            self.last_log = Instant::now();
        }

        Ok(read)
    }
}

/// Presents decoded chunks received from the decoder thread as a byte stream,
/// returning each consumed chunk to the decoder for reuse.
#[derive(Debug)]
pub struct ProofsDecodedChunks {
    chunks: Receiver<Vec<u8>>,
    recycled: SyncSender<Vec<u8>>,
    chunk: Vec<u8>,
    offset: usize,
}

impl Read for ProofsDecodedChunks {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.offset == self.chunk.len() {
            // Recycle before blocking so the decoder can reuse this buffer for
            // the chunk being waited on.
            let _ = self.recycled.try_send(std::mem::take(&mut self.chunk));
            let Ok(chunk) = self.chunks.recv() else { return Ok(0) };
            self.chunk = chunk;
            self.offset = 0;
        }
        let len = buf.len().min(self.chunk.len() - self.offset);
        buf[..len].copy_from_slice(&self.chunk[self.offset..self.offset + len]);
        self.offset += len;
        Ok(len)
    }
}

/// Decodes the proofs `.tar.zst` archive and unpacks it on separate threads.
#[derive(Debug)]
pub struct ProofsArchiveExtractor;

impl ProofsArchiveExtractor {
    /// Extracts the archive at `archive_path` into `target_dir` as its prefix
    /// becomes available.
    ///
    /// Returns only after the whole archive is decoded, so the zstd checksum
    /// and any bytes after the tar end marker are validated.
    pub fn extract(
        archive_path: &Path,
        availability: Arc<ProofsArchiveAvailability>,
        target_dir: &Path,
    ) -> Result<()> {
        let total = availability.total;
        let reader = ProofsArchiveReader::new(File::open(archive_path)?, availability);
        let reader = ProofsExtractionProgress::new(reader, total);
        let decoder =
            zstd::Decoder::with_buffer(BufReader::with_capacity(COMPRESSED_READ_BUFFER, reader))?;

        let (sender, chunks) = sync_channel(DECODED_CHUNK_QUEUE);
        let (recycled, reusable) = sync_channel(DECODED_CHUNK_QUEUE);
        let target_dir = target_dir.to_path_buf();
        let unpacker =
            thread::Builder::new().name("proofs-untar".to_string()).spawn(move || {
                Self::unpack(
                    ProofsDecodedChunks { chunks, recycled, chunk: Vec::new(), offset: 0 },
                    &target_dir,
                )
            })?;

        let decoded = Self::decode(decoder, &sender, &reusable);
        drop(sender);
        let unpacked =
            unpacker.join().map_err(|_| eyre::eyre!("proofs tar unpack thread panicked"))?;

        match decoded {
            Err(error) => Err(eyre::eyre!("failed to decode proofs archive: {error}")),
            Ok(()) => unpacked,
        }
    }

    /// Sends decoded chunks until the end of the archive or until the unpack
    /// thread stops receiving, which only happens when it failed.
    fn decode<R: BufRead>(
        mut decoder: zstd::Decoder<'static, R>,
        sender: &SyncSender<Vec<u8>>,
        reusable: &Receiver<Vec<u8>>,
    ) -> io::Result<()> {
        loop {
            let mut chunk = reusable.try_recv().unwrap_or_default();
            chunk.resize(DECODED_CHUNK_SIZE, 0);
            let read = loop {
                match decoder.read(&mut chunk) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    result => break result?,
                }
            };
            if read == 0 {
                return Ok(());
            }
            chunk.truncate(read);
            if sender.send(chunk).is_err() {
                return Ok(());
            }
        }
    }

    /// Unpacks the tar stream, then drains the rest of the decoded stream so
    /// the decoder always runs to the end of the archive.
    fn unpack(mut chunks: ProofsDecodedChunks, target_dir: &Path) -> Result<()> {
        tar::Archive::new(&mut chunks)
            .unpack(target_dir)
            .map_err(|error| eyre::eyre!("failed to unpack proofs archive: {error}"))?;
        io::copy(&mut chunks, &mut io::sink())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, thread, time::Duration};

    use super::*;
    use crate::commands::download::proofs::test_utils::create_proofs_archive;

    /// Writes `archive` to a temp file and returns availability with every byte published.
    fn write_complete_archive(
        dir: &Path,
        archive: &[u8],
    ) -> (PathBuf, Arc<ProofsArchiveAvailability>) {
        let path = dir.join("proofs.tar.zst.part");
        std::fs::write(&path, archive).unwrap();
        let availability = Arc::new(ProofsArchiveAvailability::new(archive.len() as u64));
        availability.publish(archive.len() as u64);
        (path, availability)
    }

    #[test]
    fn extract_preserves_directory_structure() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let archive = create_proofs_archive(&[
            ("proofs/data.mdb", b"data"),
            ("proofs/lock.mdb", b"lock"),
            ("proofs/nested/deep.dat", b"deep"),
        ]);
        let (path, availability) = write_complete_archive(src.path(), &archive);

        ProofsArchiveExtractor::extract(&path, availability, dest.path()).unwrap();

        assert_eq!(std::fs::read(dest.path().join("proofs/data.mdb")).unwrap(), b"data");
        assert_eq!(std::fs::read(dest.path().join("proofs/lock.mdb")).unwrap(), b"lock");
        assert_eq!(std::fs::read(dest.path().join("proofs/nested/deep.dat")).unwrap(), b"deep");
    }

    #[test]
    fn extract_fails_on_missing_archive() {
        let dest = tempfile::tempdir().unwrap();
        let availability = Arc::new(ProofsArchiveAvailability::new(16));
        availability.publish(16);

        let result = ProofsArchiveExtractor::extract(
            &dest.path().join("nonexistent.tar.zst"),
            availability,
            dest.path(),
        );

        assert!(result.is_err());
    }

    #[test]
    fn extract_rejects_trailing_garbage() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let mut archive = create_proofs_archive(&[("proofs/data.mdb", b"data")]);
        archive.extend_from_slice(b"not-a-zstd-frame");
        let (path, availability) = write_complete_archive(src.path(), &archive);

        let error = ProofsArchiveExtractor::extract(&path, availability, dest.path())
            .expect_err("bytes after the last zstd frame must fail extraction");

        assert!(error.to_string().contains("decode"), "error: {error}");
    }

    #[test]
    fn extract_rejects_truncated_archive() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let archive = create_proofs_archive(&[("proofs/data.mdb", &[7u8; 64 * 1024])]);
        let (path, availability) =
            write_complete_archive(src.path(), &archive[..archive.len() - 8]);

        let result = ProofsArchiveExtractor::extract(&path, availability, dest.path());

        assert!(result.is_err(), "an archive cut short must fail extraction");
    }

    #[test]
    fn reader_waits_for_published_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proofs.tar.zst.part");
        std::fs::write(&path, b"abcdef").unwrap();
        let availability = Arc::new(ProofsArchiveAvailability::new(6));
        availability.publish(2);

        let reader_availability = Arc::clone(&availability);
        let reader = thread::spawn(move || {
            let mut out = Vec::new();
            ProofsArchiveReader::new(File::open(path).unwrap(), reader_availability)
                .read_to_end(&mut out)
                .map(|_| out)
        });

        thread::sleep(Duration::from_millis(50));
        assert!(!reader.is_finished(), "reader must block at the published prefix");
        availability.publish(6);

        assert_eq!(reader.join().unwrap().unwrap(), b"abcdef");
    }

    #[test]
    fn abort_wakes_blocked_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proofs.tar.zst.part");
        std::fs::write(&path, b"abcdef").unwrap();
        let availability = Arc::new(ProofsArchiveAvailability::new(6));

        let reader_availability = Arc::clone(&availability);
        let reader = thread::spawn(move || {
            let mut out = Vec::new();
            ProofsArchiveReader::new(File::open(path).unwrap(), reader_availability)
                .read_to_end(&mut out)
        });
        thread::sleep(Duration::from_millis(50));
        drop(availability.abort_on_drop());

        assert!(reader.join().unwrap().is_err(), "abort must wake and fail a blocked reader");
    }

    #[test]
    fn abort_stops_reads_of_unread_published_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proofs.tar.zst.part");
        std::fs::write(&path, b"abcdef").unwrap();
        let availability = Arc::new(ProofsArchiveAvailability::new(6));
        availability.publish(3);
        availability.abort();

        let mut buf = [0u8; 6];
        let result =
            ProofsArchiveReader::new(File::open(path).unwrap(), availability).read(&mut buf);

        assert!(result.is_err(), "an aborted download must not keep feeding the extractor");
    }

    #[test]
    fn abort_after_completion_keeps_bytes_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proofs.tar.zst.part");
        std::fs::write(&path, b"abcdef").unwrap();
        let availability = Arc::new(ProofsArchiveAvailability::new(6));
        availability.publish(6);
        availability.abort();

        let mut out = Vec::new();
        ProofsArchiveReader::new(File::open(path).unwrap(), availability)
            .read_to_end(&mut out)
            .unwrap();

        assert_eq!(out, b"abcdef");
    }
}
