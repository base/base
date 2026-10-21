//! Download of the Base proofs database for `base download --proofs`.
//!
//! This covers only the `proofs` snapshot component. Every other component is
//! downloaded and extracted by reth's downloader, so extraction problems with
//! those components do not originate here.
//!
//! The archive is fetched as fixed-size pieces and extracted while it
//! downloads: [`ProofsArchiveDownload`] publishes the contiguous prefix written
//! to disk, and [`ProofsArchiveExtractor`] decodes and unpacks that prefix as
//! it grows.

use std::time::Duration;

/// Interval between proofs download and extraction progress logs.
const PROOFS_PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(3);

mod archive_download;
pub use archive_download::{ProofsArchiveDownload, ProofsArchiveSizeMismatch, ProofsPieceMap};

mod archive_extract;
pub use archive_extract::{
    ProofsArchiveAvailability, ProofsArchiveExtractor, ProofsArchiveReader,
    ProofsAvailabilityAbortGuard, ProofsDecodedChunks, ProofsExtractionProgress,
};

mod downloader;
pub use downloader::ProofsDownloader;

mod manifest;
pub use manifest::{ProofsManifest, ProofsManifestEntry};

#[cfg(test)]
mod test_utils;
