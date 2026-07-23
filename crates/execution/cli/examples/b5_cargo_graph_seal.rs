//! Evidence-only Cargo graph seal tool for the B5-1a P0/P1 history lifecycle.
//!
//! Subcommands: `normalize`, `seal-registration`, `seal-implementation`, `verify`.
//! Every flag is required and explicit; unknown, duplicate, or omitted flags and
//! unlisted evidence paths fail closed. Outputs are RFC 8785 (JCS) canonical JSON
//! written create-new (`O_CREAT|O_EXCL`); nothing is overwritten. This example is
//! never imported by production code and performs no network, signing, submission,
//! or node operation.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

const RAW_SEAL_SCHEMA: &str = "base-mev/b5-cargo-registration-raw-seal/v1";
const ROOTED_SCHEMA: &str = "base-mev/b5-cargo-rooted-metadata/v2";
const REGISTRATION_SEAL_SCHEMA: &str = "base-mev/b5-cargo-registration-canonical-seal/v1";
const IMPLEMENTATION_SEAL_SCHEMA: &str = "base-mev/b5-cargo-implementation-seal/v1";
const VERIFY_SCHEMA: &str = "base-mev/b5-cargo-history-verify/v1";

const TARGET_TRIPLE: &str = "x86_64-unknown-linux-gnu";
const VERDICT_PASS: &str = "PASS";

const REF_P0_PARENT: &str = "refs/gjc/b5-1a/p0-parent";
const REF_P0: &str = "refs/gjc/b5-1a/p0";
const REF_P1_PARENT: &str = "refs/gjc/b5-1a/p1-parent";
const REF_P1: &str = "refs/gjc/b5-1a/p1";

const MANIFEST_CLI: &str = "crates/execution/cli/Cargo.toml";
const MANIFEST_NODE: &str = "bin/node/Cargo.toml";
const PACKAGE_CLI: &str = "base-execution-cli";
const PACKAGE_NODE: &str = "base-reth-node";
const SELECTION_DEFAULT: &str = "default";
const SELECTION_PRESIGN: &str = "b5-dormant-presign";

const SUBMIT_NAME: &str = "mev-trader-submit";
const SUBMIT_VERSION: &str = "1.1.1";
const CLI_LOCK_STANZA_VERSION: &str = "1.1.1";
const SUBMIT_MANIFEST: &str = "crates/execution/mev-trader-submit/Cargo.toml";
const SUBMIT_PRESIGN_FEATURE: &str = "presign";
const SUBMIT_PRESIGN_DIRECT_ALLOWLIST: [&str; 2] = ["alloy-primitives", "sha2"];
const PROHIBITED_SELECTED_DELTA_NAMES: [&str; 3] = ["k256", "redb", "reqwest"];

const RAW_SEAL_PATH: &str = "target/b5-1a-cargo-history/p0-registration-raw-seal-v1.json";
const REGISTRATION_SEAL_PATH: &str =
    "target/b5-1a-cargo-history/p0-registration-canonical-seal-v1.json";
const IMPLEMENTATION_SEAL_PATH: &str =
    "target/b5-1a-cargo-history/p1-implementation-graph-lock-delta-seal-v1.json";
const VERIFY_OUTPUT_PATH: &str = "target/b5-1a-cargo-history/b5-cargo-history-verify-v1.json";

const P0_PARENT_CLI_RAW: &str =
    "target/b5-1a-cargo-history/p0-parent/raw/base-execution-cli.default.metadata.json";
const P0_PARENT_NODE_RAW: &str =
    "target/b5-1a-cargo-history/p0-parent/raw/base-reth-node.default.metadata.json";
const P0_PARENT_LOCK: &str = "target/b5-1a-cargo-history/p0-parent/raw/Cargo.lock";
const P0_PARENT_LOCK_SIDECAR: &str = "target/b5-1a-cargo-history/p0-parent/raw/Cargo.lock.sha256";
const P0_CLI_RAW: &str =
    "target/b5-1a-cargo-history/p0/raw/base-execution-cli.default.metadata.json";
const P0_NODE_RAW: &str = "target/b5-1a-cargo-history/p0/raw/base-reth-node.default.metadata.json";
const P0_LOCK: &str = "target/b5-1a-cargo-history/p0/raw/Cargo.lock";
const P0_LOCK_SIDECAR: &str = "target/b5-1a-cargo-history/p0/raw/Cargo.lock.sha256";

const P1_PARENT_CLI_RAW: &str =
    "target/b5-1a-cargo-history/p1-parent/raw/base-execution-cli.default.metadata.json";
const P1_PARENT_NODE_RAW: &str =
    "target/b5-1a-cargo-history/p1-parent/raw/base-reth-node.default.metadata.json";
const P1_PARENT_LOCK: &str = "target/b5-1a-cargo-history/p1-parent/raw/Cargo.lock";
const P1_PARENT_LOCK_SIDECAR: &str = "target/b5-1a-cargo-history/p1-parent/raw/Cargo.lock.sha256";
const P1_CLI_RAW: &str =
    "target/b5-1a-cargo-history/p1/raw/base-execution-cli.default.metadata.json";
const P1_NODE_RAW: &str = "target/b5-1a-cargo-history/p1/raw/base-reth-node.default.metadata.json";
const P1_LOCK: &str = "target/b5-1a-cargo-history/p1/raw/Cargo.lock";
const P1_LOCK_SIDECAR: &str = "target/b5-1a-cargo-history/p1/raw/Cargo.lock.sha256";
const P1_SELECTED_CLI_RAW: &str =
    "target/b5-1a-cargo-history/p1/raw/base-execution-cli.b5-dormant-presign.metadata.json";

const P0_PARENT_CLI_NORMALIZED: &str =
    "target/b5-1a-cargo-history/p0-parent/normalized/base-execution-cli.default.rooted.json";
const P0_PARENT_NODE_NORMALIZED: &str =
    "target/b5-1a-cargo-history/p0-parent/normalized/base-reth-node.default.rooted.json";
const P0_CLI_NORMALIZED: &str =
    "target/b5-1a-cargo-history/p0/normalized/base-execution-cli.default.rooted.json";
const P0_NODE_NORMALIZED: &str =
    "target/b5-1a-cargo-history/p0/normalized/base-reth-node.default.rooted.json";
const P1_PARENT_CLI_NORMALIZED: &str =
    "target/b5-1a-cargo-history/p1-parent/normalized/base-execution-cli.default.rooted.json";
const P1_PARENT_NODE_NORMALIZED: &str =
    "target/b5-1a-cargo-history/p1-parent/normalized/base-reth-node.default.rooted.json";
const P1_CLI_NORMALIZED: &str =
    "target/b5-1a-cargo-history/p1/normalized/base-execution-cli.default.rooted.json";
const P1_NODE_NORMALIZED: &str =
    "target/b5-1a-cargo-history/p1/normalized/base-reth-node.default.rooted.json";
const P1_SELECTED_CLI_NORMALIZED: &str =
    "target/b5-1a-cargo-history/p1/normalized/base-execution-cli.b5-dormant-presign.rooted.json";

const P0_RAW_SEAL_INPUT_PATHS: [&str; 8] = [
    P0_PARENT_CLI_RAW,
    P0_PARENT_NODE_RAW,
    P0_PARENT_LOCK,
    P0_PARENT_LOCK_SIDECAR,
    P0_CLI_RAW,
    P0_NODE_RAW,
    P0_LOCK,
    P0_LOCK_SIDECAR,
];

const EXPECTED_CLI_LOCK_INSERTIONS: [&str; 5] = [
    " \"libc\",",
    " \"mev-trader-submit\",",
    " \"serde\",",
    " \"serde_json\",",
    " \"sha2 0.10.9\",",
];
const EXPECTED_SUBMIT_LOCK_INSERTION: &str = " \"sha2 0.10.9\",";

/// Closed fail-closed error surface; variants expose the failure class only.
#[derive(Debug)]
enum SealError {
    ArgsNotUtf8,
    MissingSubcommand,
    UnknownSubcommand,
    UnknownFlag,
    DuplicateFlag,
    MissingFlagValue,
    OmittedFlag,
    UnlistedFlagValue,
    CheckoutRootInvalid,
    CheckoutRootMismatch,
    GitInvocationFailed,
    GitRefResolutionFailed,
    CaptureCommitMismatch,
    CargoVersionMismatch,
    CommitConsistencyMismatch,
    ReadInputFailed,
    EvidencePathInvalid,
    CreateOutputFailed,
    WriteOutputFailed,
    JsonParseFailed,
    NotJsonObject,
    NotCanonicalJson,
    JsonNumberNotInteger,
    SchemaFieldSetMismatch,
    SchemaValueMismatch,
    VerdictNotPass,
    BooleanNotTrue,
    HexFormatInvalid,
    ByteLenFormatInvalid,
    PathFormatInvalid,
    FileBindingMismatch,
    RefsSectionInvalid,
    RawEqualityFailed,
    SidecarFormatInvalid,
    SidecarDigestMismatch,
    MetadataShapeInvalid,
    WorkspaceRootInvalid,
    RootPackageNotFound,
    RootPackageAmbiguous,
    DuplicateRawPackageId,
    DuplicateWorkspaceMember,
    DuplicateResolveNodeId,
    DepKindInvalid,
    UnresolvedRoot,
    UnresolvedEdge,
    IdentityCollision,
    ManifestPathOutsideRoot,
    ManifestPathSegmentInvalid,
    ExternalSourceMissing,
    DuplicateFeatureEntry,
    FeatureSelectionMismatch,
    ClosureMismatch,
    NormalizedReproductionMismatch,
    LockNotUtf8,
    LockFormatInvalid,
    LockDependencyEntryInvalid,
    LockDuplicateNameUnqualified,
    LockUniverseMismatch,
    LockDeltaMismatch,
    SubmitInDefaultClosure,
    SelectedDeltaMismatch,
    SubmitFeatureSetMismatch,
    SubmitAllowlistMismatch,
    SealReproductionMismatch,
}

/// Which capture ref a normalization row binds to, for raw-seal cross-checks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RefClass {
    P0Parent,
    P0,
    P1Parent,
    P1,
}

/// One admissible `normalize` invocation; the full flag tuple must match a row.
struct NormalizeRow {
    raw: &'static str,
    capture_ref: &'static str,
    root_manifest: &'static str,
    root_package: &'static str,
    feature_selection: &'static str,
    output: &'static str,
    class: RefClass,
}

const ROW_P0_PARENT_CLI: NormalizeRow = NormalizeRow {
    raw: P0_PARENT_CLI_RAW,
    capture_ref: REF_P0_PARENT,
    root_manifest: MANIFEST_CLI,
    root_package: PACKAGE_CLI,
    feature_selection: SELECTION_DEFAULT,
    output: P0_PARENT_CLI_NORMALIZED,
    class: RefClass::P0Parent,
};
const ROW_P0_PARENT_NODE: NormalizeRow = NormalizeRow {
    raw: P0_PARENT_NODE_RAW,
    capture_ref: REF_P0_PARENT,
    root_manifest: MANIFEST_NODE,
    root_package: PACKAGE_NODE,
    feature_selection: SELECTION_DEFAULT,
    output: P0_PARENT_NODE_NORMALIZED,
    class: RefClass::P0Parent,
};
const ROW_P0_CLI: NormalizeRow = NormalizeRow {
    raw: P0_CLI_RAW,
    capture_ref: REF_P0,
    root_manifest: MANIFEST_CLI,
    root_package: PACKAGE_CLI,
    feature_selection: SELECTION_DEFAULT,
    output: P0_CLI_NORMALIZED,
    class: RefClass::P0,
};
const ROW_P0_NODE: NormalizeRow = NormalizeRow {
    raw: P0_NODE_RAW,
    capture_ref: REF_P0,
    root_manifest: MANIFEST_NODE,
    root_package: PACKAGE_NODE,
    feature_selection: SELECTION_DEFAULT,
    output: P0_NODE_NORMALIZED,
    class: RefClass::P0,
};
const ROW_P1_PARENT_CLI: NormalizeRow = NormalizeRow {
    raw: P1_PARENT_CLI_RAW,
    capture_ref: REF_P1_PARENT,
    root_manifest: MANIFEST_CLI,
    root_package: PACKAGE_CLI,
    feature_selection: SELECTION_DEFAULT,
    output: P1_PARENT_CLI_NORMALIZED,
    class: RefClass::P1Parent,
};
const ROW_P1_PARENT_NODE: NormalizeRow = NormalizeRow {
    raw: P1_PARENT_NODE_RAW,
    capture_ref: REF_P1_PARENT,
    root_manifest: MANIFEST_NODE,
    root_package: PACKAGE_NODE,
    feature_selection: SELECTION_DEFAULT,
    output: P1_PARENT_NODE_NORMALIZED,
    class: RefClass::P1Parent,
};
const ROW_P1_CLI: NormalizeRow = NormalizeRow {
    raw: P1_CLI_RAW,
    capture_ref: REF_P1,
    root_manifest: MANIFEST_CLI,
    root_package: PACKAGE_CLI,
    feature_selection: SELECTION_DEFAULT,
    output: P1_CLI_NORMALIZED,
    class: RefClass::P1,
};
const ROW_P1_NODE: NormalizeRow = NormalizeRow {
    raw: P1_NODE_RAW,
    capture_ref: REF_P1,
    root_manifest: MANIFEST_NODE,
    root_package: PACKAGE_NODE,
    feature_selection: SELECTION_DEFAULT,
    output: P1_NODE_NORMALIZED,
    class: RefClass::P1,
};
const ROW_P1_SELECTED_CLI: NormalizeRow = NormalizeRow {
    raw: P1_SELECTED_CLI_RAW,
    capture_ref: REF_P1,
    root_manifest: MANIFEST_CLI,
    root_package: PACKAGE_CLI,
    feature_selection: SELECTION_PRESIGN,
    output: P1_SELECTED_CLI_NORMALIZED,
    class: RefClass::P1,
};

const NORMALIZE_ROWS: [&NormalizeRow; 9] = [
    &ROW_P0_PARENT_CLI,
    &ROW_P0_PARENT_NODE,
    &ROW_P0_CLI,
    &ROW_P0_NODE,
    &ROW_P1_PARENT_CLI,
    &ROW_P1_PARENT_NODE,
    &ROW_P1_CLI,
    &ROW_P1_NODE,
    &ROW_P1_SELECTED_CLI,
];

