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
        let mut extensions = BTreeMap::new();
        extensions.insert("custom".to_string(), serde_json::json!({ "enabled": true }));
        let populated = SnapshotManifest {
            block: 100,
            chain_id: 8453,
            storage_version: 2,
            timestamp: 1_700_000_000,
            base_url: Some("https://example.com".to_string()),
            reth_version: Some("2.8.1".to_string()),
            components,
            extensions,
        };
        let minimal = SnapshotManifest {
            block: 0,
            chain_id: 8453,
            storage_version: 2,
            timestamp: 0,
            base_url: None,
            reth_version: None,
            components: BTreeMap::from([(
                "state".to_string(),
                ComponentManifest::Single(SingleArchive {
                    file: "0/state.tar.zst".to_string(),
                    size: 0,
                    decompressed_size: 0,
                    blake3: None,
                    output_files: Vec::new(),
                }),
            )]),
            extensions: BTreeMap::new(),
        };
        for local in [populated, minimal] {
            let local_val = serde_json::to_value(&local).unwrap();
            let reth: reth_cli_commands::download::manifest::SnapshotManifest =
                serde_json::from_value(local_val.clone()).unwrap();
            let reth_val = serde_json::to_value(&reth).unwrap();
            assert_eq!(local_val, reth_val);
            let roundtrip: SnapshotManifest = serde_json::from_value(reth_val).unwrap();
            assert_eq!(serde_json::to_value(&roundtrip).unwrap(), local_val);
        }
    }
}
