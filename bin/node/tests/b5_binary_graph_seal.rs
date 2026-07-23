//! Deterministic source/manifest seal for the dormant B5 node feature link.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("bin/node must be nested directly below the workspace root")
        .to_path_buf()
}

fn read(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

fn section(manifest: &str, name: &str) -> String {
    let header = format!("[{name}]");
    let start = manifest
        .lines()
        .position(|line| line.trim() == header)
        .unwrap_or_else(|| panic!("missing manifest section {header}"));
    let lines: Vec<_> = manifest.lines().collect();
    let end = lines[start + 1..]
        .iter()
        .position(|line| line.trim_start().starts_with('['))
        .map_or(lines.len(), |offset| start + 1 + offset);
    lines[start + 1..end].join("\n")
}

fn manifest_value(section: &str, key: &str) -> String {
    let lines: Vec<_> = section.lines().collect();
    let (start, first_value) = lines
        .iter()
        .enumerate()
        .find_map(|(index, line)| {
            let (candidate, value) = line.split_once('=')?;
            (candidate.trim() == key).then_some((index, value))
        })
        .unwrap_or_else(|| panic!("missing manifest key {key}"));

    let mut value = first_value.to_owned();
    let mut square_depth = bracket_delta(first_value);
    for line in &lines[start + 1..] {
        if square_depth == 0 {
            break;
        }
        value.push_str(line);
        square_depth += bracket_delta(line);
    }
    assert_eq!(square_depth, 0, "unclosed array for manifest key {key}");
    value.chars().filter(|character| !character.is_whitespace()).collect()
}

fn bracket_delta(value: &str) -> i32 {
    value.chars().fold(0, |depth, character| match character {
        '[' => depth + 1,
        ']' => depth - 1,
        _ => depth,
    })
}

fn rust_sources_below(directory: &Path) -> Vec<PathBuf> {
    fn canonical_path_below(path: &Path, root: &Path) -> PathBuf {
        let canonical = fs::canonicalize(path)
            .unwrap_or_else(|error| panic!("failed to canonicalize {}: {error}", path.display()));
        assert!(
            canonical.starts_with(root),
            "{} escapes source root {}",
            path.display(),
            root.display()
        );
        canonical
    }

    fn visit(
        directory: &Path,
        root: &Path,
        visited_directories: &mut HashSet<PathBuf>,
        sources: &mut Vec<PathBuf>,
    ) {
        let metadata = fs::symlink_metadata(directory)
            .unwrap_or_else(|error| panic!("failed to inspect {}: {error}", directory.display()));
        assert!(
            !metadata.file_type().is_symlink(),
            "source traversal rejects symlink directory {}",
            directory.display()
        );
        assert!(metadata.is_dir(), "{} is not a directory", directory.display());
        let canonical_directory = canonical_path_below(directory, root);
        assert!(
            visited_directories.insert(canonical_directory),
            "source traversal revisited directory {}",
            directory.display()
        );

        let mut entries: Vec<_> = fs::read_dir(directory)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", directory.display()))
            .map(|entry| entry.expect("source directory entry"))
            .collect();
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .unwrap_or_else(|error| panic!("failed to inspect {}: {error}", path.display()));
            assert!(
                !metadata.file_type().is_symlink(),
                "source traversal rejects symlink entry {}",
                path.display()
            );
            canonical_path_below(&path, root);
            if metadata.is_dir() {
                visit(&path, root, visited_directories, sources);
            } else if metadata.is_file()
                && path.extension().is_some_and(|extension| extension == "rs")
            {
                sources.push(path);
            }
        }
    }

    let metadata = fs::symlink_metadata(directory)
        .unwrap_or_else(|error| panic!("failed to inspect {}: {error}", directory.display()));
    assert!(
        !metadata.file_type().is_symlink(),
        "source traversal rejects symlink root {}",
        directory.display()
    );
    let root = fs::canonicalize(directory)
        .unwrap_or_else(|error| panic!("failed to canonicalize {}: {error}", directory.display()));
    let mut visited_directories = HashSet::new();
    let mut sources = Vec::new();
    visit(directory, &root, &mut visited_directories, &mut sources);
    sources
}