fn main() -> Result<(), SealError> {
    let mut raw_args = Vec::new();
    for arg in std::env::args_os().skip(1) {
        raw_args.push(arg.into_string().map_err(|_| SealError::ArgsNotUtf8)?);
    }
    let (subcommand, rest) = raw_args.split_first().ok_or(SealError::MissingSubcommand)?;
    match subcommand.as_str() {
        "normalize" => cmd_normalize(rest),
        "seal-registration" => cmd_seal_registration(rest),
        "seal-implementation" => cmd_seal_implementation(rest),
        "verify" => cmd_verify(rest),
        _ => Err(SealError::UnknownSubcommand),
    }
}

/// Strict `--flag value` parser: every listed flag is required exactly once and
/// no unlisted flag or positional argument is accepted.
fn parse_flag_values(args: &[String], names: &[&str]) -> Result<Vec<String>, SealError> {
    let mut values: Vec<Option<String>> = names.iter().map(|_| None).collect();
    let mut index = 0;
    while index < args.len() {
        let flag = args.get(index).ok_or(SealError::UnknownFlag)?;
        let position =
            names.iter().position(|name| *name == flag.as_str()).ok_or(SealError::UnknownFlag)?;
        let value = args.get(index + 1).ok_or(SealError::MissingFlagValue)?;
        if value.starts_with("--") {
            return Err(SealError::MissingFlagValue);
        }
        let slot = values.get_mut(position).ok_or(SealError::UnknownFlag)?;
        if slot.is_some() {
            return Err(SealError::DuplicateFlag);
        }
        *slot = Some(value.clone());
        index += 2;
    }
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        out.push(value.ok_or(SealError::OmittedFlag)?);
    }
    Ok(out)
}

/// Parses flags whose values are all fixed literals from the approved plan.
fn parse_literal_flags(
    args: &[String],
    spec: &[(&'static str, &'static str)],
) -> Result<(), SealError> {
    let names: Vec<&str> = spec.iter().map(|(name, _)| *name).collect();
    let values = parse_flag_values(args, &names)?;
    for ((_, literal), value) in spec.iter().zip(values.iter()) {
        if value.as_str() != *literal {
            return Err(SealError::UnlistedFlagValue);
        }
    }
    Ok(())
}

/// Validated repository root used as the authority for every evidence path.
struct WorkspaceRoot {
    path: PathBuf,
    directory: File,
}

impl WorkspaceRoot {
    fn explicit(text: &str) -> Result<Self, SealError> {
        if !text.starts_with('/') || text.ends_with('/') || text.contains('\0') {
            return Err(SealError::CheckoutRootInvalid);
        }
        let root = Self::validate(Path::new(text))?;
        root.require_git_top_level()?;
        Ok(root)
    }

    fn current() -> Result<Self, SealError> {
        let current = std::env::current_dir().map_err(|_| SealError::CheckoutRootInvalid)?;
        let output = git_command_at(&current)
            .arg("rev-parse")
            .arg("--show-toplevel")
            .output()
            .map_err(|_| SealError::GitInvocationFailed)?;
        let top_level = parse_git_path_output(&output)?;
        Self::validate(Path::new(&top_level))
    }

    fn validate(path: &Path) -> Result<Self, SealError> {
        if !path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
        {
            return Err(SealError::CheckoutRootInvalid);
        }

        let mut directory =
            open_directory(Path::new("/")).map_err(|_| SealError::CheckoutRootInvalid)?;
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(segment) => {
                    directory = open_directory(&fd_child_path(&directory, segment))
                        .map_err(|_| SealError::CheckoutRootInvalid)?;
                }
                _ => return Err(SealError::CheckoutRootInvalid),
            }
        }
        let canonical = std::fs::canonicalize(fd_path(&directory))
            .map_err(|_| SealError::CheckoutRootInvalid)?;
        if canonical != path {
            return Err(SealError::CheckoutRootInvalid);
        }
        Ok(Self { path: canonical, directory })
    }

    fn require_git_top_level(&self) -> Result<(), SealError> {
        let output = self
            .git_command()
            .arg("rev-parse")
            .arg("--show-toplevel")
            .output()
            .map_err(|_| SealError::GitInvocationFailed)?;
        let top_level = parse_git_path_output(&output)?;
        if Path::new(&top_level) != self.path {
            return Err(SealError::CheckoutRootMismatch);
        }
        Ok(())
    }

    fn as_str(&self) -> Result<&str, SealError> {
        self.path.to_str().ok_or(SealError::CheckoutRootInvalid)
    }

    fn git_command(&self) -> std::process::Command {
        git_command_at(&fd_path(&self.directory))
    }

    fn components<'a>(&self, relative: &'a str) -> Result<Vec<&'a str>, SealError> {
        if !is_repo_relative_path(relative) {
            return Err(SealError::EvidencePathInvalid);
        }
        Ok(relative.split('/').collect())
    }

    fn open_parent(&self, relative: &str, create: bool) -> Result<(File, String), SealError> {
        let mut components = self.components(relative)?;
        let leaf = components.pop().ok_or(SealError::EvidencePathInvalid)?;
        let mut directory =
            self.directory.try_clone().map_err(|_| SealError::EvidencePathInvalid)?;

        for component in components {
            let child = fd_child_path(&directory, component);
            directory = match open_directory(&child) {
                Ok(opened) => opened,
                Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
                    match std::fs::create_dir(&child) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(_) => return Err(SealError::CreateOutputFailed),
                    }
                    open_directory(&child).map_err(|_| SealError::EvidencePathInvalid)?
                }
                Err(_) => return Err(SealError::EvidencePathInvalid),
            };
        }
        Ok((directory, leaf.to_string()))
    }

    fn read(&self, relative: &str) -> Result<Vec<u8>, SealError> {
        let (parent, leaf) = self.open_parent(relative, false)?;
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(fd_child_path(&parent, leaf))
            .map_err(|error| {
                if matches!(error.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)) {
                    SealError::EvidencePathInvalid
                } else {
                    SealError::ReadInputFailed
                }
            })?;
        if !file.metadata().map_err(|_| SealError::ReadInputFailed)?.is_file() {
            return Err(SealError::EvidencePathInvalid);
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(|_| SealError::ReadInputFailed)?;
        Ok(bytes)
    }

    /// Create-new (`O_CREAT|O_EXCL`) write rooted below this checkout.
    fn write_create_new(&self, relative: &str, bytes: &[u8]) -> Result<(), SealError> {
        let (parent, leaf) = self.open_parent(relative, true)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(fd_child_path(&parent, leaf))
            .map_err(|_| SealError::CreateOutputFailed)?;
        if !file.metadata().map_err(|_| SealError::CreateOutputFailed)?.is_file() {
            return Err(SealError::EvidencePathInvalid);
        }
        file.write_all(bytes).map_err(|_| SealError::WriteOutputFailed)?;
        file.sync_all().map_err(|_| SealError::WriteOutputFailed)?;
        Ok(())
    }
}

fn fd_path(directory: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
}

fn fd_child_path(directory: &File, component: impl AsRef<Path>) -> PathBuf {
    fd_path(directory).join(component)
}

fn open_directory(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

fn git_command_at(path: &Path) -> std::process::Command {
    let mut command = std::process::Command::new("/usr/bin/git");
    command
        .env_clear()
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("LC_ALL", "C")
        .current_dir(path);
    command
}

fn parse_git_path_output(output: &std::process::Output) -> Result<String, SealError> {
    if !output.status.success()
        || output.stdout.last() != Some(&b'\n')
        || output.stdout[..output.stdout.len().saturating_sub(1)].contains(&b'\n')
    {
        return Err(SealError::GitRefResolutionFailed);
    }
    let path = output
        .stdout
        .get(..output.stdout.len().saturating_sub(1))
        .ok_or(SealError::GitRefResolutionFailed)?;
    let path = std::str::from_utf8(path).map_err(|_| SealError::GitRefResolutionFailed)?;
    if path.is_empty() {
        return Err(SealError::GitRefResolutionFailed);
    }
    Ok(path.to_string())
}

fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

fn is_lower_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_canonical_decimal(text: &str) -> bool {
    if text == "0" {
        return true;
    }
    let mut bytes = text.bytes();
    matches!(bytes.next(), Some(b'1'..=b'9')) && bytes.all(|b| b.is_ascii_digit())
}

fn is_repo_relative_path(text: &str) -> bool {
    if text.is_empty() || text.contains('\\') || text.contains('\0') {
        return false;
    }
    text.split('/').all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// RFC 8785 (JCS) serialization. Only integer JSON numbers are admissible in
/// this evidence surface; any other number is rejected rather than approximated.
fn jcs_bytes(value: &Value) -> Result<Vec<u8>, SealError> {
    let mut out = Vec::new();
    jcs_append(value, &mut out)?;
    Ok(out)
}

fn jcs_append(value: &Value, out: &mut Vec<u8>) -> Result<(), SealError> {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(number) => {
            if let Some(unsigned) = number.as_u64() {
                out.extend_from_slice(unsigned.to_string().as_bytes());
            } else if let Some(signed) = number.as_i64() {
                out.extend_from_slice(signed.to_string().as_bytes());
            } else {
                return Err(SealError::JsonNumberNotInteger);
            }
        }
        Value::String(text) => jcs_append_string(text, out),
        Value::Array(items) => {
            out.push(b'[');
            for (position, item) in items.iter().enumerate() {
                if position > 0 {
                    out.push(b',');
                }
                jcs_append(item, out)?;
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push(b'{');
            for (position, key) in keys.iter().enumerate() {
                if position > 0 {
                    out.push(b',');
                }
                jcs_append_string(key, out);
                out.push(b':');
                let entry = map.get(*key).ok_or(SealError::NotJsonObject)?;
                jcs_append(entry, out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

fn jcs_append_string(text: &str, out: &mut Vec<u8>) {
    out.push(b'"');
    for ch in text.chars() {
        match ch {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\u{8}' => out.extend_from_slice(b"\\b"),
            '\t' => out.extend_from_slice(b"\\t"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\u{c}' => out.extend_from_slice(b"\\f"),
            '\r' => out.extend_from_slice(b"\\r"),
            control if (control as u32) < 0x20 => {
                out.extend_from_slice(format!("\\u{:04x}", control as u32).as_bytes());
            }
            other => {
                let mut buffer = [0u8; 4];
                out.extend_from_slice(other.encode_utf8(&mut buffer).as_bytes());
            }
        }
    }
    out.push(b'"');
}

fn as_object(value: &Value) -> Result<&Map<String, Value>, SealError> {
    value.as_object().ok_or(SealError::NotJsonObject)
}

fn expect_exact_keys(map: &Map<String, Value>, keys: &[&str]) -> Result<(), SealError> {
    if map.len() != keys.len() {
        return Err(SealError::SchemaFieldSetMismatch);
    }
    for key in keys {
        if !map.contains_key(*key) {
            return Err(SealError::SchemaFieldSetMismatch);
        }
    }
    Ok(())
}

fn field<'a>(map: &'a Map<String, Value>, key: &str) -> Result<&'a Value, SealError> {
    map.get(key).ok_or(SealError::SchemaFieldSetMismatch)
}

fn str_field<'a>(map: &'a Map<String, Value>, key: &str) -> Result<&'a str, SealError> {
    match map.get(key) {
        Some(Value::String(text)) => Ok(text),
        _ => Err(SealError::SchemaValueMismatch),
    }
}

fn true_field(map: &Map<String, Value>, key: &str) -> Result<(), SealError> {
    match map.get(key) {
        Some(Value::Bool(true)) => Ok(()),
        _ => Err(SealError::BooleanNotTrue),
    }
}

fn doc_array<'a>(doc: &'a Value, key: &str) -> Result<&'a Vec<Value>, SealError> {
    match as_object(doc)?.get(key) {
        Some(Value::Array(items)) => Ok(items),
        _ => Err(SealError::SchemaValueMismatch),
    }
}

/// Parses a receipt that must be a JSON object in exact RFC 8785 canonical form:
/// re-serialization must reproduce the input bytes (rejects BOM, duplicate keys,
/// whitespace, trailing bytes, and any non-canonical encoding).
fn parse_canonical_object(bytes: &[u8]) -> Result<Value, SealError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| SealError::JsonParseFailed)?;
    if !value.is_object() {
        return Err(SealError::NotJsonObject);
    }
    if jcs_bytes(&value)? != bytes {
        return Err(SealError::NotCanonicalJson);
    }
    Ok(value)
}

struct Binding {
    path: String,
    byte_len: String,
    sha256: String,
}

fn parse_file_binding(value: &Value) -> Result<Binding, SealError> {
    let map = as_object(value)?;
    expect_exact_keys(map, &["byte_len", "path", "sha256"])?;
    let path = str_field(map, "path")?;
    let byte_len = str_field(map, "byte_len")?;
    let sha256 = str_field(map, "sha256")?;
    if !is_repo_relative_path(path) {
        return Err(SealError::PathFormatInvalid);
    }
    if !is_canonical_decimal(byte_len) {
        return Err(SealError::ByteLenFormatInvalid);
    }
    if !is_lower_hex(sha256, 64) {
        return Err(SealError::HexFormatInvalid);
    }
    Ok(Binding {
        path: path.to_string(),
        byte_len: byte_len.to_string(),
        sha256: sha256.to_string(),
    })
}

fn check_binding(binding: &Binding, expected_path: &str, bytes: &[u8]) -> Result<(), SealError> {
    if binding.path != expected_path
        || binding.byte_len != bytes.len().to_string()
        || binding.sha256 != sha256_hex(bytes)
    {
        return Err(SealError::FileBindingMismatch);
    }
    Ok(())
}

fn file_binding(path: &str, bytes: &[u8]) -> Value {
    let mut map = Map::new();
    map.insert("byte_len".to_string(), Value::String(bytes.len().to_string()));
    map.insert("path".to_string(), Value::String(path.to_string()));
    map.insert("sha256".to_string(), Value::String(sha256_hex(bytes)));
    Value::Object(map)
}

