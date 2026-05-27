//! Archive extraction for tar+zstd compressed snapshots.

use std::path::Path;

use anyhow::{Context, Result};
use indicatif::ProgressBar;
use tracing::debug;

/// Extracts a tar+zstd archive to the target directory.
///
/// The archive is decompressed with zstd then unpacked with tar. Each entry is
/// written relative to `target_dir`, preserving the archive's internal directory
/// structure (e.g. `db/mdbx.dat`, `static_files/...`).
#[derive(Debug)]
pub struct ArchiveExtractor;

impl ArchiveExtractor {
    /// Extracts a `.tar.zst` archive file to `target_dir`.
    pub fn extract(
        archive_path: &Path,
        target_dir: &Path,
        progress: Option<&ProgressBar>,
    ) -> Result<()> {
        let file = std::fs::File::open(archive_path)
            .with_context(|| format!("failed to open archive {}", archive_path.display()))?;

        let file_size = file.metadata()?.len();
        if let Some(pb) = progress {
            pb.set_length(file_size);
            pb.set_position(0);
        }

        let counting_reader = CountingReader::new(file, progress);
        let decoder = zstd::Decoder::new(counting_reader).with_context(|| {
            format!("failed to create zstd decoder for {}", archive_path.display())
        })?;
        let mut archive = tar::Archive::new(decoder);

        archive.unpack(target_dir).with_context(|| {
            format!("failed to extract {} to {}", archive_path.display(), target_dir.display())
        })?;

        if let Some(pb) = progress {
            pb.set_position(file_size);
        }

        debug!(
            archive = %archive_path.display(),
            target = %target_dir.display(),
            "extraction complete"
        );

        Ok(())
    }

    /// Extracts a tar+zstd archive from an in-memory reader.
    ///
    /// Used for streaming extraction where the archive bytes are piped directly
    /// from an HTTP response without writing to disk first.
    pub fn extract_from_reader<R: std::io::Read>(reader: R, target_dir: &Path) -> Result<()> {
        let decoder = zstd::Decoder::new(reader).context("failed to create zstd decoder")?;
        let mut archive = tar::Archive::new(decoder);

        archive
            .unpack(target_dir)
            .with_context(|| format!("failed to extract to {}", target_dir.display()))?;

        Ok(())
    }
}

/// Wraps a reader to track bytes read and update a progress bar.
struct CountingReader<'a, R> {
    inner: R,
    progress: Option<&'a ProgressBar>,
}

impl<'a, R> CountingReader<'a, R> {
    const fn new(inner: R, progress: Option<&'a ProgressBar>) -> Self {
        Self { inner, progress }
    }
}

impl<R: std::io::Read> std::io::Read for CountingReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0
            && let Some(pb) = self.progress {
                pb.inc(n as u64);
            }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a tar+zstd archive containing a single file with the given content.
    fn create_test_archive(
        dir: &Path,
        archive_name: &str,
        inner_path: &str,
        content: &[u8],
    ) -> std::path::PathBuf {
        let archive_path = dir.join(archive_name);
        let file = std::fs::File::create(&archive_path).unwrap();
        let encoder = zstd::Encoder::new(file, 0).unwrap();
        let mut builder = tar::Builder::new(encoder);

        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, inner_path, content).unwrap();

        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();
        archive_path
    }

    #[test]
    fn extract_tar_zst_single_file() {
        let src_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();

        let archive =
            create_test_archive(src_dir.path(), "test.tar.zst", "db/mdbx.dat", b"state-data");

        ArchiveExtractor::extract(&archive, dest_dir.path(), None).unwrap();

        let extracted = dest_dir.path().join("db/mdbx.dat");
        assert!(extracted.exists(), "extracted file should exist");
        assert_eq!(std::fs::read(&extracted).unwrap(), b"state-data");
    }

    #[test]
    fn extract_from_reader_works() {
        let src_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();

        let archive = create_test_archive(
            src_dir.path(),
            "stream.tar.zst",
            "static_files/headers_0_499999",
            b"header-data",
        );

        let file = std::fs::File::open(&archive).unwrap();
        ArchiveExtractor::extract_from_reader(file, dest_dir.path()).unwrap();

        let extracted = dest_dir.path().join("static_files/headers_0_499999");
        assert!(extracted.exists(), "extracted file should exist");
        assert_eq!(std::fs::read(&extracted).unwrap(), b"header-data");
    }

    #[test]
    fn extract_nonexistent_archive_fails() {
        let dir = tempfile::tempdir().unwrap();
        let result =
            ArchiveExtractor::extract(&dir.path().join("nonexistent.tar.zst"), dir.path(), None);
        assert!(result.is_err(), "nonexistent archive should fail");
    }
}
