//! Download command wrapper that extends reth's `DownloadCommand` with `--proofs`.
//!
//! Delegates all standard snapshot components to reth's download pipeline. The
//! Base-specific proofs database is downloaded separately by [`ProofsDownloader`]
//! from the same snapshot source and manifest.

use std::{ffi::OsString, path::PathBuf, sync::Arc};

use base_execution_chainspec::BaseChainSpec;
use clap::Parser;
use eyre::Result;
use reth_chainspec::EthChainSpec;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::download::DownloadCommand;
use reth_node_core::args::DatadirArgs;
use tracing::info;

mod proofs;
pub use proofs::{
    ProofsArchiveAvailability, ProofsArchiveDownload, ProofsArchiveExtractor, ProofsArchiveReader,
    ProofsArchiveSizeMismatch, ProofsAvailabilityAbortGuard, ProofsDecodedChunks, ProofsDownloader,
    ProofsExtractionProgress, ProofsManifest, ProofsManifestEntry, ProofsPieceMap,
};

/// Download Base node snapshots from R2 storage.
///
/// Wraps reth's download command with an additional `--proofs` flag that
/// downloads the expanded trie proof database for fault proof support.
///
/// When `--proofs` is passed, the command runs reth's standard download
/// then fetches and extracts the proofs archive from the same snapshot source.
#[derive(Debug, Parser)]
pub struct BaseDownloadCommand<C: ChainSpecParser> {
    #[command(flatten)]
    inner: DownloadCommand<C>,

    /// Also download the proofs database for fault proof support.
    ///
    /// After the standard download completes, fetches the proofs archive
    /// from the same snapshot source and extracts it into the data directory.
    /// Re-running with `--proofs` will overwrite any existing proofs database.
    #[arg(long)]
    proofs: bool,
}

impl<C: ChainSpecParser<ChainSpec = BaseChainSpec>> BaseDownloadCommand<C> {
    /// Executes the download command.
    pub async fn execute<N>(self) -> Result<()> {
        let Self { inner, proofs } = self;

        let (data_dir, chain_id) = if proofs {
            let chain = inner
                .chain_spec()
                .ok_or_else(|| eyre::eyre!("--proofs flag is only on Base"))?
                .chain();
            let chain_id = chain.id();
            let dir = resolve_datadir_args(std::env::args_os()).resolve_datadir(chain);
            info!(target: "reth::cli", datadir = %dir.data_dir().display(), "Resolved datadir for proofs download");
            (Some(dir), Some(chain_id))
        } else {
            (None, None)
        };

        inner.execute::<N>().await?;

        if let (Some(data_dir), Some(chain_id)) = (data_dir, chain_id) {
            let target_dir = data_dir.data_dir().to_path_buf();
            let concurrency = resolve_download_concurrency_arg(std::env::args_os());
            let manifest_url = resolve_manifest_url_arg(std::env::args_os());
            ProofsDownloader::run(&target_dir, chain_id, concurrency, manifest_url).await?;
        }

        Ok(())
    }
}

/// Extracts `--datadir` from the current process args without doing a second
/// permissive clap parse of the whole command.
fn resolve_datadir_args(args: impl IntoIterator<Item = OsString>) -> DatadirArgs {
    let mut datadir_args = DatadirArgs::default();
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        let Some(arg) = arg.to_str() else { continue };

        if arg == "--datadir" {
            if let Some(value) = args.next() {
                datadir_args.datadir = PathBuf::from(value).into();
            }
            continue;
        }

        if let Some(value) = arg.strip_prefix("--datadir=") {
            datadir_args.datadir = PathBuf::from(value).into();
        }
    }

    datadir_args
}

/// Matches reth's `--download-concurrency` default.
const DEFAULT_DOWNLOAD_CONCURRENCY: usize = 8;

/// Extracts `--download-concurrency` so proofs use the same parallel budget as reth.
fn resolve_download_concurrency_arg(args: impl IntoIterator<Item = OsString>) -> usize {
    let mut concurrency = DEFAULT_DOWNLOAD_CONCURRENCY;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        let Some(arg) = arg.to_str() else { continue };

        if arg == "--download-concurrency" {
            if let Some(value) = args.next()
                && let Ok(parsed) = value.to_string_lossy().parse::<usize>()
            {
                concurrency = parsed;
            }
            continue;
        }

        if let Some(value) = arg.strip_prefix("--download-concurrency=")
            && let Ok(parsed) = value.parse::<usize>()
        {
            concurrency = parsed;
        }
    }

    concurrency.max(1)
}

/// Extracts `--manifest-url` so proofs reuse the same snapshot reth downloaded.
fn resolve_manifest_url_arg(args: impl IntoIterator<Item = OsString>) -> Option<String> {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let Some(arg) = arg.to_str() else { continue };

        if arg == "--manifest-url" {
            return args.next().and_then(|value| value.into_string().ok());
        }

        if let Some(value) = arg.strip_prefix("--manifest-url=") {
            return Some(value.to_string());
        }
    }

    None
}