/// Resolves the explicitly supplied capture ref to a full commit inside the
/// retained checkout descriptor. Replacement processing is disabled and the
/// invocation is local and read-only, with no network or ref mutation.
fn git_resolve_commit(root: &WorkspaceRoot, refname: &str) -> Result<String, SealError> {
    let output = root
        .git_command()
        .arg("rev-parse")
        .arg("--verify")
        .arg("--end-of-options")
        .arg(format!("{refname}^{{commit}}"))
        .output()
        .map_err(|_| SealError::GitInvocationFailed)?;
    if !output.status.success() {
        return Err(SealError::GitRefResolutionFailed);
    }
    let stdout = output.stdout;
    if stdout.len() != 41 || stdout.last() != Some(&b'\n') {
        return Err(SealError::GitRefResolutionFailed);
    }
    let commit = stdout.get(..40).ok_or(SealError::GitRefResolutionFailed)?;
    let commit = std::str::from_utf8(commit).map_err(|_| SealError::GitRefResolutionFailed)?;
    if !is_lower_hex(commit, 40) {
        return Err(SealError::GitRefResolutionFailed);
    }
    Ok(commit.to_string())
}

/// Reads the exact commit object with replacement processing disabled.
fn git_read_commit(root: &WorkspaceRoot, commit: &str) -> Result<Vec<u8>, SealError> {
    if !is_lower_hex(commit, 40) {
        return Err(SealError::GitRefResolutionFailed);
    }
    let output = root
        .git_command()
        .arg("cat-file")
        .arg("commit")
        .arg(commit)
        .output()
        .map_err(|_| SealError::GitInvocationFailed)?;
    if !output.status.success() {
        return Err(SealError::GitRefResolutionFailed);
    }
    Ok(output.stdout)
}

/// Requires one and only one valid lowercase object ID in the commit's parent
/// headers. Root commits and merge commits are rejected.
fn parse_single_commit_parent(commit: &[u8]) -> Result<String, SealError> {
    let header_end = commit
        .windows(2)
        .position(|window| window == b"\n\n")
        .ok_or(SealError::GitRefResolutionFailed)?;
    let headers = commit.get(..header_end).ok_or(SealError::GitRefResolutionFailed)?;
    let mut parent = None;
    for header in headers.split(|byte| *byte == b'\n') {
        if header.starts_with(b"parent") {
            let value = header.strip_prefix(b"parent ").ok_or(SealError::GitRefResolutionFailed)?;
            let value =
                std::str::from_utf8(value).map_err(|_| SealError::GitRefResolutionFailed)?;
            if !is_lower_hex(value, 40) || parent.is_some() {
                return Err(SealError::GitRefResolutionFailed);
            }
            parent = Some(value.to_string());
        }
    }
    parent.ok_or(SealError::GitRefResolutionFailed)
}

/// Checks independently resolved history values, including each commit's sole parent.
fn check_history_commits(
    [p0_parent, p0, p0_parent_from_object, p1_parent, p1, p1_parent_from_object]: [&str; 6],
    [sealed_p0_parent, sealed_p0]: [&str; 2],
) -> Result<(), SealError> {
    if p0_parent != sealed_p0_parent
        || p0_parent_from_object != p0_parent
        || p0_parent_from_object != sealed_p0_parent
        || p0 != sealed_p0
        || p1_parent != p0
        || p1_parent_from_object != p1_parent
        || p1 == p1_parent
    {
        return Err(SealError::CaptureCommitMismatch);
    }
    Ok(())
}

/// Proves both approved commits have exactly one direct parent matching the
/// independently sealed parent ref.
fn check_p1_ref_history(
    root: &WorkspaceRoot,
    seal: &RawSeal,
) -> Result<(String, String), SealError> {
    let p0_parent = git_resolve_commit(root, REF_P0_PARENT)?;
    let p0 = git_resolve_commit(root, REF_P0)?;
    let p0_parent_from_object = parse_single_commit_parent(&git_read_commit(root, &p0)?)?;
    let p1_parent = git_resolve_commit(root, REF_P1_PARENT)?;
    let p1 = git_resolve_commit(root, REF_P1)?;
    let p1_parent_from_object = parse_single_commit_parent(&git_read_commit(root, &p1)?)?;
    check_history_commits(
        [&p0_parent, &p0, &p0_parent_from_object, &p1_parent, &p1, &p1_parent_from_object],
        [&seal.p0_parent_commit, &seal.p0_commit],
    )?;
    Ok((p1_parent, p1))
}

/// Validates the exact sidecar grammar `<64-lower-hex><two spaces><path><LF>`
/// against the sidecar's own lock bytes, and returns the parsed digest.
fn check_sidecar(sidecar: &[u8], lock_path: &str, lock_bytes: &[u8]) -> Result<String, SealError> {
    let expected_len = 64 + 2 + lock_path.len() + 1;
    if sidecar.len() != expected_len {
        return Err(SealError::SidecarFormatInvalid);
    }
    let hex = sidecar.get(..64).ok_or(SealError::SidecarFormatInvalid)?;
    let hex = std::str::from_utf8(hex).map_err(|_| SealError::SidecarFormatInvalid)?;
    if !is_lower_hex(hex, 64) {
        return Err(SealError::SidecarFormatInvalid);
    }
    if sidecar.get(64..66) != Some(b"  ".as_slice()) {
        return Err(SealError::SidecarFormatInvalid);
    }
    if sidecar.get(66..expected_len - 1) != Some(lock_path.as_bytes()) {
        return Err(SealError::SidecarFormatInvalid);
    }
    if sidecar.get(expected_len - 1) != Some(&b'\n') {
        return Err(SealError::SidecarFormatInvalid);
    }
    if hex != sha256_hex(lock_bytes) {
        return Err(SealError::SidecarDigestMismatch);
    }
    Ok(hex.to_string())
}

struct RawSeal {
    p0_parent_commit: String,
    p0_commit: String,
    cargo_version_sha256: String,
    inputs: Vec<Binding>,
}

fn raw_seal_ref_entry(
    refs: &Map<String, Value>,
    key: &str,
    expected_name: &str,
) -> Result<String, SealError> {
    let entry = as_object(field(refs, key)?).map_err(|_| SealError::RefsSectionInvalid)?;
    expect_exact_keys(entry, &["commit", "name"]).map_err(|_| SealError::RefsSectionInvalid)?;
    if str_field(entry, "name")? != expected_name {
        return Err(SealError::RefsSectionInvalid);
    }
    let commit = str_field(entry, "commit")?;
    if !is_lower_hex(commit, 40) {
        return Err(SealError::RefsSectionInvalid);
    }
    Ok(commit.to_string())
}

/// Full strict parse of the audited raw-writer seal (bytes only; file bindings
/// are checked against reopened files separately where those files are inputs).
fn parse_raw_seal(bytes: &[u8]) -> Result<RawSeal, SealError> {
    let value = parse_canonical_object(bytes)?;
    let map = as_object(&value)?;
    expect_exact_keys(
        map,
        &["cargo_version_sha256", "equality", "inputs", "refs", "schema", "target", "verdict"],
    )?;
    if str_field(map, "schema")? != RAW_SEAL_SCHEMA {
        return Err(SealError::SchemaValueMismatch);
    }
    if str_field(map, "target")? != TARGET_TRIPLE {
        return Err(SealError::SchemaValueMismatch);
    }
    if str_field(map, "verdict")? != VERDICT_PASS {
        return Err(SealError::VerdictNotPass);
    }
    let cargo_version_sha256 = str_field(map, "cargo_version_sha256")?;
    if !is_lower_hex(cargo_version_sha256, 64) {
        return Err(SealError::HexFormatInvalid);
    }
    let equality = as_object(field(map, "equality")?)?;
    expect_exact_keys(
        equality,
        &["cli_metadata_bytes", "lock_bytes", "node_metadata_bytes", "parsed_lock_digest"],
    )?;
    true_field(equality, "cli_metadata_bytes")?;
    true_field(equality, "lock_bytes")?;
    true_field(equality, "node_metadata_bytes")?;
    true_field(equality, "parsed_lock_digest")?;
    let refs = as_object(field(map, "refs")?)?;
    expect_exact_keys(refs, &["p0", "p0_parent", "p0_parent_is_p0_parent"])
        .map_err(|_| SealError::RefsSectionInvalid)?;
    match refs.get("p0_parent_is_p0_parent") {
        Some(Value::Bool(true)) => {}
        _ => return Err(SealError::RefsSectionInvalid),
    }
    let p0_parent_commit = raw_seal_ref_entry(refs, "p0_parent", REF_P0_PARENT)?;
    let p0_commit = raw_seal_ref_entry(refs, "p0", REF_P0)?;
    if p0_parent_commit == p0_commit {
        return Err(SealError::RefsSectionInvalid);
    }
    let inputs_value = match field(map, "inputs")? {
        Value::Array(items) => items,
        _ => Err(SealError::SchemaValueMismatch)?,
    };
    let mut expected_paths: Vec<&str> = P0_RAW_SEAL_INPUT_PATHS.to_vec();
    expected_paths.sort_unstable();
    if inputs_value.len() != expected_paths.len() {
        return Err(SealError::SchemaValueMismatch);
    }
    let mut inputs = Vec::with_capacity(expected_paths.len());
    for (entry, expected_path) in inputs_value.iter().zip(expected_paths.iter()) {
        let binding = parse_file_binding(entry)?;
        if binding.path != *expected_path {
            return Err(SealError::SchemaValueMismatch);
        }
        inputs.push(binding);
    }
    Ok(RawSeal {
        p0_parent_commit,
        p0_commit,
        cargo_version_sha256: cargo_version_sha256.to_string(),
        inputs,
    })
}

struct P0Files {
    parent_cli: Vec<u8>,
    parent_node: Vec<u8>,
    parent_lock: Vec<u8>,
    parent_lock_sidecar: Vec<u8>,
    cli: Vec<u8>,
    node: Vec<u8>,
    lock: Vec<u8>,
    lock_sidecar: Vec<u8>,
}

/// Rechecks the raw seal's eight file bindings against reopened bytes and
/// independently recomputes every raw-seal equality claim.
fn check_raw_seal_files(seal: &RawSeal, files: &P0Files) -> Result<(), SealError> {
    let pairs: [(&str, &[u8]); 8] = [
        (P0_PARENT_CLI_RAW, &files.parent_cli),
        (P0_PARENT_NODE_RAW, &files.parent_node),
        (P0_PARENT_LOCK, &files.parent_lock),
        (P0_PARENT_LOCK_SIDECAR, &files.parent_lock_sidecar),
        (P0_CLI_RAW, &files.cli),
        (P0_NODE_RAW, &files.node),
        (P0_LOCK, &files.lock),
        (P0_LOCK_SIDECAR, &files.lock_sidecar),
    ];
    for binding in &seal.inputs {
        let &(path, bytes) = pairs
            .iter()
            .find(|(path, _)| *path == binding.path)
            .ok_or(SealError::FileBindingMismatch)?;
        check_binding(binding, path, bytes)?;
    }
    if files.parent_cli != files.cli {
        return Err(SealError::RawEqualityFailed);
    }
    if files.parent_node != files.node {
        return Err(SealError::RawEqualityFailed);
    }
    if files.parent_lock != files.lock {
        return Err(SealError::RawEqualityFailed);
    }
    let parent_digest =
        check_sidecar(&files.parent_lock_sidecar, P0_PARENT_LOCK, &files.parent_lock)?;
    let child_digest = check_sidecar(&files.lock_sidecar, P0_LOCK, &files.lock)?;
    if parent_digest != child_digest {
        return Err(SealError::RawEqualityFailed);
    }
    Ok(())
}

struct MetaPackage {
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
    manifest_path: String,
}

struct MetaNode {
    /// Package ids of direct dependencies reachable through resolved normal or
    /// build edges (dev edges are excluded at parse time).
    deps: Vec<String>,
    features: Vec<String>,
}

struct RawMetadata {
    workspace_root: String,
    packages: BTreeMap<String, MetaPackage>,
    members: BTreeSet<String>,
    nodes: BTreeMap<String, MetaNode>,
}

fn optional_string(map: &Map<String, Value>, key: &str) -> Result<Option<String>, SealError> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        _ => Err(SealError::MetadataShapeInvalid),
    }
}

fn parse_raw_metadata(bytes: &[u8]) -> Result<RawMetadata, SealError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| SealError::JsonParseFailed)?;
    let map = as_object(&value)?;
    let workspace_root =
        str_field(map, "workspace_root").map_err(|_| SealError::MetadataShapeInvalid)?.to_string();
    if !workspace_root.starts_with('/')
        || workspace_root.len() < 2
        || workspace_root.ends_with('/')
        || workspace_root.contains('\0')
        || workspace_root.contains('\\')
    {
        return Err(SealError::WorkspaceRootInvalid);
    }
    let packages_value = match map.get("packages") {
        Some(Value::Array(items)) => items,
        _ => return Err(SealError::MetadataShapeInvalid),
    };
    let mut packages = BTreeMap::new();
    for entry in packages_value {
        let package = as_object(entry).map_err(|_| SealError::MetadataShapeInvalid)?;
        let id = str_field(package, "id").map_err(|_| SealError::MetadataShapeInvalid)?.to_string();
        let name =
            str_field(package, "name").map_err(|_| SealError::MetadataShapeInvalid)?.to_string();
        let version =
            str_field(package, "version").map_err(|_| SealError::MetadataShapeInvalid)?.to_string();
        let manifest_path = str_field(package, "manifest_path")
            .map_err(|_| SealError::MetadataShapeInvalid)?
            .to_string();
        let source = optional_string(package, "source")?;
        let checksum = optional_string(package, "checksum")?;
        if let Some(checksum) = &checksum
            && !is_lower_hex(checksum, 64)
        {
            return Err(SealError::MetadataShapeInvalid);
        }
        let info = MetaPackage { name, version, source, checksum, manifest_path };
        if packages.insert(id, info).is_some() {
            return Err(SealError::DuplicateRawPackageId);
        }
    }
    let members_value = match map.get("workspace_members") {
        Some(Value::Array(items)) => items,
        _ => return Err(SealError::MetadataShapeInvalid),
    };
    let mut members = BTreeSet::new();
    for entry in members_value {
        let id = match entry {
            Value::String(text) => text.clone(),
            _ => return Err(SealError::MetadataShapeInvalid),
        };
        if !members.insert(id) {
            return Err(SealError::DuplicateWorkspaceMember);
        }
    }
    let resolve = as_object(field(map, "resolve").map_err(|_| SealError::MetadataShapeInvalid)?)
        .map_err(|_| SealError::MetadataShapeInvalid)?;
    let nodes_value = match resolve.get("nodes") {
        Some(Value::Array(items)) => items,
        _ => return Err(SealError::MetadataShapeInvalid),
    };
    let mut nodes = BTreeMap::new();
    for entry in nodes_value {
        let node = as_object(entry).map_err(|_| SealError::MetadataShapeInvalid)?;
        let id = str_field(node, "id").map_err(|_| SealError::MetadataShapeInvalid)?.to_string();
        let deps_value = match node.get("deps") {
            Some(Value::Array(items)) => items,
            _ => return Err(SealError::MetadataShapeInvalid),
        };
        let mut deps = Vec::new();
        for dep_entry in deps_value {
            let dep = as_object(dep_entry).map_err(|_| SealError::MetadataShapeInvalid)?;
            let pkg = str_field(dep, "pkg").map_err(|_| SealError::MetadataShapeInvalid)?;
            let kinds = match dep.get("dep_kinds") {
                Some(Value::Array(items)) if !items.is_empty() => items,
                _ => return Err(SealError::MetadataShapeInvalid),
            };
            let mut include = false;
            for kind_entry in kinds {
                let kind_map =
                    as_object(kind_entry).map_err(|_| SealError::MetadataShapeInvalid)?;
                match kind_map.get("kind") {
                    Some(Value::Null) => include = true,
                    Some(Value::String(kind)) if kind == "build" => include = true,
                    Some(Value::String(kind)) if kind == "dev" => {}
                    _ => return Err(SealError::DepKindInvalid),
                }
            }
            if include {
                deps.push(pkg.to_string());
            }
        }
        let features_value = match node.get("features") {
            Some(Value::Array(items)) => items,
            _ => return Err(SealError::MetadataShapeInvalid),
        };
        let mut features = Vec::new();
        for feature_entry in features_value {
            match feature_entry {
                Value::String(feature) => features.push(feature.clone()),
                _ => return Err(SealError::MetadataShapeInvalid),
            }
        }
        if nodes.insert(id, MetaNode { deps, features }).is_some() {
            return Err(SealError::DuplicateResolveNodeId);
        }
    }
    Ok(RawMetadata { workspace_root, packages, members, nodes })
}

