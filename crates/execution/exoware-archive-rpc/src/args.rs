//! `--archive.exoware.*` command-line arguments.

use std::time::Duration;

use alloy_primitives::B256;
use base_exoware_archive::{
    ArchiveConfig, ArchiveError, DEFAULT_EXPORT_BATCH_BLOCKS, DEFAULT_NAMESPACE,
    DEFAULT_PRUNE_GRACE_SEGMENTS, DEFAULT_STORE_TIMEOUT, MAX_BLOCKS_PER_SEGMENT, PublisherOptions,
};
use reth_prune_types::{PruneMode, PruneModes, PruneSegment};

/// Default blocks per segment.
pub const DEFAULT_SEGMENT_BLOCKS: u64 = 8_192;

/// Default `--archive.exoware.min-retention-margin`, in blocks. The archive's head trails the
/// node's head by the finality lag plus up to one segment, and the reader falls back to the
/// node's own history at that head, so the node must keep history past it. L1 finality is about
/// two epochs (~13 minutes); 4096 blocks covers that at Base's fastest block time (200 ms) and is
/// generous at 2 s blocks.
pub const DEFAULT_MIN_RETENTION_MARGIN: u64 = 4_096;

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

    /// Blocks of history the node must keep beyond one segment for the archive to serve it:
    /// startup is refused when account history, storage history, bodies, receipts or the
    /// transaction lookup is pruned fully or at a distance of at most segment blocks plus this
    /// margin (which covers the finality lag of the archive's head).
    #[arg(
        long = "archive.exoware.min-retention-margin",
        id = "archive_exoware_min_retention_margin",
        default_value_t = DEFAULT_MIN_RETENTION_MARGIN,
        requires = "archive_exoware_url"
    )]
    pub archive_exoware_min_retention_margin: u64,
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
            archive_exoware_min_retention_margin: DEFAULT_MIN_RETENTION_MARGIN,
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

    /// Refuses a node whose retention the archive cannot cover. The reader serves blocks up to
    /// the archive's head and reads keys unchanged since then from the node's own history at
    /// that head, which trails the node's head by the finality lag plus up to one segment. So
    /// each of account history, storage history, bodies, receipts and the transaction lookup
    /// must be unpruned, pruned before a fixed block, or kept for more than segment blocks plus
    /// `--archive.exoware.min-retention-margin` blocks. Full pruning is refused.
    pub fn validate_retention(&self, modes: &PruneModes) -> Result<(), ArchiveError> {
        let required = self
            .archive_exoware_segment_blocks
            .saturating_add(self.archive_exoware_min_retention_margin);
        let segments = [
            (PruneSegment::AccountHistory, modes.account_history),
            (PruneSegment::StorageHistory, modes.storage_history),
            (PruneSegment::Bodies, modes.bodies_history),
            (PruneSegment::Receipts, modes.receipts),
            (PruneSegment::TransactionLookup, modes.transaction_lookup),
        ];
        for (segment, mode) in segments {
            let kept = match mode {
                Some(PruneMode::Full) => "fully pruned".to_string(),
                Some(PruneMode::Distance(distance)) if distance <= required => {
                    format!("kept for {distance} blocks")
                }
                None | Some(PruneMode::Before(_) | PruneMode::Distance(_)) => continue,
            };
            return Err(ArchiveError::InvalidConfig {
                reason: format!(
                    "{segment} is {kept}; the archive needs more than {required} blocks \
                     (--archive.exoware.segment-blocks {} + --archive.exoware.min-retention-margin {})",
                    self.archive_exoware_segment_blocks, self.archive_exoware_min_retention_margin
                ),
            });
        }
        Ok(())
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
            "--archive.exoware.min-retention-margin",
            "40",
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
                archive_exoware_min_retention_margin: 40,
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
        assert_eq!(
            off.archive_exoware_min_retention_margin, 4_096,
            "the retention margin defaults to 4096 blocks"
        );
    }

    /// Retention the archive covers is accepted: unpruned, pruned before a fixed block, or kept
    /// for more than segment blocks plus the margin. Each of the five segments is refused when
    /// fully pruned or kept for at most that many blocks, with the segment and numbers named.
    #[test]
    fn retention_shorter_than_the_archive_lag_is_refused() {
        let args = ExowareArchiveArgs {
            archive_exoware_url: Some("http://x".into()),
            archive_exoware_segment_blocks: 8,
            archive_exoware_min_retention_margin: 32,
            ..ExowareArchiveArgs::default()
        };
        assert!(args.validate_retention(&PruneModes::default()).is_ok(), "unpruned is accepted");
        let kept = |mode| PruneModes {
            account_history: Some(mode),
            storage_history: Some(mode),
            bodies_history: Some(mode),
            receipts: Some(mode),
            transaction_lookup: Some(mode),
            ..PruneModes::default()
        };
        assert!(args.validate_retention(&kept(PruneMode::Distance(41))).is_ok(), "41 > 8 + 32");
        assert!(
            args.validate_retention(&kept(PruneMode::Before(5))).is_ok(),
            "pruning before a fixed block is accepted"
        );
        let reason = |segment: &str, kept: &str| {
            format!(
                "invalid archive config: {segment} is {kept}; the archive needs more than 40 blocks \
                 (--archive.exoware.segment-blocks 8 + --archive.exoware.min-retention-margin 32)"
            )
        };
        /// Sets one prune segment's mode.
        type SetMode = fn(&mut PruneModes, PruneMode);
        let set: [(&str, SetMode); 5] = [
            ("AccountHistory", |m, v| m.account_history = Some(v)),
            ("StorageHistory", |m, v| m.storage_history = Some(v)),
            ("Bodies", |m, v| m.bodies_history = Some(v)),
            ("Receipts", |m, v| m.receipts = Some(v)),
            ("TransactionLookup", |m, v| m.transaction_lookup = Some(v)),
        ];
        for (segment, apply) in set {
            for (mode, kept) in [
                (PruneMode::Distance(40), "kept for 40 blocks"),
                (PruneMode::Distance(3), "kept for 3 blocks"),
                (PruneMode::Full, "fully pruned"),
            ] {
                let mut modes = PruneModes::default();
                apply(&mut modes, mode);
                let err = args.validate_retention(&modes).expect_err("short retention");
                assert_eq!(err.to_string(), reason(segment, kept), "{segment} {mode:?} is refused");
            }
        }
    }

    /// Out-of-range values, options without a url, an unaligned start and the removed audit
    /// flag are refused.
    #[test]
    fn invalid_flags_are_refused() {
        use clap::error::ErrorKind::{MissingRequiredArgument, UnknownArgument, ValueValidation};
        let url = ["--archive.exoware.url", "http://x"];
        for (flags, kind, why) in [
            (vec!["--archive.exoware.export"], MissingRequiredArgument, "export without a url"),
            (
                vec!["--archive.exoware.provision"],
                MissingRequiredArgument,
                "provision without a url",
            ),
            (
                vec![url[0], url[1], "--archive.exoware.segment-blocks", "0"],
                ValueValidation,
                "zero segment size",
            ),
            (
                vec![url[0], url[1], "--archive.exoware.segment-blocks", "65537"],
                ValueValidation,
                "segment above 65536",
            ),
            (
                vec![url[0], url[1], "--archive.exoware.prune-grace-segments", "0"],
                ValueValidation,
                "zero grace",
            ),
            (
                vec![url[0], url[1], "--archive.exoware.export-batch-blocks", "0"],
                ValueValidation,
                "zero export batch",
            ),
            (
                vec!["--archive.exoware.export-batch-blocks", "8"],
                MissingRequiredArgument,
                "export batch without a url",
            ),
            (
                vec!["--archive.exoware.min-retention-margin", "8"],
                MissingRequiredArgument,
                "retention margin without a url",
            ),
            (
                vec![url[0], url[1], "--archive.exoware.retain-segments", "0"],
                ValueValidation,
                "zero retention",
            ),
            (
                vec![url[0], url[1], "--archive.exoware.audit-interval", "1m"],
                UnknownArgument,
                "audit interval was dropped",
            ),
            (
                vec![url[0], url[1], "--archive.exoware.api-key", "k"],
                UnknownArgument,
                "no api key flag",
            ),
        ] {
            let err = parse(&flags).expect_err(why);
            assert_eq!(err.kind(), kind, "{why} is refused as {kind:?}: {err}");
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
        assert_eq!(
            err.to_string(),
            "invalid archive config: --archive.exoware.start-block 100 is not a multiple of \
             --archive.exoware.segment-blocks 64",
            "unaligned start is an invalid config"
        );
    }
}
