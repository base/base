//! BLAKE3 integrity verification of extracted snapshot files.

use std::path::Path;

use anyhow::{Context, Result};
use indicatif::ProgressBar;
use tracing::{debug, warn};

use crate::OutputFileChecksum;

/// Read buffer size for BLAKE3 hashing (64 KB).
const HASH_BUF_SIZE: usize = 64 * 1024;

/// Verifies extracted files against their manifest checksums.
#[derive(Debug)]
pub struct OutputVerifier<'a> {
    target_dir: &'a Path,
}

impl<'a> OutputVerifier<'a> {
    /// Creates a verifier rooted at the given data directory.
    pub const fn new(target_dir: &'a Path) -> Self {
        Self { target_dir }
    }

    /// Returns `true` if all output files exist with correct size and BLAKE3 hash.
    ///
    /// Returns `false` (not an error) when any file is missing or mismatched,
    /// allowing the caller to decide whether to re-download.
    pub fn verify(&self, output_files: &[OutputFileChecksum]) -> Result<bool> {
        self.verify_with_progress(output_files, None)
    }

    /// Verifies output files, optionally updating a progress bar.
    pub fn verify_with_progress(
        &self,
        output_files: &[OutputFileChecksum],
        progress: Option<&ProgressBar>,
    ) -> Result<bool> {
        for expected in output_files {
            let output_path = self.target_dir.join(&expected.path);

            let meta = match std::fs::metadata(&output_path) {
                Ok(m) => m,
                Err(_) => {
                    debug!(path = %expected.path, "file missing");
                    return Ok(false);
                }
            };

            if meta.len() != expected.size {
                debug!(
                    path = %expected.path,
                    expected_size = expected.size,
                    actual_size = meta.len(),
                    "size mismatch"
                );
                return Ok(false);
            }

            let actual_hash = Self::file_blake3(&output_path, progress)
                .with_context(|| format!("failed to hash {}", output_path.display()))?;

            if !actual_hash.eq_ignore_ascii_case(&expected.blake3) {
                warn!(
                    path = %expected.path,
                    expected = %expected.blake3,
                    actual = %actual_hash,
                    "BLAKE3 mismatch"
                );
                return Ok(false);
            }
        }

        Ok(true)
    }

    /// Removes all output files declared in the checksums list.
    pub fn cleanup(&self, output_files: &[OutputFileChecksum]) {
        for entry in output_files {
            let path = self.target_dir.join(&entry.path);
            if path.exists()
                && let Err(e) = std::fs::remove_file(&path) {
                    warn!(path = %path.display(), error = %e, "failed to remove file");
                }
        }
    }

    /// Computes the hex-encoded BLAKE3 hash of a file.
    fn file_blake3(path: &Path, progress: Option<&ProgressBar>) -> Result<String> {
        let mut file = std::fs::File::open(path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let mut hasher = blake3::Hasher::new();
        let mut buf = vec![0u8; HASH_BUF_SIZE];

        loop {
            let n = std::io::Read::read(&mut file, &mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            if let Some(pb) = progress {
                pb.inc(n as u64);
            }
        }

        Ok(hasher.finalize().to_hex().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_returns_false_for_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let verifier = OutputVerifier::new(dir.path());
        let checksums = vec![OutputFileChecksum {
            path: "nonexistent.dat".to_string(),
            size: 100,
            blake3: "abc".to_string(),
        }];

        assert!(!verifier.verify(&checksums).unwrap(), "missing file should return false");
    }

    #[test]
    fn verify_returns_false_for_size_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.dat"), b"hello").unwrap();
        let verifier = OutputVerifier::new(dir.path());
        let checksums = vec![OutputFileChecksum {
            path: "data.dat".to_string(),
            size: 999,
            blake3: "ignored".to_string(),
        }];

        assert!(!verifier.verify(&checksums).unwrap(), "size mismatch should return false");
    }

    #[test]
    fn verify_returns_true_for_correct_file() {
        let dir = tempfile::tempdir().unwrap();
        let content = b"hello world";
        std::fs::write(dir.path().join("data.dat"), content).unwrap();

        let expected_hash = blake3::hash(content).to_hex().to_string();
        let verifier = OutputVerifier::new(dir.path());
        let checksums = vec![OutputFileChecksum {
            path: "data.dat".to_string(),
            size: content.len() as u64,
            blake3: expected_hash,
        }];

        assert!(verifier.verify(&checksums).unwrap(), "correct file should verify");
    }

    #[test]
    fn verify_returns_false_for_hash_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.dat"), b"hello").unwrap();
        let verifier = OutputVerifier::new(dir.path());
        let checksums = vec![OutputFileChecksum {
            path: "data.dat".to_string(),
            size: 5,
            blake3: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        }];

        assert!(!verifier.verify(&checksums).unwrap(), "wrong hash should return false");
    }

    #[test]
    fn cleanup_removes_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db").join("mdbx.dat");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"data").unwrap();

        let verifier = OutputVerifier::new(dir.path());
        let checksums = vec![OutputFileChecksum {
            path: "db/mdbx.dat".to_string(),
            size: 4,
            blake3: String::new(),
        }];

        verifier.cleanup(&checksums);
        assert!(!path.exists(), "cleanup should remove the file");
    }
}