fn strip_workspace_prefix(workspace_root: &str, manifest_path: &str) -> Result<String, SealError> {
    let root = std::path::Path::new(workspace_root);
    let manifest = std::path::Path::new(manifest_path);
    let canonical_root =
        std::fs::canonicalize(root).map_err(|_| SealError::ManifestPathOutsideRoot)?;
    let canonical_manifest =
        std::fs::canonicalize(manifest).map_err(|_| SealError::ManifestPathOutsideRoot)?;
    if canonical_root != root
        || canonical_manifest != manifest
        || !canonical_manifest.is_file()
        || !canonical_manifest.starts_with(&canonical_root)
    {
        return Err(SealError::ManifestPathOutsideRoot);
    }
    let relative = canonical_manifest
        .strip_prefix(&canonical_root)
        .map_err(|_| SealError::ManifestPathOutsideRoot)?
        .to_str()
        .ok_or(SealError::ManifestPathSegmentInvalid)?;
    if !is_repo_relative_path(relative) {
        return Err(SealError::ManifestPathSegmentInvalid);
    }
    Ok(relative.to_string())
}

/// Builds the collision-safe canonical identity object for one resolved
/// package: workspace members become rooted repository-relative identities and
/// external packages keep name/version/source/checksum; no raw Cargo id,
/// absolute path, or target directory survives.
fn identity_for(meta: &RawMetadata, id: &str) -> Result<Value, SealError> {
    let package = meta.packages.get(id).ok_or(SealError::UnresolvedEdge)?;
    let mut map = Map::new();
    map.insert("version".to_string(), Value::String(package.version.clone()));
    if meta.members.contains(id) {
        let relative = strip_workspace_prefix(&meta.workspace_root, &package.manifest_path)?;
        map.insert("kind".to_string(), Value::String("workspace".to_string()));
        map.insert("manifest_path".to_string(), Value::String(relative));
        map.insert("name".to_string(), Value::String(package.name.clone()));
        map.insert("source".to_string(), Value::String("path+workspace".to_string()));
    } else {
        let source = package.source.clone().ok_or(SealError::ExternalSourceMissing)?;
        if source.is_empty() {
            return Err(SealError::ExternalSourceMissing);
        }
        map.insert(
            "checksum".to_string(),
            package.checksum.clone().map_or(Value::Null, Value::String),
        );
        map.insert("kind".to_string(), Value::String("external".to_string()));
        map.insert("name".to_string(), Value::String(package.name.clone()));
        map.insert("source".to_string(), Value::String(source));
    }
    Ok(Value::Object(map))
}

struct RootedDoc {
    bytes: Vec<u8>,
    doc: Value,
    capture_commit: String,
    cargo_version_sha256: String,
}

/// Deterministically normalizes one raw `cargo metadata` capture into the
/// `base-mev/b5-cargo-rooted-metadata/v2` document for the given approved row.
fn build_rooted_doc(
    raw_bytes: &[u8],
    row: &NormalizeRow,
    capture_commit: &str,
    cargo_version_sha256: &str,
    expected_checkout_root: Option<&str>,
) -> Result<RootedDoc, SealError> {
    let meta = parse_raw_metadata(raw_bytes)?;
    if let Some(checkout_root) = expected_checkout_root
        && checkout_root != meta.workspace_root
    {
        return Err(SealError::CheckoutRootMismatch);
    }
    let expected_manifest = format!("{}/{}", meta.workspace_root, row.root_manifest);
    let mut root_id: Option<&String> = None;
    for (id, package) in &meta.packages {
        if package.name == row.root_package && package.manifest_path == expected_manifest {
            if root_id.is_some() {
                return Err(SealError::RootPackageAmbiguous);
            }
            root_id = Some(id);
        }
    }
    let root_id = root_id.ok_or(SealError::RootPackageNotFound)?.clone();
    if !meta.members.contains(&root_id) {
        return Err(SealError::RootPackageNotFound);
    }
    let root_node = meta.nodes.get(&root_id).ok_or(SealError::UnresolvedRoot)?;
    let root_has_presign = root_node.features.iter().any(|f| f == SELECTION_PRESIGN);
    if row.feature_selection == SELECTION_DEFAULT {
        if root_has_presign {
            return Err(SealError::FeatureSelectionMismatch);
        }
    } else if row.feature_selection == SELECTION_PRESIGN {
        if !root_has_presign {
            return Err(SealError::FeatureSelectionMismatch);
        }
    } else {
        return Err(SealError::FeatureSelectionMismatch);
    }
    let mut closure: BTreeSet<String> = BTreeSet::new();
    let mut stack = vec![root_id.clone()];
    while let Some(id) = stack.pop() {
        if !closure.insert(id.clone()) {
            continue;
        }
        let node = meta.nodes.get(&id).ok_or(SealError::UnresolvedEdge)?;
        for dep in &node.deps {
            if !closure.contains(dep) {
                stack.push(dep.clone());
            }
        }
    }
    let mut identity_by_id: BTreeMap<String, (Vec<u8>, Value)> = BTreeMap::new();
    let mut seen_identities: BTreeSet<Vec<u8>> = BTreeSet::new();
    for id in &closure {
        let identity = identity_for(&meta, id)?;
        let identity_bytes = jcs_bytes(&identity)?;
        if !seen_identities.insert(identity_bytes.clone()) {
            return Err(SealError::IdentityCollision);
        }
        identity_by_id.insert(id.clone(), (identity_bytes, identity));
    }
    let mut package_entries: Vec<(Vec<u8>, Value)> = identity_by_id
        .values()
        .map(|(bytes, identity)| (bytes.clone(), identity.clone()))
        .collect();
    package_entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut feature_entries: Vec<(Vec<u8>, Value)> = Vec::new();
    for id in &closure {
        let node = meta.nodes.get(id).ok_or(SealError::UnresolvedEdge)?;
        let (_, identity) = identity_by_id.get(id).ok_or(SealError::UnresolvedEdge)?;
        for feature in &node.features {
            let mut entry = Map::new();
            entry.insert("feature".to_string(), Value::String(feature.clone()));
            entry.insert("package".to_string(), identity.clone());
            let entry = Value::Object(entry);
            let entry_bytes = jcs_bytes(&entry)?;
            feature_entries.push((entry_bytes, entry));
        }
    }
    feature_entries.sort_by(|a, b| a.0.cmp(&b.0));
    for window in feature_entries.windows(2) {
        if let [left, right] = window
            && left.0 == right.0
        {
            return Err(SealError::DuplicateFeatureEntry);
        }
    }
    let (_, root_identity) = identity_by_id.get(&root_id).ok_or(SealError::UnresolvedRoot)?;
    let mut doc = Map::new();
    doc.insert("capture_commit".to_string(), Value::String(capture_commit.to_string()));
    doc.insert("capture_ref".to_string(), Value::String(row.capture_ref.to_string()));
    doc.insert("cargo_version_sha256".to_string(), Value::String(cargo_version_sha256.to_string()));
    doc.insert(
        "edge_kinds".to_string(),
        Value::Array(vec![Value::String("build".to_string()), Value::String("normal".to_string())]),
    );
    doc.insert("feature_selection".to_string(), Value::String(row.feature_selection.to_string()));
    doc.insert(
        "features".to_string(),
        Value::Array(feature_entries.into_iter().map(|(_, entry)| entry).collect()),
    );
    doc.insert(
        "packages".to_string(),
        Value::Array(package_entries.into_iter().map(|(_, entry)| entry).collect()),
    );
    doc.insert("root".to_string(), root_identity.clone());
    doc.insert("schema".to_string(), Value::String(ROOTED_SCHEMA.to_string()));
    doc.insert("target".to_string(), Value::String(TARGET_TRIPLE.to_string()));
    let doc = Value::Object(doc);
    let bytes = jcs_bytes(&doc)?;
    Ok(RootedDoc {
        bytes,
        doc,
        capture_commit: capture_commit.to_string(),
        cargo_version_sha256: cargo_version_sha256.to_string(),
    })
}

fn check_capture_commit(class: RefClass, commit: &str, seal: &RawSeal) -> Result<(), SealError> {
    let ok = match class {
        RefClass::P0Parent => commit == seal.p0_parent_commit,
        RefClass::P0 | RefClass::P1Parent => commit == seal.p0_commit,
        RefClass::P1 => commit != seal.p0_commit && commit != seal.p0_parent_commit,
    };
    if ok { Ok(()) } else { Err(SealError::CaptureCommitMismatch) }
}

/// Strict-parses one stored normalized document, then reruns normalization in
/// memory from its raw capture and requires byte-identical reproduction.
fn reproduce_normalized(
    row: &NormalizeRow,
    raw_bytes: &[u8],
    stored_bytes: &[u8],
    raw_seal: Option<&RawSeal>,
) -> Result<RootedDoc, SealError> {
    let stored = parse_canonical_object(stored_bytes)?;
    let map = as_object(&stored)?;
    expect_exact_keys(
        map,
        &[
            "capture_commit",
            "capture_ref",
            "cargo_version_sha256",
            "edge_kinds",
            "feature_selection",
            "features",
            "packages",
            "root",
            "schema",
            "target",
        ],
    )?;
    if str_field(map, "schema")? != ROOTED_SCHEMA {
        return Err(SealError::SchemaValueMismatch);
    }
    if str_field(map, "capture_ref")? != row.capture_ref {
        return Err(SealError::SchemaValueMismatch);
    }
    if str_field(map, "target")? != TARGET_TRIPLE {
        return Err(SealError::SchemaValueMismatch);
    }
    if str_field(map, "feature_selection")? != row.feature_selection {
        return Err(SealError::SchemaValueMismatch);
    }
    let capture_commit = str_field(map, "capture_commit")?;
    if !is_lower_hex(capture_commit, 40) {
        return Err(SealError::HexFormatInvalid);
    }
    let cargo_version_sha256 = str_field(map, "cargo_version_sha256")?;
    if !is_lower_hex(cargo_version_sha256, 64) {
        return Err(SealError::HexFormatInvalid);
    }
    if let Some(seal) = raw_seal {
        check_capture_commit(row.class, capture_commit, seal)?;
        if cargo_version_sha256 != seal.cargo_version_sha256 {
            return Err(SealError::CargoVersionMismatch);
        }
    }
    let rebuilt = build_rooted_doc(raw_bytes, row, capture_commit, cargo_version_sha256, None)?;
    if rebuilt.bytes != stored_bytes {
        return Err(SealError::NormalizedReproductionMismatch);
    }
    Ok(rebuilt)
}

struct LockDepEntry {
    name: String,
    version: Option<String>,
    source: Option<String>,
}

struct LockStanza {
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
    deps: Vec<LockDepEntry>,
}

fn parse_lock_kv(line: &str, key: &str) -> Option<String> {
    let rest = line.strip_prefix(key)?;
    let rest = rest.strip_prefix(" = \"")?;
    let value = rest.strip_suffix('"')?;
    if value.contains('"') {
        return None;
    }
    Some(value.to_string())
}

fn parse_lock_dep_entry(line: &str) -> Result<LockDepEntry, SealError> {
    let inner = line
        .strip_prefix(" \"")
        .and_then(|rest| rest.strip_suffix("\","))
        .ok_or(SealError::LockDependencyEntryInvalid)?;
    if inner.is_empty() || inner.contains('"') {
        return Err(SealError::LockDependencyEntryInvalid);
    }
    let mut parts = inner.split(' ');
    let name = parts.next().ok_or(SealError::LockDependencyEntryInvalid)?;
    if name.is_empty() {
        return Err(SealError::LockDependencyEntryInvalid);
    }
    let version = match parts.next() {
        None => None,
        Some(version) if !version.is_empty() && !version.starts_with('(') => {
            Some(version.to_string())
        }
        _ => return Err(SealError::LockDependencyEntryInvalid),
    };
    let source = match parts.next() {
        None => None,
        Some(source)
            if source.starts_with('(') && source.ends_with(')') && parts.next().is_none() =>
        {
            Some(
                source
                    .strip_prefix('(')
                    .and_then(|value| value.strip_suffix(')'))
                    .ok_or(SealError::LockDependencyEntryInvalid)?
                    .to_string(),
            )
        }
        _ => return Err(SealError::LockDependencyEntryInvalid),
    };
    if source.is_some() && version.is_none() {
        return Err(SealError::LockDependencyEntryInvalid);
    }
    Ok(LockDepEntry { name: name.to_string(), version, source })
}

