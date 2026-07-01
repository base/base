use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
};

use alloy_primitives::Bytes;

use crate::{Result, TdxReportData, TdxRuntimeError};

/// Default Linux TSM/configfs report root.
pub const DEFAULT_TSM_REPORT_ROOT: &str = "/sys/kernel/config/tsm/report";

/// Provider name exposed by the Linux TDX guest TSM backend.
pub const TDX_CONFIGFS_PROVIDER_NAME: &str = "tdx_guest";

const INBLOB_FILE: &str = "inblob";
const OUTBLOB_FILE: &str = "outblob";
const GENERATION_FILE: &str = "generation";
const PROVIDER_FILE: &str = "provider";

/// Narrow provider trait for TDX quote generation.
pub trait TdxQuoteProvider: Send + Sync {
    /// Generates a quote over exactly 64 report-data bytes.
    fn quote(&self, report_data: &[u8]) -> Result<Bytes>;
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
        Self::with_report_dir(Path::new(DEFAULT_TSM_REPORT_ROOT).join(report_name))
    }

    /// Creates a provider from a concrete report directory.
    pub fn with_report_dir(report_dir: impl Into<PathBuf>) -> Self {
        Self { report_dir: report_dir.into(), quote_lock: Mutex::new(()) }
    }
}

impl TdxQuoteProvider for ConfigfsTdxQuoteProvider {
    fn quote(&self, report_data: &[u8]) -> Result<Bytes> {
        TdxReportData::validate(report_data)?;
        let _quote_guard = self.quote_lock.lock().map_err(|_| {
            TdxRuntimeError::QuoteGeneration("configfs quote lock is poisoned".into())
        })?;

        fs::create_dir_all(&self.report_dir)
            .map_err(|error| TdxRuntimeError::filesystem_at(&self.report_dir, error))?;

        let provider_path = self.report_dir.join(PROVIDER_FILE);
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

        let generation_path = self.report_dir.join(GENERATION_FILE);
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

        let expected_generation = read_generation()?;
        let expected_generation = expected_generation.checked_add(1).ok_or_else(|| {
            TdxRuntimeError::QuoteGeneration(
                "configfs generation counter overflowed while collecting a quote".into(),
            )
        })?;

        let inblob_path = self.report_dir.join(INBLOB_FILE);
        fs::write(&inblob_path, report_data)
            .map_err(|error| TdxRuntimeError::filesystem_at(&inblob_path, error))?;

        let outblob_path = self.report_dir.join(OUTBLOB_FILE);
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
        path::{Path, PathBuf},
        process::Command,
        thread::{self, JoinHandle},
    };

    use alloy_primitives::Bytes;
    use tempfile::TempDir;

    use super::*;
    use crate::TDX_REPORT_DATA_LEN;

    fn create_generation_fifo(report_dir: &Path) -> PathBuf {
        let generation_path = report_dir.join("generation");
        let status = Command::new("mkfifo").arg(&generation_path).status().unwrap();
        assert!(status.success());
        generation_path
    }

    fn spawn_generation_writer(
        generation_path: &Path,
        generations: impl IntoIterator<Item = u64>,
    ) -> JoinHandle<()> {
        let generation_path = generation_path.to_path_buf();
        let generations = generations.into_iter().collect::<Vec<_>>();

        thread::spawn(move || {
            for generation in generations {
                let mut file = fs::OpenOptions::new().write(true).open(&generation_path).unwrap();
                writeln!(file, "{generation}").unwrap();
            }
        })
    }

    #[test]
    fn providers_reject_non_64_byte_report_data_before_hardware_access() {
        let configfs = ConfigfsTdxQuoteProvider::with_report_dir("/path/that/does/not/exist");

        assert!(matches!(
            configfs.quote(&[0u8; 65]),
            Err(TdxRuntimeError::InvalidReportDataLength(65))
        ));
    }

    #[test]
    fn configfs_provider_reads_quote_from_report_dir() {
        let temp = TempDir::new().unwrap();
        let report_dir = temp.path().join("base-tdx-runtime-test");
        fs::create_dir_all(&report_dir).unwrap();
        fs::write(report_dir.join("provider"), TDX_CONFIGFS_PROVIDER_NAME).unwrap();
        fs::write(report_dir.join("outblob"), b"fixture-quote").unwrap();
        let generation_path = create_generation_fifo(&report_dir);
        let generation_writer = spawn_generation_writer(&generation_path, [7, 8]);

        let provider = ConfigfsTdxQuoteProvider::with_report_dir(&report_dir);
        let quote = provider.quote(&[0x11; TDX_REPORT_DATA_LEN]).unwrap();
        generation_writer.join().unwrap();

        assert_eq!(fs::read(report_dir.join("inblob")).unwrap(), [0x11; TDX_REPORT_DATA_LEN]);
        assert_eq!(quote, Bytes::from_static(b"fixture-quote"));
    }

    #[test]
    fn configfs_provider_rejects_generation_counter_mismatch() {
        let temp = TempDir::new().unwrap();
        let report_dir = temp.path().join("base-tdx-runtime-test");
        fs::create_dir_all(&report_dir).unwrap();
        fs::write(report_dir.join("provider"), TDX_CONFIGFS_PROVIDER_NAME).unwrap();
        fs::write(report_dir.join("outblob"), b"fixture-quote").unwrap();
        let generation_path = create_generation_fifo(&report_dir);
        let generation_writer = spawn_generation_writer(&generation_path, [7, 9]);

        let provider = ConfigfsTdxQuoteProvider::with_report_dir(&report_dir);
        assert!(matches!(
            provider.quote(&[0x11; TDX_REPORT_DATA_LEN]),
            Err(TdxRuntimeError::ConfigfsGenerationMismatch { expected: 8, actual: 9 })
        ));
        generation_writer.join().unwrap();
    }

    #[test]
    fn configfs_provider_rejects_non_tdx_provider_marker() {
        let temp = TempDir::new().unwrap();
        let report_dir = temp.path().join("base-tdx-runtime-test");
        fs::create_dir_all(&report_dir).unwrap();
        fs::write(report_dir.join("provider"), "sev_guest").unwrap();

        let provider = ConfigfsTdxQuoteProvider::with_report_dir(&report_dir);
        assert!(matches!(
            provider.quote(&[0x11; TDX_REPORT_DATA_LEN]),
            Err(TdxRuntimeError::UnexpectedConfigfsProvider(provider)) if provider == "sev_guest"
        ));
    }
}
