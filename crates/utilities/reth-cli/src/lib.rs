#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod version;
pub use version::Version;

mod engine;
pub use engine::Engine;

mod snapshots;
pub use base_snapshot_manifest::{
    ChunkFilename, ChunkedArchive, ComponentManifest, DEFAULT_BLOCKS_PER_FILE,
    DirectoryArchiveSink, ManifestGenerationParams, OutputFileChecksum, ProgressDisplay,
    SingleArchive, SnapshotArchiveSink, SnapshotArchiveWriter, SnapshotGenerator, SnapshotManifest,
    SnapshotManifestExt,
};
pub use snapshots::Snapshots;

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        ChunkedArchive, ComponentManifest, OutputFileChecksum, SingleArchive, SnapshotManifest,
    };

    #[test]
    fn snapshot_manifest_matches_reth_wire_format() {
        let mut components = BTreeMap::new();
        components.insert(
            "state".to_string(),
            ComponentManifest::Single(SingleArchive {
                file: "100/state.tar.zst".to_string(),
                size: 42,
                decompressed_size: 100,
                blake3: Some("abcd".to_string()),
                output_files: vec![OutputFileChecksum {
                    path: "db/mdbx.dat".to_string(),
                    size: 100,
                    blake3: "ef01".to_string(),
                }],
            }),
        );
        components.insert(
            "headers".to_string(),
            ComponentManifest::Chunked(ChunkedArchive {
                blocks_per_file: 500_000,
                total_blocks: 500_000,
                chunk_sizes: vec![10],
                chunk_decompressed_sizes: vec![20],
                chunk_files: vec!["static_files/headers-0-499999.tar.zst".to_string()],
                chunk_output_files: vec![vec![OutputFileChecksum {
                    path: "static_files/static_file_headers_0_499999".to_string(),
                    size: 20,
                    blake3: "1234".to_string(),
                }]],
            }),
        );
        let local = SnapshotManifest {
            block: 100,
            chain_id: 8453,
            storage_version: 2,
            timestamp: 1_700_000_000,
            base_url: Some("https://example.com".to_string()),
            reth_version: Some("2.8.1".to_string()),
            components,
            extensions: BTreeMap::new(),
        };
        let json = serde_json::to_string(&local).unwrap();
        let reth: reth_cli_commands::download::manifest::SnapshotManifest =
            serde_json::from_str(&json).unwrap();
        let roundtrip: SnapshotManifest =
            serde_json::from_str(&serde_json::to_string(&reth).unwrap()).unwrap();
        assert_eq!(roundtrip.block, local.block);
        assert_eq!(roundtrip.chain_id, local.chain_id);
        assert_eq!(roundtrip.reth_version, local.reth_version);
        assert_eq!(roundtrip.components.len(), 2);
    }
}