fn parse_lock(bytes: &[u8]) -> Result<Vec<LockStanza>, SealError> {
    let text = std::str::from_utf8(bytes).map_err(|_| SealError::LockNotUtf8)?;
    if !text.ends_with('\n') {
        return Err(SealError::LockFormatInvalid);
    }
    let mut stanzas: Vec<LockStanza> = Vec::new();
    let mut in_deps = false;
    let mut lines: Vec<&str> = text.split('\n').collect();
    lines.pop();
    for line in lines {
        if in_deps {
            if line == "]" {
                in_deps = false;
                continue;
            }
            let entry = parse_lock_dep_entry(line)?;
            stanzas.last_mut().ok_or(SealError::LockFormatInvalid)?.deps.push(entry);
            continue;
        }
        if line == "[[package]]" {
            stanzas.push(LockStanza {
                name: String::new(),
                version: String::new(),
                source: None,
                checksum: None,
                deps: Vec::new(),
            });
            continue;
        }
        if line == "dependencies = [" {
            if stanzas.is_empty() {
                return Err(SealError::LockFormatInvalid);
            }
            in_deps = true;
            continue;
        }
        if let Some(stanza) = stanzas.last_mut() {
            if let Some(value) = parse_lock_kv(line, "name") {
                stanza.name = value;
            } else if let Some(value) = parse_lock_kv(line, "version") {
                stanza.version = value;
            } else if let Some(value) = parse_lock_kv(line, "source") {
                stanza.source = Some(value);
            } else if let Some(value) = parse_lock_kv(line, "checksum") {
                stanza.checksum = Some(value);
            }
        }
    }
    if in_deps {
        return Err(SealError::LockFormatInvalid);
    }
    for stanza in &stanzas {
        if stanza.name.is_empty() || stanza.version.is_empty() {
            return Err(SealError::LockFormatInvalid);
        }
    }
    Ok(stanzas)
}

fn lock_source_matches(candidate: Option<&str>, qualifier: &str) -> bool {
    candidate.is_some_and(|candidate| {
        candidate == qualifier
            || candidate
                .strip_prefix(qualifier)
                .is_some_and(|suffix| suffix.starts_with('#') && suffix.len() > 1)
    })
}

/// Requires each dependency entry to identify exactly one complete lock stanza.
/// A bare name is valid only when that name has exactly one stanza. Qualifiers
/// are matched against every supplied identity component, without deduplicating
/// same-name/version stanzas that differ by source.
fn check_lock_dep_qualification(stanzas: &[LockStanza]) -> Result<(), SealError> {
    for owner in stanzas {
        for dep in &owner.deps {
            let matching_name_count =
                stanzas.iter().filter(|candidate| candidate.name == dep.name).count();
            if matching_name_count == 0 {
                return Err(SealError::LockDependencyEntryInvalid);
            }
            if dep.version.is_none() {
                if matching_name_count != 1 {
                    return Err(SealError::LockDuplicateNameUnqualified);
                }
                continue;
            }
            let matching_identity_count = stanzas
                .iter()
                .filter(|candidate| {
                    candidate.name == dep.name
                        && dep.version.as_deref() == Some(candidate.version.as_str())
                        && dep.source.as_deref().is_none_or(|source| {
                            lock_source_matches(candidate.source.as_deref(), source)
                        })
                })
                .count();
            if matching_identity_count != 1 {
                return Err(SealError::LockDuplicateNameUnqualified);
            }
        }
    }
    Ok(())
}

type LockUniverseEntry = (String, String, Option<String>, Option<String>);

fn lock_universe(stanzas: &[LockStanza]) -> Vec<LockUniverseEntry> {
    let mut universe: Vec<LockUniverseEntry> = stanzas
        .iter()
        .map(|stanza| {
            (
                stanza.name.clone(),
                stanza.version.clone(),
                stanza.source.clone(),
                stanza.checksum.clone(),
            )
        })
        .collect();
    universe.sort();
    universe
}

/// Returns the `(name, version)` of the `[[package]]` stanza that owns the
/// given line position of the child lock.
fn lock_stanza_at(lines: &[&str], position: usize) -> Result<(String, String), SealError> {
    let mut name: Option<String> = None;
    let mut version: Option<String> = None;
    let mut in_stanza = false;
    for line in lines.iter().take(position) {
        if *line == "[[package]]" {
            in_stanza = true;
            name = None;
            version = None;
            continue;
        }
        if in_stanza {
            if let Some(value) = parse_lock_kv(line, "name") {
                name = Some(value);
            } else if let Some(value) = parse_lock_kv(line, "version") {
                version = Some(value);
            }
        }
    }
    match (name, version) {
        (Some(name), Some(version)) => Ok((name, version)),
        _ => Err(SealError::LockDeltaMismatch),
    }
}

fn line_at<'a>(lines: &[&'a str], position: Option<usize>) -> Result<&'a str, SealError> {
    position.and_then(|index| lines.get(index).copied()).ok_or(SealError::LockDeltaMismatch)
}

/// Proves the child lock equals the parent lock plus exactly the approved six
/// inserted dependency lines across the two existing stanzas, with the exact
/// unchanged neighbor anchors; any other byte delta fails.
fn check_lock_delta(parent: &[u8], child: &[u8]) -> Result<(), SealError> {
    let parent_text = std::str::from_utf8(parent).map_err(|_| SealError::LockNotUtf8)?;
    let child_text = std::str::from_utf8(child).map_err(|_| SealError::LockNotUtf8)?;
    if !parent_text.ends_with('\n') || !child_text.ends_with('\n') {
        return Err(SealError::LockFormatInvalid);
    }
    let mut old_lines: Vec<&str> = parent_text.split('\n').collect();
    old_lines.pop();
    let mut new_lines: Vec<&str> = child_text.split('\n').collect();
    new_lines.pop();
    if new_lines.len() != old_lines.len() + 6 {
        return Err(SealError::LockDeltaMismatch);
    }
    let mut insertions: Vec<usize> = Vec::new();
    let mut old_index = 0;
    let mut new_index = 0;
    while new_index < new_lines.len() {
        let old_line = old_lines.get(old_index);
        let new_line = new_lines.get(new_index).ok_or(SealError::LockDeltaMismatch)?;
        if old_line == Some(new_line) {
            old_index += 1;
            new_index += 1;
            continue;
        }
        insertions.push(new_index);
        if insertions.len() > 6 {
            return Err(SealError::LockDeltaMismatch);
        }
        new_index += 1;
    }
    if old_index != old_lines.len() || insertions.len() != 6 {
        return Err(SealError::LockDeltaMismatch);
    }
    let expected_lines: [&str; 6] = [
        EXPECTED_CLI_LOCK_INSERTIONS[0],
        EXPECTED_CLI_LOCK_INSERTIONS[1],
        EXPECTED_CLI_LOCK_INSERTIONS[2],
        EXPECTED_CLI_LOCK_INSERTIONS[3],
        EXPECTED_CLI_LOCK_INSERTIONS[4],
        EXPECTED_SUBMIT_LOCK_INSERTION,
    ];
    for (position, expected) in insertions.iter().zip(expected_lines.iter()) {
        let actual = new_lines.get(*position).ok_or(SealError::LockDeltaMismatch)?;
        if actual != expected {
            return Err(SealError::LockDeltaMismatch);
        }
    }
    let &[libc_at, submit_dep_at, serde_at, serde_json_at, sha2_cli_at, sha2_submit_at] =
        insertions.as_slice()
    else {
        return Err(SealError::LockDeltaMismatch);
    };
    if submit_dep_at != libc_at + 1
        || serde_json_at != serde_at + 1
        || sha2_cli_at != serde_json_at + 1
    {
        return Err(SealError::LockDeltaMismatch);
    }
    if line_at(&new_lines, libc_at.checked_sub(1))? != " \"humantime\"," {
        return Err(SealError::LockDeltaMismatch);
    }
    if line_at(&new_lines, submit_dep_at.checked_add(1))? != " \"proptest\"," {
        return Err(SealError::LockDeltaMismatch);
    }
    if line_at(&new_lines, serde_at.checked_sub(1))? != " \"secp256k1 0.30.0\"," {
        return Err(SealError::LockDeltaMismatch);
    }
    if line_at(&new_lines, sha2_cli_at.checked_add(1))? != " \"tempfile\"," {
        return Err(SealError::LockDeltaMismatch);
    }
    if line_at(&new_lines, sha2_submit_at.checked_sub(1))? != " \"serde_json\"," {
        return Err(SealError::LockDeltaMismatch);
    }
    if line_at(&new_lines, sha2_submit_at.checked_add(1))? != " \"syn 2.0.117\"," {
        return Err(SealError::LockDeltaMismatch);
    }
    let cli_stanza = (PACKAGE_CLI.to_string(), CLI_LOCK_STANZA_VERSION.to_string());
    let submit_stanza = (SUBMIT_NAME.to_string(), SUBMIT_VERSION.to_string());
    if lock_stanza_at(&new_lines, libc_at)? != cli_stanza
        || lock_stanza_at(&new_lines, serde_at)? != cli_stanza
        || lock_stanza_at(&new_lines, sha2_submit_at)? != submit_stanza
    {
        return Err(SealError::LockDeltaMismatch);
    }
    Ok(())
}

/// Requires the named closure arrays (`packages` and `features`) of two rooted
/// documents to be structurally identical.
fn check_closures_equal(left: &RootedDoc, right: &RootedDoc) -> Result<(), SealError> {
    if doc_array(&left.doc, "packages")? != doc_array(&right.doc, "packages")? {
        return Err(SealError::ClosureMismatch);
    }
    if doc_array(&left.doc, "features")? != doc_array(&right.doc, "features")? {
        return Err(SealError::ClosureMismatch);
    }
    Ok(())
}

fn packages_contain_name(packages: &[Value], name: &str) -> Result<bool, SealError> {
    for identity in packages {
        if str_field(as_object(identity)?, "name")? == name {
            return Ok(true);
        }
    }
    Ok(false)
}

fn true_comparisons(keys: &[&str]) -> Value {
    let mut map = Map::new();
    for key in keys {
        map.insert((*key).to_string(), Value::Bool(true));
    }
    Value::Object(map)
}

struct P0NormalizedFiles {
    parent_cli: Vec<u8>,
    parent_node: Vec<u8>,
    cli: Vec<u8>,
    node: Vec<u8>,
}

/// Recomputes the full registration canonical seal from its inputs. Every
/// comparison the seal records must independently hold; otherwise this errors
/// and no document is produced.
fn build_registration_doc(
    raw_seal_bytes: &[u8],
    files: &P0Files,
    normalized: &P0NormalizedFiles,
) -> Result<Value, SealError> {
    let seal = parse_raw_seal(raw_seal_bytes)?;
    check_raw_seal_files(&seal, files)?;
    let parent_cli = reproduce_normalized(
        &ROW_P0_PARENT_CLI,
        &files.parent_cli,
        &normalized.parent_cli,
        Some(&seal),
    )?;
    let parent_node = reproduce_normalized(
        &ROW_P0_PARENT_NODE,
        &files.parent_node,
        &normalized.parent_node,
        Some(&seal),
    )?;
    let cli = reproduce_normalized(&ROW_P0_CLI, &files.cli, &normalized.cli, Some(&seal))?;
    let node = reproduce_normalized(&ROW_P0_NODE, &files.node, &normalized.node, Some(&seal))?;
    check_closures_equal(&parent_cli, &cli)?;
    check_closures_equal(&parent_node, &node)?;
    let mut normalized_bindings = Map::new();
    normalized_bindings
        .insert("p0_cli".to_string(), file_binding(P0_CLI_NORMALIZED, &normalized.cli));
    normalized_bindings
        .insert("p0_node".to_string(), file_binding(P0_NODE_NORMALIZED, &normalized.node));
    normalized_bindings.insert(
        "p0_parent_cli".to_string(),
        file_binding(P0_PARENT_CLI_NORMALIZED, &normalized.parent_cli),
    );
    normalized_bindings.insert(
        "p0_parent_node".to_string(),
        file_binding(P0_PARENT_NODE_NORMALIZED, &normalized.parent_node),
    );
    let mut lock_bindings = Map::new();
    lock_bindings.insert("p0".to_string(), file_binding(P0_LOCK, &files.lock));
    lock_bindings.insert("p0_parent".to_string(), file_binding(P0_PARENT_LOCK, &files.parent_lock));
    let mut doc = Map::new();
    doc.insert("schema".to_string(), Value::String(REGISTRATION_SEAL_SCHEMA.to_string()));
    doc.insert("raw_seal".to_string(), file_binding(RAW_SEAL_PATH, raw_seal_bytes));
    doc.insert("normalized".to_string(), Value::Object(normalized_bindings));
    doc.insert("locks".to_string(), Value::Object(lock_bindings));
    doc.insert(
        "comparisons".to_string(),
        true_comparisons(&[
            "cli_default_features",
            "cli_default_packages",
            "lock_bytes",
            "node_default_features",
            "node_default_packages",
            "parsed_lock_digest",
        ]),
    );
    doc.insert("verdict".to_string(), Value::String(VERDICT_PASS.to_string()));
    Ok(Value::Object(doc))
}

fn binding_field_with_path(
    map: &Map<String, Value>,
    key: &str,
    expected_path: &str,
) -> Result<(), SealError> {
    let binding = parse_file_binding(field(map, key)?)?;
    if binding.path != expected_path {
        return Err(SealError::FileBindingMismatch);
    }
    Ok(())
}

/// Structural validation of a stored registration seal (bytes only; used by
/// `seal-implementation`, which has no authority to reopen the P0 inputs).
fn validate_registration_seal(bytes: &[u8]) -> Result<(), SealError> {
    let value = parse_canonical_object(bytes)?;
    let map = as_object(&value)?;
    expect_exact_keys(
        map,
        &["comparisons", "locks", "normalized", "raw_seal", "schema", "verdict"],
    )?;
    if str_field(map, "schema")? != REGISTRATION_SEAL_SCHEMA {
        return Err(SealError::SchemaValueMismatch);
    }
    if str_field(map, "verdict")? != VERDICT_PASS {
        return Err(SealError::VerdictNotPass);
    }
    let raw_seal_binding = parse_file_binding(field(map, "raw_seal")?)?;
    if raw_seal_binding.path != RAW_SEAL_PATH {
        return Err(SealError::FileBindingMismatch);
    }
    let normalized = as_object(field(map, "normalized")?)?;
    expect_exact_keys(normalized, &["p0_cli", "p0_node", "p0_parent_cli", "p0_parent_node"])?;
    binding_field_with_path(normalized, "p0_cli", P0_CLI_NORMALIZED)?;
    binding_field_with_path(normalized, "p0_node", P0_NODE_NORMALIZED)?;
    binding_field_with_path(normalized, "p0_parent_cli", P0_PARENT_CLI_NORMALIZED)?;
    binding_field_with_path(normalized, "p0_parent_node", P0_PARENT_NODE_NORMALIZED)?;
    let locks = as_object(field(map, "locks")?)?;
    expect_exact_keys(locks, &["p0", "p0_parent"])?;
    binding_field_with_path(locks, "p0", P0_LOCK)?;
    binding_field_with_path(locks, "p0_parent", P0_PARENT_LOCK)?;
    let comparisons = as_object(field(map, "comparisons")?)?;
    expect_exact_keys(
        comparisons,
        &[
            "cli_default_features",
            "cli_default_packages",
            "lock_bytes",
            "node_default_features",
            "node_default_packages",
            "parsed_lock_digest",
        ],
    )?;
    for key in [
        "cli_default_features",
        "cli_default_packages",
        "lock_bytes",
        "node_default_features",
        "node_default_packages",
        "parsed_lock_digest",
    ] {
        true_field(comparisons, key)?;
    }
    Ok(())
}