impl<C: ChainSpecParser> BaseDownloadCommand<C> {
    /// Returns the underlying chain spec.
    pub fn chain_spec(&self) -> Option<&Arc<C::ChainSpec>> {
        self.inner.chain_spec()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::Parser;

    use super::*;
    use crate::chainspec::BaseChainSpecParser;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        args: BaseDownloadCommand<BaseChainSpecParser>,
    }

    #[test]
    fn proofs_flag_is_parsed() {
        let cli = TestCli::parse_from(["test", "--proofs"]);
        assert!(cli.args.proofs, "--proofs should be true");
    }

    #[test]
    fn download_without_proofs_flag() {
        let cli = TestCli::parse_from(["test"]);
        assert!(!cli.args.proofs, "--proofs should default to false");
    }

    #[test]
    fn resolve_datadir_args_uses_explicit_datadir() {
        let datadir = resolve_datadir_args([
            OsString::from("test"),
            OsString::from("--datadir"),
            OsString::from("/tmp/base-download-test"),
        ])
        .resolve_datadir(BaseChainSpec::mainnet().chain());

        assert_eq!(
            datadir.data_dir(),
            Path::new("/tmp/base-download-test"),
            "proofs download should use --datadir without adding the chain directory"
        );
    }

    #[test]
    fn resolve_datadir_args_uses_equals_syntax() {
        let datadir = resolve_datadir_args([
            OsString::from("test"),
            OsString::from("--datadir=/tmp/base-download-test"),
        ])
        .resolve_datadir(BaseChainSpec::mainnet().chain());

        assert_eq!(
            datadir.data_dir(),
            Path::new("/tmp/base-download-test"),
            "proofs download should use --datadir=VALUE without adding the chain directory"
        );
    }

    #[test]
    fn resolve_manifest_url_arg_reads_separate_flag() {
        let url = resolve_manifest_url_arg([
            OsString::from("test"),
            OsString::from("--manifest-url"),
            OsString::from("https://zeronet-v2-snapshots.base.org/1789516802/manifest.json"),
        ]);

        assert_eq!(
            url.as_deref(),
            Some("https://zeronet-v2-snapshots.base.org/1789516802/manifest.json"),
            "proofs should reuse the same --manifest-url as reth's downloader"
        );
    }

    #[test]
    fn resolve_manifest_url_arg_reads_equals_syntax() {
        let url = resolve_manifest_url_arg([
            OsString::from("test"),
            OsString::from(
                "--manifest-url=https://zeronet-v2-snapshots.base.org/1789516802/manifest.json",
            ),
        ]);

        assert_eq!(
            url.as_deref(),
            Some("https://zeronet-v2-snapshots.base.org/1789516802/manifest.json"),
            "proofs should reuse --manifest-url=VALUE"
        );
    }

    #[test]
    fn resolve_manifest_url_arg_is_none_without_flag() {
        let url = resolve_manifest_url_arg([
            OsString::from("test"),
            OsString::from("--proofs"),
            OsString::from("--chain"),
            OsString::from("base-zeronet"),
        ]);

        assert_eq!(url, None, "missing --manifest-url should fall back to snapshot API discovery");
    }

    #[test]
    fn resolve_download_concurrency_arg_defaults_to_reth() {
        let concurrency =
            resolve_download_concurrency_arg([OsString::from("test"), OsString::from("--proofs")]);

        assert_eq!(
            concurrency, DEFAULT_DOWNLOAD_CONCURRENCY,
            "proofs should use reth's default --download-concurrency"
        );
    }

    #[test]
    fn resolve_download_concurrency_arg_reads_separate_flag() {
        let concurrency = resolve_download_concurrency_arg([
            OsString::from("test"),
            OsString::from("--download-concurrency"),
            OsString::from("16"),
        ]);

        assert_eq!(concurrency, 16, "proofs should reuse --download-concurrency");
    }

    #[test]
    fn resolve_download_concurrency_arg_reads_equals_syntax() {
        let concurrency = resolve_download_concurrency_arg([
            OsString::from("test"),
            OsString::from("--download-concurrency=4"),
        ]);

        assert_eq!(concurrency, 4, "proofs should reuse --download-concurrency=VALUE");
    }

    #[test]
    fn resolve_download_concurrency_arg_rejects_zero() {
        let concurrency = resolve_download_concurrency_arg([
            OsString::from("test"),
            OsString::from("--download-concurrency"),
            OsString::from("0"),
        ]);

        assert_eq!(concurrency, 1, "zero concurrency should be clamped to one stream");
    }
}
