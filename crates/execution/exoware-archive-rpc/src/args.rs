//! `--archive.exoware.*` command-line arguments.

use std::time::Duration;

use alloy_primitives::B256;
use base_exoware_archive::{
    ArchiveConfig, ArchiveError, DEFAULT_EXPORT_BATCH_BLOCKS, DEFAULT_NAMESPACE,
    DEFAULT_PRUNE_GRACE_SEGMENTS, DEFAULT_STORE_TIMEOUT, MAX_BLOCKS_PER_SEGMENT, PublisherOptions,
};

/// Default blocks per segment.
pub const DEFAULT_SEGMENT_BLOCKS: u64 = 8_192;

/// Exoware archive arguments. Leaving `--archive.exoware.url` unset turns the feature off and
/// leaves the node unchanged. The store API key is read by the SDK from `EXOWARE_API_KEY` only;
/// there is no flag for it.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Exoware archive")]
pub struct ExowareArchiveArgs {
    /// Exoware store URL. Unset disables the archive.
    #[arg(long = "archive.exoware.url", id = "archive_exoware_url", value_name = "URL")]
    pub archive_exoware_url: Option<String>,

    /// Key namespace inside the store (lowercase letters, digits and `-`, ending in `/`).
    #[arg(
        long = "archive.exoware.namespace",
        id = "archive_exoware_namespace",
        default_value = DEFAULT_NAMESPACE,
        requires = "archive_exoware_url"
    )]
    pub archive_exoware_namespace: String,

    /// Publish finalized segments from this node.
    #[arg(
        long = "archive.exoware.export",
        id = "archive_exoware_export",
        default_value_t = false,
        requires = "archive_exoware_url"
    )]
    pub archive_exoware_export: bool,

    /// Write the archive config row at startup if it is absent. An existing row must match.
    #[arg(
        long = "archive.exoware.provision",
        id = "archive_exoware_provision",
        default_value_t = false,
        requires = "archive_exoware_url"
    )]
    pub archive_exoware_provision: bool,

    /// Blocks per segment.
    #[arg(
        long = "archive.exoware.segment-blocks",
        id = "archive_exoware_segment_blocks",
        default_value_t = DEFAULT_SEGMENT_BLOCKS,
        value_parser = clap::value_parser!(u64).range(1..=MAX_BLOCKS_PER_SEGMENT),
        requires = "archive_exoware_url"
    )]
    pub archive_exoware_segment_blocks: u64,

    /// First archived block; must be a multiple of the segment size.
    #[arg(
        long = "archive.exoware.start-block",
        id = "archive_exoware_start_block",
        default_value_t = 0,
        requires = "archive_exoware_url"
    )]
    pub archive_exoware_start_block: u64,

    /// Most recent segments to keep; older segments are pruned. Unset keeps everything.
    #[arg(
        long = "archive.exoware.retain-segments",
        id = "archive_exoware_retain_segments",
        value_parser = clap::value_parser!(u64).range(1..),
        requires = "archive_exoware_url"
    )]
    pub archive_exoware_retain_segments: Option<u64>,

    /// Store budget per RPC request and per store call of the exporter.
    #[arg(
        long = "archive.exoware.timeout",
        id = "archive_exoware_timeout",
        default_value = "5s",
        value_parser = humantime::parse_duration,
        requires = "archive_exoware_url"
    )]
    pub archive_exoware_timeout: Duration,

    /// Segments kept below the earliest served segment before rows are physically pruned.
    #[arg(
        long = "archive.exoware.prune-grace-segments",
        id = "archive_exoware_prune_grace_segments",
        default_value_t = DEFAULT_PRUNE_GRACE_SEGMENTS,
        value_parser = clap::value_parser!(u64).range(1..),
        requires = "archive_exoware_url"
    )]
    pub archive_exoware_prune_grace_segments: u64,

    /// Blocks the exporter loads, encodes and writes at a time; bounds its block memory.
    #[arg(
        long = "archive.exoware.export-batch-blocks",
        id = "archive_exoware_export_batch_blocks",
        default_value_t = DEFAULT_EXPORT_BATCH_BLOCKS,
        value_parser = clap::value_parser!(u64).range(1..=MAX_BLOCKS_PER_SEGMENT),
        requires = "archive_exoware_url"
    )]
    pub archive_exoware_export_batch_blocks: u64,
}

impl Default for ExowareArchiveArgs {
    fn default() -> Self {
        Self {
            archive_exoware_url: None,
            archive_exoware_namespace: DEFAULT_NAMESPACE.to_string(),
            archive_exoware_export: false,
            archive_exoware_provision: false,
            archive_exoware_segment_blocks: DEFAULT_SEGMENT_BLOCKS,
            archive_exoware_start_block: 0,
            archive_exoware_retain_segments: None,
            archive_exoware_timeout: DEFAULT_STORE_TIMEOUT,
            archive_exoware_prune_grace_segments: DEFAULT_PRUNE_GRACE_SEGMENTS,
            archive_exoware_export_batch_blocks: DEFAULT_EXPORT_BATCH_BLOCKS,
        }
    }
}

impl ExowareArchiveArgs {
    /// Whether the archive is configured.
    pub const fn enabled(&self) -> bool {
        self.archive_exoware_url.is_some()
    }