fn check_submit_identity(identity: &Value) -> Result<(), SealError> {
    let map = as_object(identity)?;
    expect_exact_keys(map, &["kind", "manifest_path", "name", "source", "version"])?;
    if str_field(map, "kind")? != "workspace"
        || str_field(map, "manifest_path")? != SUBMIT_MANIFEST
        || str_field(map, "name")? != SUBMIT_NAME
        || str_field(map, "source")? != "path+workspace"
        || str_field(map, "version")? != SUBMIT_VERSION
    {
        return Err(SealError::SelectedDeltaMismatch);
    }
    Ok(())
}

/// Selected-minus-default identity delta; also requires the default closure to
/// be a subset of the selected closure so no package silently disappears.
fn selected_minus_default(
    selected: &[Value],
    default_packages: &[Value],
) -> Result<Vec<Value>, SealError> {
    let mut default_set: BTreeSet<Vec<u8>> = BTreeSet::new();
    for identity in default_packages {
        default_set.insert(jcs_bytes(identity)?);
    }
    let mut selected_set: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut delta = Vec::new();
    for identity in selected {
        let identity_bytes = jcs_bytes(identity)?;
        if !default_set.contains(&identity_bytes) {
            delta.push(identity.clone());
        }
        selected_set.insert(identity_bytes);
    }
    for identity_bytes in &default_set {
        if !selected_set.contains(identity_bytes) {
            return Err(SealError::SelectedDeltaMismatch);
        }
    }
    Ok(delta)
}

/// The submit package's enabled feature set in the selected closure must be
/// exactly `{presign}`.
fn check_submit_selected_features(selected: &RootedDoc) -> Result<(), SealError> {
    let mut submit_features = Vec::new();
    for entry in doc_array(&selected.doc, "features")? {
        let entry_map = as_object(entry)?;
        let package = as_object(field(entry_map, "package")?)?;
        if str_field(package, "name")? == SUBMIT_NAME {
            submit_features.push(str_field(entry_map, "feature")?.to_string());
        }
    }
    if submit_features != [SUBMIT_PRESIGN_FEATURE] {
        return Err(SealError::SubmitFeatureSetMismatch);
    }
    Ok(())
}

fn features_for_identity(
    rooted: &RootedDoc,
    identity: &Value,
) -> Result<BTreeSet<String>, SealError> {
    let mut features = BTreeSet::new();
    for entry in doc_array(&rooted.doc, "features")? {
        let entry_map = as_object(entry)?;
        if field(entry_map, "package")? == identity {
            features.insert(str_field(entry_map, "feature")?.to_string());
        }
    }
    Ok(features)
}

/// Proves the selected root adds exactly the approved B5 feature and that
/// already-present prohibited capability packages gain no enabled feature.
fn check_selected_feature_deltas(
    default: &RootedDoc,
    selected: &RootedDoc,
) -> Result<(), SealError> {
    let default_root = field(as_object(&default.doc)?, "root")?;
    let selected_root = field(as_object(&selected.doc)?, "root")?;
    if default_root != selected_root {
        return Err(SealError::SelectedDeltaMismatch);
    }
    let default_root_features = features_for_identity(default, default_root)?;
    let selected_root_features = features_for_identity(selected, selected_root)?;
    if !default_root_features.is_subset(&selected_root_features) {
        return Err(SealError::SelectedDeltaMismatch);
    }
    let root_delta: BTreeSet<&str> =
        selected_root_features.difference(&default_root_features).map(String::as_str).collect();
    if root_delta != BTreeSet::from([SELECTION_PRESIGN]) {
        return Err(SealError::SelectedDeltaMismatch);
    }
    for package_name in PROHIBITED_SELECTED_DELTA_NAMES {
        let mut default_identities: BTreeMap<Vec<u8>, &Value> = BTreeMap::new();
        for identity in doc_array(&default.doc, "packages")? {
            if str_field(as_object(identity)?, "name")? == package_name {
                default_identities.insert(jcs_bytes(identity)?, identity);
            }
        }
        let mut selected_identities: BTreeMap<Vec<u8>, &Value> = BTreeMap::new();
        for identity in doc_array(&selected.doc, "packages")? {
            if str_field(as_object(identity)?, "name")? == package_name {
                selected_identities.insert(jcs_bytes(identity)?, identity);
            }
        }
        if !default_identities.keys().eq(selected_identities.keys()) {
            return Err(SealError::SelectedDeltaMismatch);
        }
        for (identity_bytes, default_identity) in default_identities {
            let selected_identity =
                selected_identities.get(&identity_bytes).ok_or(SealError::SelectedDeltaMismatch)?;
            let default_features = features_for_identity(default, default_identity)?;
            let selected_features = features_for_identity(selected, selected_identity)?;
            if selected_features.difference(&default_features).next().is_some() {
                return Err(SealError::SelectedDeltaMismatch);
            }
        }
    }
    Ok(())
}

/// The submit package's direct resolved normal/build dependencies in the
/// selected raw metadata must be exactly the `presign` allowlist
/// `{alloy-primitives, sha2}`.
fn check_submit_allowlist(selected_raw: &[u8]) -> Result<(), SealError> {
    let meta = parse_raw_metadata(selected_raw)?;
    let mut submit_id: Option<&String> = None;
    for (id, package) in &meta.packages {
        if package.name == SUBMIT_NAME && meta.members.contains(id) {
            if submit_id.is_some() {
                return Err(SealError::SubmitAllowlistMismatch);
            }
            submit_id = Some(id);
        }
    }
    let submit_id = submit_id.ok_or(SealError::SubmitAllowlistMismatch)?;
    let node = meta.nodes.get(submit_id).ok_or(SealError::SubmitAllowlistMismatch)?;
    if node.deps.len() != SUBMIT_PRESIGN_DIRECT_ALLOWLIST.len() {
        return Err(SealError::SubmitAllowlistMismatch);
    }
    let mut direct_ids: BTreeSet<&str> = BTreeSet::new();
    let mut direct_names = Vec::with_capacity(node.deps.len());
    for dep in &node.deps {
        if !direct_ids.insert(dep.as_str()) {
            return Err(SealError::SubmitAllowlistMismatch);
        }
        let package = meta.packages.get(dep).ok_or(SealError::UnresolvedEdge)?;
        direct_names.push(package.name.as_str());
    }
    direct_names.sort_unstable();
    if direct_names.as_slice() != SUBMIT_PRESIGN_DIRECT_ALLOWLIST.as_slice() {
        return Err(SealError::SubmitAllowlistMismatch);
    }
    Ok(())
}

struct P1Files {
    parent_cli: Vec<u8>,
    parent_node: Vec<u8>,
    parent_lock: Vec<u8>,
    parent_lock_sidecar: Vec<u8>,
    cli: Vec<u8>,
    node: Vec<u8>,
    lock: Vec<u8>,
    lock_sidecar: Vec<u8>,
    selected_cli: Vec<u8>,
}

struct P1NormalizedFiles {
    parent_cli: Vec<u8>,
    parent_node: Vec<u8>,
    cli: Vec<u8>,
    node: Vec<u8>,
    selected_cli: Vec<u8>,
}

fn p1_checkout_root(files: &P1Files) -> Result<String, SealError> {
    let roots = [
        parse_raw_metadata(&files.parent_cli)?.workspace_root,
        parse_raw_metadata(&files.parent_node)?.workspace_root,
        parse_raw_metadata(&files.cli)?.workspace_root,
        parse_raw_metadata(&files.node)?.workspace_root,
        parse_raw_metadata(&files.selected_cli)?.workspace_root,
    ];
    let root = roots.first().ok_or(SealError::CheckoutRootMismatch)?;
    if roots.iter().any(|candidate| candidate != root) {
        return Err(SealError::CheckoutRootMismatch);
    }
    Ok(root.clone())
}

/// Recomputes the full implementation seal from its inputs. The optional raw
/// seal (available under `verify`) adds capture-commit and Cargo-version
/// cross-checks; the document bytes do not depend on it.
fn build_implementation_doc(
    registration_seal_bytes: &[u8],
    files: &P1Files,
    normalized: &P1NormalizedFiles,
    root: &WorkspaceRoot,
    raw_seal: Option<&RawSeal>,
) -> Result<Value, SealError> {
    validate_registration_seal(registration_seal_bytes)?;
    let parent_cli = reproduce_normalized(
        &ROW_P1_PARENT_CLI,
        &files.parent_cli,
        &normalized.parent_cli,
        raw_seal,
    )?;
    let parent_node = reproduce_normalized(
        &ROW_P1_PARENT_NODE,
        &files.parent_node,
        &normalized.parent_node,
        raw_seal,
    )?;
    let cli = reproduce_normalized(&ROW_P1_CLI, &files.cli, &normalized.cli, raw_seal)?;
    let node = reproduce_normalized(&ROW_P1_NODE, &files.node, &normalized.node, raw_seal)?;
    let selected_cli = reproduce_normalized(
        &ROW_P1_SELECTED_CLI,
        &files.selected_cli,
        &normalized.selected_cli,
        raw_seal,
    )?;
    if let Some(seal) = raw_seal {
        let checkout_root = p1_checkout_root(files)?;
        if checkout_root != root.as_str()? {
            return Err(SealError::CheckoutRootMismatch);
        }
        let (p1_parent_commit, p1_commit) = check_p1_ref_history(root, seal)?;
        if parent_cli.capture_commit != p1_parent_commit
            || parent_node.capture_commit != p1_parent_commit
            || cli.capture_commit != p1_commit
            || node.capture_commit != p1_commit
            || selected_cli.capture_commit != p1_commit
        {
            return Err(SealError::CaptureCommitMismatch);
        }
    }
    if parent_cli.capture_commit != parent_node.capture_commit {
        return Err(SealError::CommitConsistencyMismatch);
    }
    if cli.capture_commit != node.capture_commit
        || cli.capture_commit != selected_cli.capture_commit
    {
        return Err(SealError::CommitConsistencyMismatch);
    }
    if parent_cli.capture_commit == cli.capture_commit {
        return Err(SealError::CommitConsistencyMismatch);
    }
    let cargo_version = &parent_cli.cargo_version_sha256;
    if [&parent_node, &cli, &node, &selected_cli]
        .iter()
        .any(|rooted| rooted.cargo_version_sha256 != *cargo_version)
    {
        return Err(SealError::CargoVersionMismatch);
    }
    check_sidecar(&files.parent_lock_sidecar, P1_PARENT_LOCK, &files.parent_lock)?;
    check_sidecar(&files.lock_sidecar, P1_LOCK, &files.lock)?;
    check_lock_delta(&files.parent_lock, &files.lock)?;
    let parent_stanzas = parse_lock(&files.parent_lock)?;
    let child_stanzas = parse_lock(&files.lock)?;
    check_lock_dep_qualification(&parent_stanzas)?;
    check_lock_dep_qualification(&child_stanzas)?;
    if lock_universe(&parent_stanzas) != lock_universe(&child_stanzas) {
        return Err(SealError::LockUniverseMismatch);
    }
    check_closures_equal(&parent_cli, &cli)?;
    check_closures_equal(&parent_node, &node)?;
    for rooted in [&parent_cli, &parent_node, &cli, &node] {
        if packages_contain_name(doc_array(&rooted.doc, "packages")?, SUBMIT_NAME)? {
            return Err(SealError::SubmitInDefaultClosure);
        }
    }
    let delta = selected_minus_default(
        doc_array(&selected_cli.doc, "packages")?,
        doc_array(&cli.doc, "packages")?,
    )?;
    let [submit_identity] = delta.as_slice() else {
        return Err(SealError::SelectedDeltaMismatch);
    };
    check_submit_identity(submit_identity)?;
    for prohibited in PROHIBITED_SELECTED_DELTA_NAMES {
        if packages_contain_name(&delta, prohibited)? {
            return Err(SealError::SelectedDeltaMismatch);
        }
    }
    check_submit_selected_features(&selected_cli)?;
    check_selected_feature_deltas(&cli, &selected_cli)?;
    check_submit_allowlist(&files.selected_cli)?;
    let mut raw_bindings = Map::new();
    raw_bindings.insert("p1_cli".to_string(), file_binding(P1_CLI_RAW, &files.cli));
    raw_bindings.insert("p1_lock".to_string(), file_binding(P1_LOCK, &files.lock));
    raw_bindings
        .insert("p1_lock_sidecar".to_string(), file_binding(P1_LOCK_SIDECAR, &files.lock_sidecar));
    raw_bindings.insert("p1_node".to_string(), file_binding(P1_NODE_RAW, &files.node));
    raw_bindings
        .insert("p1_parent_cli".to_string(), file_binding(P1_PARENT_CLI_RAW, &files.parent_cli));
    raw_bindings
        .insert("p1_parent_lock".to_string(), file_binding(P1_PARENT_LOCK, &files.parent_lock));
    raw_bindings.insert(
        "p1_parent_lock_sidecar".to_string(),
        file_binding(P1_PARENT_LOCK_SIDECAR, &files.parent_lock_sidecar),
    );
    raw_bindings
        .insert("p1_parent_node".to_string(), file_binding(P1_PARENT_NODE_RAW, &files.parent_node));
    raw_bindings.insert(
        "p1_selected_cli".to_string(),
        file_binding(P1_SELECTED_CLI_RAW, &files.selected_cli),
    );
    let mut normalized_bindings = Map::new();
    normalized_bindings
        .insert("p1_cli".to_string(), file_binding(P1_CLI_NORMALIZED, &normalized.cli));
    normalized_bindings
        .insert("p1_node".to_string(), file_binding(P1_NODE_NORMALIZED, &normalized.node));
    normalized_bindings.insert(
        "p1_parent_cli".to_string(),
        file_binding(P1_PARENT_CLI_NORMALIZED, &normalized.parent_cli),
    );
    normalized_bindings.insert(
        "p1_parent_node".to_string(),
        file_binding(P1_PARENT_NODE_NORMALIZED, &normalized.parent_node),
    );
    normalized_bindings.insert(
        "p1_selected_cli".to_string(),
        file_binding(P1_SELECTED_CLI_NORMALIZED, &normalized.selected_cli),
    );
    let mut lock_delta = Map::new();
    lock_delta.insert(
        "base_execution_cli_insertions".to_string(),
        Value::Array(
            EXPECTED_CLI_LOCK_INSERTIONS
                .into_iter()
                .map(|line| Value::String(line.to_string()))
                .collect(),
        ),
    );
    lock_delta.insert("changed_line_count".to_string(), Value::Number(6.into()));
    lock_delta.insert("changed_stanza_count".to_string(), Value::Number(2.into()));
    lock_delta.insert(
        "mev_trader_submit_insertions".to_string(),
        Value::Array(vec![Value::String(EXPECTED_SUBMIT_LOCK_INSERTION.to_string())]),
    );
    lock_delta.insert("other_changed_bytes".to_string(), Value::Number(0.into()));
    let mut doc = Map::new();
    doc.insert("schema".to_string(), Value::String(IMPLEMENTATION_SEAL_SCHEMA.to_string()));
    doc.insert(
        "registration_seal".to_string(),
        file_binding(REGISTRATION_SEAL_PATH, registration_seal_bytes),
    );
    doc.insert("raw".to_string(), Value::Object(raw_bindings));
    doc.insert("normalized".to_string(), Value::Object(normalized_bindings));
    doc.insert("lock_delta".to_string(), Value::Object(lock_delta));
    doc.insert(
        "comparisons".to_string(),
        true_comparisons(&[
            "cli_default_features",
            "cli_default_packages",
            "node_default_features",
            "node_default_packages",
            "package_identity_universe",
            "selected_k256_delta_empty",
            "selected_redb_delta_empty",
            "selected_reqwest_delta_empty",
            "selected_submit_exact",
            "submit_presign_direct_allowlist",
        ]),
    );
    doc.insert("verdict".to_string(), Value::String(VERDICT_PASS.to_string()));
    Ok(Value::Object(doc))
}