#[test]
fn dormant_node_link_is_exactly_opt_in_and_non_operational() {
    let root = workspace_root();
    let root_manifest = read(&root.join("Cargo.toml"));
    let node_manifest = read(&root.join("bin/node/Cargo.toml"));
    let cli_manifest = read(&root.join("crates/execution/cli/Cargo.toml"));

    let root_dependencies = section(&root_manifest, "workspace.dependencies");
    let root_cli = manifest_value(&root_dependencies, "base-execution-cli");
    assert_eq!(root_cli, r#"{path="crates/execution/cli"}"#);
    assert!(!root_cli.contains("features"), "workspace registration must not enable features",);

    let node_dependencies = section(&node_manifest, "dependencies");
    assert_eq!(
        manifest_value(&node_dependencies, "base-execution-cli"),
        r#"{workspace=true,features=["otlp"]}"#,
    );

    let node_features = section(&node_manifest, "features");
    assert_eq!(manifest_value(&node_features, "default"), r#"["jemalloc"]"#);
    assert_eq!(
        manifest_value(&node_features, "b5-dormant-presign"),
        r#"["base-execution-cli/b5-dormant-presign"]"#,
    );

    let cli_dependencies = section(&cli_manifest, "dependencies");
    for dependency in ["mev-trader-submit", "sha2", "serde", "serde_json", "libc"] {
        assert_eq!(
            manifest_value(&cli_dependencies, dependency),
            "{workspace=true,optional=true}",
            "B5 dependency {dependency} must remain workspace-scoped and optional",
        );
    }

    let cli_features = section(&cli_manifest, "features");
    assert_eq!(manifest_value(&cli_features, "default"), "[]");
    assert_eq!(
        manifest_value(&cli_features, "b5-dormant-presign"),
        concat!(
            r#"["dep:mev-trader-submit","mev-trader-submit/presign","dep:sha2","#,
            r#""dep:serde","dep:serde_json","dep:libc",]"#,
        ),
    );

    let node_source_directory = root.join("bin/node/src");
    for source_path in rust_sources_below(&node_source_directory) {
        let source = read(&source_path);
        assert!(
            !source.to_ascii_lowercase().contains("b5"),
            "{} must remain free of B5 runtime wiring",
            source_path.display(),
        );
    }

    let private_child = root.join("crates/execution/cli/src/mev_trader/b5_dormant.rs");
    let private_source = read(&private_child);
    assert_eq!(
        private_source.matches("fn verify_provisioning_bindings_against(").count(),
        1,
        "the verifier must have exactly one child-private definition",
    );
    assert!(!private_source.contains("pub fn verify_provisioning_bindings_against"));
    let private_source_lower = private_source.to_ascii_lowercase();
    for forbidden in [
        "const commit_b_reviewed",
        "static commit_b_reviewed",
        "reviewed_provisioning_authority",
        ".sign(",
        ".request(",
        ".submit(",
        ".broadcast(",
        "reqwest",
        "std::fs",
        "std::net",
    ] {
        assert!(
            !private_source_lower.contains(forbidden),
            "dormant private source gained operational capability: {forbidden}",
        );
    }
    let cli_source_directory = root.join("crates/execution/cli/src");
    let mut external_cli_source = String::new();
    for source_path in rust_sources_below(&cli_source_directory) {
        if source_path == private_child {
            continue;
        }
        let source = read(&source_path);
        for (line_number, line) in source.lines().enumerate() {
            if line.to_ascii_lowercase().contains("b5") {
                assert!(
                    source_path.ends_with("lib.rs") || source_path.ends_with("mev_trader.rs"),
                    "unexpected B5 source wiring at {}:{}",
                    source_path.display(),
                    line_number + 1,
                );
                let wiring_only = line
                    .replace("b5-dormant-presign", "")
                    .replace("b5_dormant", "")
                    .replace("B5-1a", "")
                    .to_ascii_lowercase();
                for forbidden in [
                    "verify",
                    "reviewed",
                    "authority",
                    "sign(",
                    "request(",
                    "submit(",
                    "broadcast(",
                ] {
                    assert!(
                        !wiring_only.contains(forbidden),
                        "B5 wiring gained operational capability at {}:{}",
                        source_path.display(),
                        line_number + 1,
                    );
                }
            }
        }
        external_cli_source.push_str(&source);
        external_cli_source.push('\n');
    }

    assert_eq!(external_cli_source.matches("b5-dormant-presign").count(), 3);
    assert_eq!(external_cli_source.matches("mod b5_dormant;").count(), 1);
    for forbidden in [
        "verify_provisioning_bindings_against",
        "CommitBReviewedProvisioningBinding",
        "AuthenticatedProvisioningSnapshot",
        "mev_trader_submit",
    ] {
        assert!(
            !external_cli_source.contains(forbidden),
            "private B5 surface escaped its dormant child: {forbidden}",
        );
    }
}