    /// The archive config these arguments describe for the node's chain. The start block must
    /// be segment-aligned.
    pub fn archive_config(
        &self,
        chain_id: u64,
        genesis_hash: B256,
    ) -> Result<ArchiveConfig, ArchiveError> {
        let blocks = self.archive_exoware_segment_blocks;
        if blocks == 0 || !self.archive_exoware_start_block.is_multiple_of(blocks) {
            return Err(ArchiveError::InvalidConfig {
                reason: format!(
                    "--archive.exoware.start-block {} is not a multiple of --archive.exoware.segment-blocks {blocks}",
                    self.archive_exoware_start_block
                ),
            });
        }
        ArchiveConfig::new(
            chain_id,
            genesis_hash,
            blocks,
            self.archive_exoware_start_block / blocks,
            self.archive_exoware_retain_segments,
        )
    }

    /// Publisher tuning from these arguments.
    pub fn publisher_options(&self) -> PublisherOptions {
        PublisherOptions {
            prune_grace_segments: self.archive_exoware_prune_grace_segments,
            export_batch_blocks: self.archive_exoware_export_batch_blocks,
            ..PublisherOptions::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Debug, Parser)]
    struct CommandParser {
        #[command(flatten)]
        args: ExowareArchiveArgs,
    }

    /// Parses `flags` after a program name.
    fn parse(flags: &[&str]) -> Result<ExowareArchiveArgs, clap::Error> {
        CommandParser::try_parse_from(["reth"].iter().chain(flags).copied()).map(|c| c.args)
    }

    /// No flags is the disabled default; every flag parses into its field.
    #[test]
    fn flags_parse_into_their_fields() {
        let off = parse(&[]).expect("no flags parse");
        assert_eq!(off, ExowareArchiveArgs::default(), "no flags is the default");
        assert!(!off.enabled(), "unset url disables the archive");

        let args = parse(&[
            "--archive.exoware.url",
            "http://127.0.0.1:9000",
            "--archive.exoware.namespace",
            "mainnet-archive/",
            "--archive.exoware.export",
            "--archive.exoware.provision",
            "--archive.exoware.segment-blocks",
            "64",
            "--archive.exoware.start-block",
            "128",
            "--archive.exoware.retain-segments",
            "10",
            "--archive.exoware.timeout",
            "750ms",
            "--archive.exoware.prune-grace-segments",
            "3",
            "--archive.exoware.export-batch-blocks",
            "32",
        ])
        .expect("all flags parse");
        assert_eq!(
            args,
            ExowareArchiveArgs {
                archive_exoware_url: Some("http://127.0.0.1:9000".to_string()),
                archive_exoware_namespace: "mainnet-archive/".to_string(),
                archive_exoware_export: true,
                archive_exoware_provision: true,
                archive_exoware_segment_blocks: 64,
                archive_exoware_start_block: 128,
                archive_exoware_retain_segments: Some(10),
                archive_exoware_timeout: Duration::from_millis(750),
                archive_exoware_prune_grace_segments: 3,
                archive_exoware_export_batch_blocks: 32,
            },
            "every flag lands in its field"
        );
        let config = args.archive_config(8453, B256::repeat_byte(1)).expect("aligned config");
        assert_eq!(config.start_segment, 2, "start block 128 is segment 2 of 64 blocks");
        assert_eq!(config.retain_segments, Some(10), "retention carried over");
        assert_eq!(args.publisher_options().prune_grace_segments, 3, "grace carried over");
        assert_eq!(args.publisher_options().export_batch_blocks, 32, "export batch carried over");
        assert_eq!(
            off.archive_exoware_export_batch_blocks, 256,
            "export batch defaults to 256 blocks"
        );
    }

    /// Out-of-range values, options without a url, an unaligned start and the removed audit
    /// flag are refused.
    #[test]
    fn invalid_flags_are_refused() {
        let url = ["--archive.exoware.url", "http://x"];
        for (flags, why) in [
            (vec!["--archive.exoware.export"], "export without a url"),
            (vec!["--archive.exoware.provision"], "provision without a url"),
            (vec![url[0], url[1], "--archive.exoware.segment-blocks", "0"], "zero segment size"),
            (
                vec![url[0], url[1], "--archive.exoware.segment-blocks", "65537"],
                "segment above 65536",
            ),
            (vec![url[0], url[1], "--archive.exoware.prune-grace-segments", "0"], "zero grace"),
            (
                vec![url[0], url[1], "--archive.exoware.export-batch-blocks", "0"],
                "zero export batch",
            ),
            (vec!["--archive.exoware.export-batch-blocks", "8"], "export batch without a url"),
            (vec![url[0], url[1], "--archive.exoware.retain-segments", "0"], "zero retention"),
            (
                vec![url[0], url[1], "--archive.exoware.audit-interval", "1m"],
                "audit interval was dropped",
            ),
            (vec![url[0], url[1], "--archive.exoware.api-key", "k"], "no api key flag"),
        ] {
            assert!(parse(&flags).is_err(), "{why} must be refused");
        }
        let unaligned = parse(&[
            url[0],
            url[1],
            "--archive.exoware.segment-blocks",
            "64",
            "--archive.exoware.start-block",
            "100",
        ])
        .expect("parses");
        let err = unaligned.archive_config(1, B256::ZERO).expect_err("unaligned start");
        assert!(
            matches!(err, ArchiveError::InvalidConfig { .. }),
            "unaligned start is an invalid config: {err}"
        );
    }
}