const NORMALIZE_FLAGS: [&str; 9] = [
    "--checkout-root",
    "--raw-metadata",
    "--capture-ref",
    "--root-manifest",
    "--root-package",
    "--feature-selection",
    "--target",
    "--cargo-version-sha256-from",
    "--output",
];

fn cmd_normalize(args: &[String]) -> Result<(), SealError> {
    let values = parse_flag_values(args, &NORMALIZE_FLAGS)?;
    let [
        checkout_root,
        raw_metadata,
        capture_ref,
        root_manifest,
        root_package,
        feature_selection,
        target,
        seal_path,
        output,
    ]: [String; 9] = values.try_into().map_err(|_| SealError::OmittedFlag)?;
    if target != TARGET_TRIPLE {
        return Err(SealError::UnlistedFlagValue);
    }
    if seal_path != RAW_SEAL_PATH {
        return Err(SealError::UnlistedFlagValue);
    }
    let row = NORMALIZE_ROWS
        .into_iter()
        .find(|row| {
            row.raw == raw_metadata
                && row.capture_ref == capture_ref
                && row.root_manifest == root_manifest
                && row.root_package == root_package
                && row.feature_selection == feature_selection
                && row.output == output
        })
        .ok_or(SealError::UnlistedFlagValue)?;
    let root = WorkspaceRoot::explicit(&checkout_root)?;
    let root_text = root.as_str()?;
    let seal = parse_raw_seal(&root.read(RAW_SEAL_PATH)?)?;
    let raw_bytes = root.read(row.raw)?;
    let capture_commit = git_resolve_commit(&root, row.capture_ref)?;
    check_capture_commit(row.class, &capture_commit, &seal)?;
    if matches!(row.class, RefClass::P1Parent | RefClass::P1) {
        check_p1_ref_history(&root, &seal)?;
    }
    let rooted = build_rooted_doc(
        &raw_bytes,
        row,
        &capture_commit,
        &seal.cargo_version_sha256,
        Some(root_text),
    )?;
    root.write_create_new(row.output, &rooted.bytes)
}

const REGISTRATION_FLAG_SPEC: [(&str, &str); 14] = [
    ("--raw-seal", RAW_SEAL_PATH),
    ("--p0-parent-cli-raw", P0_PARENT_CLI_RAW),
    ("--p0-parent-node-raw", P0_PARENT_NODE_RAW),
    ("--p0-parent-lock", P0_PARENT_LOCK),
    ("--p0-parent-lock-sidecar", P0_PARENT_LOCK_SIDECAR),
    ("--p0-cli-raw", P0_CLI_RAW),
    ("--p0-node-raw", P0_NODE_RAW),
    ("--p0-lock", P0_LOCK),
    ("--p0-lock-sidecar", P0_LOCK_SIDECAR),
    ("--p0-parent-cli-normalized", P0_PARENT_CLI_NORMALIZED),
    ("--p0-parent-node-normalized", P0_PARENT_NODE_NORMALIZED),
    ("--p0-cli-normalized", P0_CLI_NORMALIZED),
    ("--p0-node-normalized", P0_NODE_NORMALIZED),
    ("--output", REGISTRATION_SEAL_PATH),
];

fn read_p0_files(root: &WorkspaceRoot) -> Result<P0Files, SealError> {
    Ok(P0Files {
        parent_cli: root.read(P0_PARENT_CLI_RAW)?,
        parent_node: root.read(P0_PARENT_NODE_RAW)?,
        parent_lock: root.read(P0_PARENT_LOCK)?,
        parent_lock_sidecar: root.read(P0_PARENT_LOCK_SIDECAR)?,
        cli: root.read(P0_CLI_RAW)?,
        node: root.read(P0_NODE_RAW)?,
        lock: root.read(P0_LOCK)?,
        lock_sidecar: root.read(P0_LOCK_SIDECAR)?,
    })
}

fn read_p0_normalized(root: &WorkspaceRoot) -> Result<P0NormalizedFiles, SealError> {
    Ok(P0NormalizedFiles {
        parent_cli: root.read(P0_PARENT_CLI_NORMALIZED)?,
        parent_node: root.read(P0_PARENT_NODE_NORMALIZED)?,
        cli: root.read(P0_CLI_NORMALIZED)?,
        node: root.read(P0_NODE_NORMALIZED)?,
    })
}

fn cmd_seal_registration(args: &[String]) -> Result<(), SealError> {
    parse_literal_flags(args, &REGISTRATION_FLAG_SPEC)?;
    let root = WorkspaceRoot::current()?;
    let raw_seal_bytes = root.read(RAW_SEAL_PATH)?;
    let files = read_p0_files(&root)?;
    let normalized = read_p0_normalized(&root)?;
    let doc = build_registration_doc(&raw_seal_bytes, &files, &normalized)?;
    root.write_create_new(REGISTRATION_SEAL_PATH, &jcs_bytes(&doc)?)
}

const IMPLEMENTATION_FLAG_SPEC: [(&str, &str); 16] = [
    ("--registration-seal", REGISTRATION_SEAL_PATH),
    ("--p1-parent-cli-raw", P1_PARENT_CLI_RAW),
    ("--p1-parent-node-raw", P1_PARENT_NODE_RAW),
    ("--p1-parent-lock", P1_PARENT_LOCK),
    ("--p1-parent-lock-sidecar", P1_PARENT_LOCK_SIDECAR),
    ("--p1-cli-raw", P1_CLI_RAW),
    ("--p1-node-raw", P1_NODE_RAW),
    ("--p1-lock", P1_LOCK),
    ("--p1-lock-sidecar", P1_LOCK_SIDECAR),
    ("--p1-selected-cli-raw", P1_SELECTED_CLI_RAW),
    ("--p1-parent-cli-normalized", P1_PARENT_CLI_NORMALIZED),
    ("--p1-parent-node-normalized", P1_PARENT_NODE_NORMALIZED),
    ("--p1-cli-normalized", P1_CLI_NORMALIZED),
    ("--p1-node-normalized", P1_NODE_NORMALIZED),
    ("--p1-selected-cli-normalized", P1_SELECTED_CLI_NORMALIZED),
    ("--output", IMPLEMENTATION_SEAL_PATH),
];

fn read_p1_files(root: &WorkspaceRoot) -> Result<P1Files, SealError> {
    Ok(P1Files {
        parent_cli: root.read(P1_PARENT_CLI_RAW)?,
        parent_node: root.read(P1_PARENT_NODE_RAW)?,
        parent_lock: root.read(P1_PARENT_LOCK)?,
        parent_lock_sidecar: root.read(P1_PARENT_LOCK_SIDECAR)?,
        cli: root.read(P1_CLI_RAW)?,
        node: root.read(P1_NODE_RAW)?,
        lock: root.read(P1_LOCK)?,
        lock_sidecar: root.read(P1_LOCK_SIDECAR)?,
        selected_cli: root.read(P1_SELECTED_CLI_RAW)?,
    })
}

fn read_p1_normalized(root: &WorkspaceRoot) -> Result<P1NormalizedFiles, SealError> {
    Ok(P1NormalizedFiles {
        parent_cli: root.read(P1_PARENT_CLI_NORMALIZED)?,
        parent_node: root.read(P1_PARENT_NODE_NORMALIZED)?,
        cli: root.read(P1_CLI_NORMALIZED)?,
        node: root.read(P1_NODE_NORMALIZED)?,
        selected_cli: root.read(P1_SELECTED_CLI_NORMALIZED)?,
    })
}

fn cmd_seal_implementation(args: &[String]) -> Result<(), SealError> {
    parse_literal_flags(args, &IMPLEMENTATION_FLAG_SPEC)?;
    let root = WorkspaceRoot::current()?;
    let registration_seal_bytes = root.read(REGISTRATION_SEAL_PATH)?;
    let files = read_p1_files(&root)?;
    let normalized = read_p1_normalized(&root)?;
    let doc = build_implementation_doc(&registration_seal_bytes, &files, &normalized, &root, None)?;
    root.write_create_new(IMPLEMENTATION_SEAL_PATH, &jcs_bytes(&doc)?)
}

const VERIFY_FLAG_SPEC: [(&str, &str); 30] = [
    ("--raw-seal", RAW_SEAL_PATH),
    ("--registration-seal", REGISTRATION_SEAL_PATH),
    ("--implementation-seal", IMPLEMENTATION_SEAL_PATH),
    ("--p0-parent-cli-raw", P0_PARENT_CLI_RAW),
    ("--p0-parent-node-raw", P0_PARENT_NODE_RAW),
    ("--p0-parent-lock", P0_PARENT_LOCK),
    ("--p0-parent-lock-sidecar", P0_PARENT_LOCK_SIDECAR),
    ("--p0-cli-raw", P0_CLI_RAW),
    ("--p0-node-raw", P0_NODE_RAW),
    ("--p0-lock", P0_LOCK),
    ("--p0-lock-sidecar", P0_LOCK_SIDECAR),
    ("--p1-parent-cli-raw", P1_PARENT_CLI_RAW),
    ("--p1-parent-node-raw", P1_PARENT_NODE_RAW),
    ("--p1-parent-lock", P1_PARENT_LOCK),
    ("--p1-parent-lock-sidecar", P1_PARENT_LOCK_SIDECAR),
    ("--p1-cli-raw", P1_CLI_RAW),
    ("--p1-node-raw", P1_NODE_RAW),
    ("--p1-lock", P1_LOCK),
    ("--p1-lock-sidecar", P1_LOCK_SIDECAR),
    ("--p1-selected-cli-raw", P1_SELECTED_CLI_RAW),
    ("--p0-parent-cli-normalized", P0_PARENT_CLI_NORMALIZED),
    ("--p0-parent-node-normalized", P0_PARENT_NODE_NORMALIZED),
    ("--p0-cli-normalized", P0_CLI_NORMALIZED),
    ("--p0-node-normalized", P0_NODE_NORMALIZED),
    ("--p1-parent-cli-normalized", P1_PARENT_CLI_NORMALIZED),
    ("--p1-parent-node-normalized", P1_PARENT_NODE_NORMALIZED),
    ("--p1-cli-normalized", P1_CLI_NORMALIZED),
    ("--p1-node-normalized", P1_NODE_NORMALIZED),
    ("--p1-selected-cli-normalized", P1_SELECTED_CLI_NORMALIZED),
    ("--output", VERIFY_OUTPUT_PATH),
];

fn sorted_bindings(pairs: &[(&str, &[u8])]) -> Value {
    let mut sorted: Vec<(&str, &[u8])> = pairs.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    Value::Array(sorted.into_iter().map(|(path, bytes)| file_binding(path, bytes)).collect())
}

