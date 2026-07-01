use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
};

use alloy_primitives::Bytes;

use crate::{Result, TDX_REPORT_DATA_LEN, TdxRuntimeError};

const DEFAULT_TSM_REPORT_ROOT: &str = "/sys/kernel/config/tsm/report";

const TDX_CONFIGFS_PROVIDER_NAME: &str = "tdx_guest";

/// Narrow provider trait for TDX quote generation.
pub trait TdxQuoteProvider: Send + Sync {
    /// Generates a quote over exactly 64 report-data bytes.
    fn quote(&self, report_data: &[u8; TDX_REPORT_DATA_LEN]) -> Result<Bytes>;
}

/// TDX quote provider backed by Linux TSM/configfs.
#[derive(Debug)]
pub struct ConfigfsTdxQuoteProvider {
    report_dir: PathBuf,
    // ponytail: per-provider lock; add a path registry if multiple providers share one report dir.
    quote_lock: Mutex<()>,
}

impl ConfigfsTdxQuoteProvider {
    /// Creates a provider under the default TSM report root.
    pub fn new(report_name: impl AsRef<Path>) -> Self {
        Self {
            report_dir: Path::new(DEFAULT_TSM_REPORT_ROOT).join(report_name),
            quote_lock: Mutex::new(()),
        }
    }
}

impl TdxQuoteProvider for ConfigfsTdxQuoteProvider {
    fn quote(&self, report_data: &[u8; TDX_REPORT_DATA_LEN]) -> Result<Bytes> {
        let _quote_guard = self.quote_lock.lock().map_err(|_| {
            TdxRuntimeError::QuoteGeneration("configfs quote lock is poisoned".into())
        })?;

        fs::create_dir_all(&self.report_dir)
            .map_err(|error| TdxRuntimeError::filesystem_at(&self.report_dir, error))?;

        let provider_path = self.report_dir.join("provider");
        match fs::read_to_string(&provider_path) {
            Ok(provider) if provider.trim() == TDX_CONFIGFS_PROVIDER_NAME => {}
            Ok(provider) => {
                return Err(TdxRuntimeError::UnexpectedConfigfsProvider(
                    provider.trim().to_owned(),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(TdxRuntimeError::filesystem_at(&provider_path, error)),
        }

        let generation_path = self.report_dir.join("generation");
        let read_generation = || {
            let generation = fs::read_to_string(&generation_path)
                .map_err(|error| TdxRuntimeError::filesystem_at(&generation_path, error))?;
            generation.trim().parse::<u64>().map_err(|error| {
                TdxRuntimeError::QuoteGeneration(format!(
                    "invalid configfs generation at {}: {error}",
                    generation_path.display()
                ))
            })
        };

        let expected_generation = read_generation()?.checked_add(1).ok_or_else(|| {
            TdxRuntimeError::QuoteGeneration(
                "configfs generation counter overflowed while collecting a quote".into(),
            )
        })?;

        let inblob_path = self.report_dir.join("inblob");
        fs::write(&inblob_path, report_data)
            .map_err(|error| TdxRuntimeError::filesystem_at(&inblob_path, error))?;

        let outblob_path = self.report_dir.join("outblob");
        let quote = fs::read(&outblob_path)
            .map_err(|error| TdxRuntimeError::filesystem_at(&outblob_path, error))?;
        if quote.is_empty() {
            return Err(TdxRuntimeError::QuoteGeneration(
                "configfs returned an empty quote".into(),
            ));
        }

        let actual_generation = read_generation()?;
        if actual_generation != expected_generation {
            return Err(TdxRuntimeError::ConfigfsGenerationMismatch {
                expected: expected_generation,
                actual: actual_generation,
            });
        }

        Ok(Bytes::from(quote))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        path::Path,
        process::Command,
        thread::{self, JoinHandle},
    };

    use tempfile::TempDir;

    use super::*;
    use crate::TDX_REPORT_DATA_LEN;

    fn test_provider(provider: &str) -> (TempDir, PathBuf, ConfigfsTdxQuoteProvider) {
        let temp = TempDir::new().unwrap();
        let report_dir = temp.path().join("base-tdx-runtime-test");
        fs::create_dir_all(&report_dir).unwrap();
        fs::write(report_dir.join("provider"), provider).unwrap();
        fs::write(report_dir.join("outblob"), b"fixture-quote").unwrap();
        let quote_provider =
            ConfigfsTdxQuoteProvider { report_dir: report_dir.clone(), quote_lock: Mutex::new(()) };

        (temp, report_dir, quote_provider)
    }

    fn spawn_generation_writer(report_dir: &Path, generations: [u64; 2]) -> JoinHandle<()> {
        let generation_path = report_dir.join("generation");
        let status = Command::new("mkfifo").arg(&generation_path).status().unwrap();
        assert!(status.success());

        thread::spawn(move || {
            for generation in generations {
                let mut file = fs::OpenOptions::new().write(true).open(&generation_path).unwrap();
                writeln!(file, "{generation}").unwrap();
            }
        })
    }

    #[test]
    fn configfs_provider_reads_quote_from_report_dir() {
        let (_temp, report_dir, provider) = test_provider(TDX_CONFIGFS_PROVIDER_NAME);
        let generation_writer = spawn_generation_writer(&report_dir, [7, 8]);

        let quote = provider.quote(&[0x11; TDX_REPORT_DATA_LEN]).unwrap();
        generation_writer.join().unwrap();

        assert_eq!(fs::read(report_dir.join("inblob")).unwrap(), [0x11; TDX_REPORT_DATA_LEN]);
        assert_eq!(quote, Bytes::from_static(b"fixture-quote"));
    }

    #[test]
    fn configfs_provider_rejects_generation_counter_mismatch() {
        let (_temp, report_dir, provider) = test_provider(TDX_CONFIGFS_PROVIDER_NAME);
        let generation_writer = spawn_generation_writer(&report_dir, [7, 9]);

        assert!(matches!(
            provider.quote(&[0x11; TDX_REPORT_DATA_LEN]),
            Err(TdxRuntimeError::ConfigfsGenerationMismatch { expected: 8, actual: 9 })
        ));
        generation_writer.join().unwrap();
    }

    #[test]
    fn configfs_provider_rejects_non_tdx_provider_marker() {
        let (_temp, _report_dir, provider) = test_provider("sev_guest");

        assert!(matches!(
            provider.quote(&[0x11; TDX_REPORT_DATA_LEN]),
            Err(TdxRuntimeError::UnexpectedConfigfsProvider(provider)) if provider == "sev_guest"
        ));
    }
}
