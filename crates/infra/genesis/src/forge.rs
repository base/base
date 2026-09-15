//! Pinned Forge execution and allocation export.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use alloy_genesis::GenesisAccount;
use alloy_primitives::Address;
use eyre::{Result, WrapErr, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Prepared contracts provenance and file digests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractManifest {
    /// Contract source revision.
    pub revision: String,
    /// Exact Forge version output used to build the bundle.
    pub forge_version: String,
    /// SHA-256 hashes of the contract sources and artifacts, relative to the project.
    pub files: BTreeMap<PathBuf, String>,
}

/// Verified artifacts and allocation exports from the Base contracts project.
#[derive(Debug)]
pub struct GenesisForge {
    /// Prepared contracts directory.
    pub project: PathBuf,
    /// Digest of the verified contract manifest.
    pub manifest_digest: String,
}

impl GenesisForge {
    /// Verify contract artifacts before using them.
    pub fn open(directory: &Path) -> Result<Self> {
        let manifest: ContractManifest = serde_json::from_slice(
            &fs::read(directory.join("manifest.json"))
                .wrap_err("missing genesis artifacts; run `just build genesis-contracts`")?,
        )?;
        let project = directory.join("contracts").canonicalize()?;
        ensure!(!manifest.files.is_empty(), "empty contract manifest");
        for (path, digest) in &manifest.files {
            ensure!(
                path.is_relative()
                    && !path.components().any(|c| c == std::path::Component::ParentDir),
                "invalid artifact path"
            );
            ensure!(
                Self::digest(&fs::read(project.join(path))?) == *digest,
                "contract artifact changed: {}",
                path.display()
            );
        }
        Ok(Self { project, manifest_digest: Self::digest(&serde_json::to_vec(&manifest)?) })
    }

    /// SHA-256 file digest used by the build recipe and output manifests.
    pub fn digest(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    /// Load final account state, removing execution helpers and empty storage words.
    pub fn allocs(directory: &Path) -> Result<BTreeMap<Address, GenesisAccount>> {
        let mut alloc: BTreeMap<Address, GenesisAccount> =
            serde_json::from_slice(&fs::read(directory.join("alloc.json"))?)?;
        let cleanup: BTreeMap<String, Vec<Address>> =
            serde_json::from_slice(&fs::read(directory.join("cleanup.json"))?)?;
        for address in
            cleanup.get("helpers").ok_or_else(|| eyre::eyre!("missing helper account list"))?
        {
            alloc.remove(address);
        }
        for account in alloc.values_mut() {
            if let Some(storage) = &mut account.storage {
                storage.retain(|_, value| !value.is_zero());
            }
        }
        ensure!(!alloc.is_empty(), "Forge produced empty allocations");
        Ok(alloc)
    }
}
