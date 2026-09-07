#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_extern_crates))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![allow(
    clippy::incompatible_msrv,
    clippy::needless_update,
    clippy::collapsible_if,
    unreachable_pub,
    dead_code,
    missing_docs,
    missing_debug_implementations
)]

pub mod btest;
pub mod compiled;
pub mod diagnostic;
pub mod merkle_trie;
pub mod runner;
pub mod utils;

use std::path::{Path, PathBuf};

use walkdir::{DirEntry, WalkDir};

/// Find all JSON test files in the given path.
/// If path is a file, returns it in a vector.
/// If path is a directory, recursively finds all .json files.
pub fn find_all_json_tests(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        vec![path.to_path_buf()]
    } else {
        WalkDir::new(path)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension() == Some("json".as_ref()))
            .map(DirEntry::into_path)
            .collect()
    }
}

/// Default path to ethereum/tests repository.
const DEFAULT_ETHTESTS_PATH: &str = "tests/ethereum-tests";

/// Default path to downloaded test fixtures (execution-spec-tests + legacytests).
const DEFAULT_FIXTURES_PATH: &str = "test-fixtures";

const STATE_TESTS_TARBALL: &str = "fixtures_general_state_tests.tgz";

/// Resolve the workspace root by walking up from `CARGO_MANIFEST_DIR`.
fn workspace_root() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    Path::new(manifest_dir)
        .ancestors()
        .find(|p| p.join("Cargo.lock").exists())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Get the path to ethereum/tests.
pub fn get_ethtests_path() -> PathBuf {
    if let Ok(path) = std::env::var("ETHTESTS") {
        return PathBuf::from(path);
    }
    let root = workspace_root();
    let path = root.join(DEFAULT_ETHTESTS_PATH);
    if path.exists() { path } else { PathBuf::from(DEFAULT_ETHTESTS_PATH) }
}

/// Get the path to downloaded test fixtures (`test-fixtures/`).
pub fn get_fixtures_path() -> PathBuf {
    if let Ok(path) = std::env::var("REVMC_TEST_FIXTURES") {
        return PathBuf::from(path);
    }
    workspace_root().join(DEFAULT_FIXTURES_PATH)
}

/// Get the path to `GeneralStateTests`, extracting the tarball if necessary.
///
/// The ethereum/tests repo ships the fixtures as `.tgz` archives.
/// We extract into the parent of the submodule (`tests/`) rather than inside
/// the submodule itself, so the root `.gitignore` can cover the extracted
/// directory and the submodule stays clean.
pub fn get_general_state_tests_path() -> Option<PathBuf> {
    let root = get_ethtests_path();

    // Check next to the submodule first (extracted location).
    if let Some(parent) = root.parent() {
        let dir = parent.join("GeneralStateTests");
        if dir.is_dir() {
            return Some(dir);
        }
    }

    // Also check inside the submodule (manual extraction or old layout).
    let dir = root.join("GeneralStateTests");
    if dir.is_dir() {
        return Some(dir);
    }

    let tarball = root.join(STATE_TESTS_TARBALL);
    if !tarball.is_file() {
        return None;
    }

    // Extract next to the submodule so the root .gitignore covers it.
    let extract_dir = root.parent()?;
    let status = std::process::Command::new("tar")
        .arg("xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(extract_dir)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }

    let dir = extract_dir.join("GeneralStateTests");
    dir.is_dir().then_some(dir)
}

/// Get the path to execution-spec-tests stable state tests.
pub fn get_exec_spec_stable_state_tests_path() -> Option<PathBuf> {
    let dir = get_fixtures_path().join("main/stable/state_tests");
    dir.is_dir().then_some(dir)
}

/// Get the path to execution-spec-tests develop state tests.
pub fn get_exec_spec_develop_state_tests_path() -> Option<PathBuf> {
    let dir = get_fixtures_path().join("main/develop/state_tests");
    dir.is_dir().then_some(dir)
}

/// Get the path to legacy Cancun GeneralStateTests.
pub fn get_legacy_cancun_state_tests_path() -> Option<PathBuf> {
    let dir = get_fixtures_path().join("legacytests/Cancun/GeneralStateTests");
    dir.is_dir().then_some(dir)
}

/// Get the path to legacy Constantinople GeneralStateTests.
pub fn get_legacy_constantinople_state_tests_path() -> Option<PathBuf> {
    let dir = get_fixtures_path().join("legacytests/Constantinople/GeneralStateTests");
    dir.is_dir().then_some(dir)
}

/// Get the path to execution-spec-tests stable blockchain tests.
pub fn get_exec_spec_stable_blockchain_tests_path() -> Option<PathBuf> {
    let dir = get_fixtures_path().join("main/stable/blockchain_tests");
    dir.is_dir().then_some(dir)
}

/// Get the path to execution-spec-tests develop blockchain tests.
pub fn get_exec_spec_develop_blockchain_tests_path() -> Option<PathBuf> {
    let dir = get_fixtures_path().join("main/develop/blockchain_tests");
    dir.is_dir().then_some(dir)
}