fn cmd_verify(args: &[String]) -> Result<(), SealError> {
    parse_literal_flags(args, &VERIFY_FLAG_SPEC)?;
    let root = WorkspaceRoot::current()?;
    let raw_seal_bytes = root.read(RAW_SEAL_PATH)?;
    let registration_seal_bytes = root.read(REGISTRATION_SEAL_PATH)?;
    let implementation_seal_bytes = root.read(IMPLEMENTATION_SEAL_PATH)?;
    let p0_files = read_p0_files(&root)?;
    let p0_normalized = read_p0_normalized(&root)?;
    let p1_files = read_p1_files(&root)?;
    let p1_normalized = read_p1_normalized(&root)?;
    if p1_checkout_root(&p1_files)? != root.as_str()? {
        return Err(SealError::CheckoutRootMismatch);
    }
    let seal = parse_raw_seal(&raw_seal_bytes)?;
    check_raw_seal_files(&seal, &p0_files)?;
    let registration_doc = build_registration_doc(&raw_seal_bytes, &p0_files, &p0_normalized)?;
    if jcs_bytes(&registration_doc)? != registration_seal_bytes {
        return Err(SealError::SealReproductionMismatch);
    }
    let implementation_doc = build_implementation_doc(
        &registration_seal_bytes,
        &p1_files,
        &p1_normalized,
        &root,
        Some(&seal),
    )?;
    if jcs_bytes(&implementation_doc)? != implementation_seal_bytes {
        return Err(SealError::SealReproductionMismatch);
    }
    let raw_pairs: [(&str, &[u8]); 17] = [
        (P0_PARENT_CLI_RAW, &p0_files.parent_cli),
        (P0_PARENT_NODE_RAW, &p0_files.parent_node),
        (P0_PARENT_LOCK, &p0_files.parent_lock),
        (P0_PARENT_LOCK_SIDECAR, &p0_files.parent_lock_sidecar),
        (P0_CLI_RAW, &p0_files.cli),
        (P0_NODE_RAW, &p0_files.node),
        (P0_LOCK, &p0_files.lock),
        (P0_LOCK_SIDECAR, &p0_files.lock_sidecar),
        (P1_PARENT_CLI_RAW, &p1_files.parent_cli),
        (P1_PARENT_NODE_RAW, &p1_files.parent_node),
        (P1_PARENT_LOCK, &p1_files.parent_lock),
        (P1_PARENT_LOCK_SIDECAR, &p1_files.parent_lock_sidecar),
        (P1_CLI_RAW, &p1_files.cli),
        (P1_NODE_RAW, &p1_files.node),
        (P1_LOCK, &p1_files.lock),
        (P1_LOCK_SIDECAR, &p1_files.lock_sidecar),
        (P1_SELECTED_CLI_RAW, &p1_files.selected_cli),
    ];
    let normalized_pairs: [(&str, &[u8]); 9] = [
        (P0_PARENT_CLI_NORMALIZED, &p0_normalized.parent_cli),
        (P0_PARENT_NODE_NORMALIZED, &p0_normalized.parent_node),
        (P0_CLI_NORMALIZED, &p0_normalized.cli),
        (P0_NODE_NORMALIZED, &p0_normalized.node),
        (P1_PARENT_CLI_NORMALIZED, &p1_normalized.parent_cli),
        (P1_PARENT_NODE_NORMALIZED, &p1_normalized.parent_node),
        (P1_CLI_NORMALIZED, &p1_normalized.cli),
        (P1_NODE_NORMALIZED, &p1_normalized.node),
        (P1_SELECTED_CLI_NORMALIZED, &p1_normalized.selected_cli),
    ];
    let mut doc = Map::new();
    doc.insert("schema".to_string(), Value::String(VERIFY_SCHEMA.to_string()));
    doc.insert("raw_inputs".to_string(), sorted_bindings(&raw_pairs));
    doc.insert("normalized_outputs".to_string(), sorted_bindings(&normalized_pairs));
    doc.insert("raw_seal".to_string(), file_binding(RAW_SEAL_PATH, &raw_seal_bytes));
    doc.insert(
        "registration_seal".to_string(),
        file_binding(REGISTRATION_SEAL_PATH, &registration_seal_bytes),
    );
    doc.insert(
        "implementation_seal".to_string(),
        file_binding(IMPLEMENTATION_SEAL_PATH, &implementation_seal_bytes),
    );
    doc.insert(
        "reproduction".to_string(),
        true_comparisons(&[
            "implementation_seal_byte_identical",
            "normalized_byte_identical",
            "raw_seal_valid",
            "registration_seal_byte_identical",
        ]),
    );
    doc.insert("verdict".to_string(), Value::String(VERDICT_PASS.to_string()));
    root.write_create_new(VERIFY_OUTPUT_PATH, &jcs_bytes(&Value::Object(doc))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_directory(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must follow the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir()
            .join(format!("b5-cargo-graph-seal-{label}-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&path).expect("temporary directory must be created");
        path
    }

    fn lock_with_dependency(dependency: &str) -> Vec<u8> {
        format!(
            "version = 4\n\n[[package]]\nname = \"owner\"\nversion = \"1.0.0\"\ndependencies = [\n \"{dependency}\",\n]\n\n[[package]]\nname = \"duplicate\"\nversion = \"2.0.0\"\nsource = \"registry+https://one.invalid/index\"\nchecksum = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n\n[[package]]\nname = \"duplicate\"\nversion = \"2.0.0\"\nsource = \"registry+https://two.invalid/index\"\nchecksum = \"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\"\n"
        )
        .into_bytes()
    }
    fn test_identity(name: &str, version: &str) -> Value {
        serde_json::json!({
            "checksum": null,
            "kind": "external",
            "name": name,
            "source": "registry+https://example.invalid/index",
            "version": version,
        })
    }

    fn test_rooted_doc(root: &Value, packages: &[Value], features: &[(&Value, &str)]) -> RootedDoc {
        let feature_entries: Vec<Value> = features
            .iter()
            .map(|(package, feature)| {
                serde_json::json!({
                    "feature": feature,
                    "package": package,
                })
            })
            .collect();
        RootedDoc {
            bytes: Vec::new(),
            doc: serde_json::json!({
                "features": feature_entries,
                "packages": packages,
                "root": root,
            }),
            capture_commit: String::new(),
            cargo_version_sha256: String::new(),
        }
    }

    fn submit_metadata(deps: &[&str]) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "workspace_root": "/checkout",
            "workspace_members": ["submit-id"],
            "packages": [
                {
                    "id": "submit-id",
                    "name": SUBMIT_NAME,
                    "version": SUBMIT_VERSION,
                    "manifest_path": "/checkout/crates/execution/mev-trader-submit/Cargo.toml",
                    "source": null,
                    "checksum": null,
                },
                {
                    "id": "alloy-id",
                    "name": "alloy-primitives",
                    "version": "1.0.0",
                    "manifest_path": "/registry/alloy-primitives/Cargo.toml",
                    "source": "registry+https://example.invalid/index",
                    "checksum": null,
                },
                {
                    "id": "sha-id-a",
                    "name": "sha2",
                    "version": "0.10.0",
                    "manifest_path": "/registry/sha2-a/Cargo.toml",
                    "source": "registry+https://example.invalid/index",
                    "checksum": null,
                },
                {
                    "id": "sha-id-b",
                    "name": "sha2",
                    "version": "0.11.0",
                    "manifest_path": "/registry/sha2-b/Cargo.toml",
                    "source": "registry+https://other.invalid/index",
                    "checksum": null,
                }
            ],
            "resolve": {
                "nodes": [{
                    "id": "submit-id",
                    "deps": deps.iter().map(|pkg| serde_json::json!({
                        "pkg": pkg,
                        "dep_kinds": [{"kind": null}]
                    })).collect::<Vec<_>>(),
                    "features": [SUBMIT_PRESIGN_FEATURE],
                }]
            }
        }))
        .expect("test metadata must serialize")
    }

    #[test]
    fn unrelated_p0_object_parent_is_rejected() {
        let a = "1111111111111111111111111111111111111111";
        let b = "2222222222222222222222222222222222222222";
        let unrelated = "3333333333333333333333333333333333333333";
        let c = "4444444444444444444444444444444444444444";
        assert!(matches!(
            check_history_commits([a, b, unrelated, b, c, b], [a, b]),
            Err(SealError::CaptureCommitMismatch)
        ));
        assert!(check_history_commits([a, b, a, b, c, b], [a, b]).is_ok());
    }

    #[test]
    fn commit_parent_parser_requires_exactly_one_lowercase_parent() {
        let parent = "1111111111111111111111111111111111111111";
        let single = format!(
            "tree 2222222222222222222222222222222222222222\nparent {parent}\nauthor a\n\nmessage"
        );
        assert_eq!(
            parse_single_commit_parent(single.as_bytes()).expect("single parent must parse"),
            parent
        );

        let root = b"tree 2222222222222222222222222222222222222222\nauthor a\n\nmessage";
        assert!(matches!(parse_single_commit_parent(root), Err(SealError::GitRefResolutionFailed)));

        let merge = format!(
            "tree 2222222222222222222222222222222222222222\nparent {parent}\nparent 3333333333333333333333333333333333333333\nauthor a\n\nmessage"
        );
        assert!(matches!(
            parse_single_commit_parent(merge.as_bytes()),
            Err(SealError::GitRefResolutionFailed)
        ));

        let uppercase =
            b"tree 2222222222222222222222222222222222222222\nparent A111111111111111111111111111111111111111\nauthor a\n\nmessage";
        assert!(matches!(
            parse_single_commit_parent(uppercase),
            Err(SealError::GitRefResolutionFailed)
        ));
    }

    #[test]
    fn git_invocations_are_hermetic_and_descriptor_rooted() {
        use std::ffi::{OsStr, OsString};

        let command = git_command_at(Path::new("/proc/self/fd/17"));
        let environment: BTreeMap<OsString, Option<OsString>> = command
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.map(OsStr::to_owned)))
            .collect();
        let expected = BTreeMap::from([
            (OsString::from("GIT_CONFIG_NOSYSTEM"), Some(OsString::from("1"))),
            (OsString::from("GIT_NO_REPLACE_OBJECTS"), Some(OsString::from("1"))),
            (OsString::from("LC_ALL"), Some(OsString::from("C"))),
        ]);

        assert_eq!(command.get_program(), OsStr::new("/usr/bin/git"));
        assert!(command.get_args().next().is_none());
        assert_eq!(command.get_current_dir(), Some(Path::new("/proc/self/fd/17")));
        assert_eq!(environment, expected);
    }

    #[test]
    fn prohibited_feature_deltas_are_checked_per_exact_identity() {
        let root = test_identity("base-execution-cli", "1.0.0");
        let k256_a = test_identity("k256", "0.13.0");
        let k256_b = test_identity("k256", "0.14.0");
        let packages = [root.clone(), k256_a.clone(), k256_b.clone()];
        let default = test_rooted_doc(&root, &packages, &[(&k256_a, "shared")]);
        let selected = test_rooted_doc(
            &root,
            &packages,
            &[(&root, SELECTION_PRESIGN), (&k256_a, "shared"), (&k256_b, "shared")],
        );

        assert!(matches!(
            check_selected_feature_deltas(&default, &selected),
            Err(SealError::SelectedDeltaMismatch)
        ));
    }

    #[test]
    fn submit_allowlist_requires_exactly_two_distinct_resolved_ids() {
        assert!(check_submit_allowlist(&submit_metadata(&["alloy-id", "sha-id-a"])).is_ok());

        assert!(matches!(
            check_submit_allowlist(&submit_metadata(&["alloy-id", "alloy-id"])),
            Err(SealError::SubmitAllowlistMismatch)
        ));
        assert!(matches!(
            check_submit_allowlist(&submit_metadata(&["sha-id-a", "sha-id-b"])),
            Err(SealError::SubmitAllowlistMismatch)
        ));
        assert!(matches!(
            check_submit_allowlist(&submit_metadata(&["alloy-id", "sha-id-a", "sha-id-b"])),
            Err(SealError::SubmitAllowlistMismatch)
        ));
    }

    #[test]
    fn lock_qualification_counts_same_version_different_sources() {
        let ambiguous =
            parse_lock(&lock_with_dependency("duplicate 2.0.0")).expect("lock fixture must parse");
        assert!(matches!(
            check_lock_dep_qualification(&ambiguous),
            Err(SealError::LockDuplicateNameUnqualified)
        ));

        let unqualified =
            parse_lock(&lock_with_dependency("duplicate")).expect("lock fixture must parse");
        assert!(matches!(
            check_lock_dep_qualification(&unqualified),
            Err(SealError::LockDuplicateNameUnqualified)
        ));

        let qualified = parse_lock(&lock_with_dependency(
            "duplicate 2.0.0 (registry+https://two.invalid/index)",
        ))
        .expect("lock fixture must parse");
        assert!(check_lock_dep_qualification(&qualified).is_ok());

        let git_qualified_bytes = String::from_utf8(lock_with_dependency(
            "duplicate 2.0.0 (git+https://two.invalid/repo?rev=abc)",
        ))
        .expect("lock fixture must be UTF-8")
        .replace(
            "registry+https://two.invalid/index",
            "git+https://two.invalid/repo?rev=abc#abcdef",
        )
        .into_bytes();
        let git_qualified = parse_lock(&git_qualified_bytes).expect("git lock fixture must parse");
        assert!(check_lock_dep_qualification(&git_qualified).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_input_and_output_parent_are_rejected() {
        use std::os::unix::fs::symlink;

        let directory = temporary_directory("symlinks");
        let root = WorkspaceRoot::validate(&directory).expect("root must validate");
        std::fs::write(directory.join("input"), b"evidence").expect("input must be written");
        symlink(directory.join("input"), directory.join("input-link"))
            .expect("input symlink must be created");
        assert!(matches!(root.read("input-link"), Err(SealError::EvidencePathInvalid)));

        std::fs::create_dir(directory.join("real-output")).expect("output directory must exist");
        symlink(directory.join("real-output"), directory.join("output-link"))
            .expect("output symlink must be created");
        assert!(matches!(
            root.write_create_new("output-link/receipt.json", b"{}"),
            Err(SealError::EvidencePathInvalid)
        ));
        assert!(!directory.join("real-output/receipt.json").exists());

        std::fs::remove_dir_all(directory).expect("temporary directory must be removed");
    }

    #[test]
    fn descriptor_root_survives_rename_and_enforces_regular_create_new_io() {
        let directory = temporary_directory("descriptor-root");
        let renamed = directory.with_extension("renamed");
        let root = WorkspaceRoot::validate(&directory).expect("root must validate");
        std::fs::rename(&directory, &renamed).expect("root rename must succeed");

        root.write_create_new("nested/output.json", b"sealed")
            .expect("descriptor-rooted output must be written");
        assert_eq!(
            root.read("nested/output.json").expect("descriptor-rooted input must be read"),
            b"sealed"
        );
        assert!(matches!(
            root.write_create_new("nested/output.json", b"replacement"),
            Err(SealError::CreateOutputFailed)
        ));
        assert!(matches!(root.read("nested"), Err(SealError::EvidencePathInvalid)));

        std::fs::remove_dir_all(renamed).expect("temporary directory must be removed");
    }

    #[test]
    fn evidence_paths_reject_parent_traversal() {
        let directory = temporary_directory("traversal");
        let root = WorkspaceRoot::validate(&directory).expect("root must validate");
        assert!(matches!(root.components("../outside"), Err(SealError::EvidencePathInvalid)));
        assert!(matches!(root.components("/absolute"), Err(SealError::EvidencePathInvalid)));
        assert!(matches!(root.components("."), Err(SealError::EvidencePathInvalid)));
        assert!(matches!(root.components(""), Err(SealError::EvidencePathInvalid)));
        std::fs::remove_dir_all(directory).expect("temporary directory must be removed");
    }
}
