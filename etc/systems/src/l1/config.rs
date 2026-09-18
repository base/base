//! Configuration for L1 container images, identity, naming, and port binding.

use std::{fs::OpenOptions, io::Write, path::PathBuf, sync::Mutex};

use eyre::{Result, WrapErr, ensure};
use testcontainers::{
    GenericImage, ImageExt,
    core::{ContainerRequest, logs::LogFrame},
};

/// Configuration for stable container naming and port binding.
/// Used when running containers with fixed names and ports.
#[derive(Debug, Clone, Default)]
pub struct L1ContainerConfig {
    /// Optional immutable Reth image override.
    pub reth_image: Option<L1Image>,
    /// Optional immutable Lighthouse image override.
    pub lighthouse_image: Option<L1Image>,
    /// If set, stream client logs here so startup failures retain diagnostics.
    pub diagnostics_dir: Option<PathBuf>,
    /// If true, use stable container names (e.g., "l1-reth") instead of unique names
    pub use_stable_names: bool,
    /// If set, use this network instead of the default randomized network
    pub network_name: Option<String>,
    /// Lets testcontainers create and remove the uniquely named network.
    pub auto_remove_network: bool,
    /// If set, bind to this specific host port for HTTP RPC
    pub http_port: Option<u16>,
    /// If set, bind to this specific host port for Engine API
    pub engine_port: Option<u16>,
    /// If set, bind to this specific host port for beacon HTTP API
    pub beacon_http_port: Option<u16>,
    /// If set, bind to this specific host port for beacon P2P
    pub beacon_p2p_port: Option<u16>,
    /// If true, back the container datadir (`/data`) with tmpfs. reth's mdbx and lighthouse's
    /// database need a writable `MAP_SHARED` mmap, which some container storage backends (e.g.
    /// overlayfs on docker-in-docker CI runners) reject; tmpfs supports it. No effect where the
    /// storage already supports mmap.
    pub tmpfs_datadir: bool,
    /// If true, supervise Reth so system tests can stop the node process, mutate its database, and
    /// restart it without replacing the container.
    pub enable_reorg_control: bool,
}

/// An immutable registry image reference for an L1 client.
#[derive(Debug, Clone)]
pub struct L1Image {
    reference: String,
}

impl L1Image {
    /// Validates a `repository@sha256:<64 hex digits>` image reference.
    pub fn new(reference: impl Into<String>) -> Result<Self> {
        let reference = reference.into();
        let (repository, digest) = reference
            .split_once("@sha256:")
            .ok_or_else(|| eyre::eyre!("L1 image must use an immutable sha256 digest"))?;
        ensure!(
            !repository.is_empty() && !repository.chars().any(char::is_whitespace),
            "invalid image repository"
        );
        ensure!(
            digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid image sha256 digest"
        );
        Ok(Self { reference })
    }

    /// Returns the complete immutable image reference.
    pub fn reference(&self) -> &str {
        &self.reference
    }

    /// Creates a testcontainers image preserving the registry digest.
    pub fn image(&self) -> GenericImage {
        let (name, digest) = self.reference.rsplit_once(':').expect("validated digest reference");
        GenericImage::new(name, digest)
    }
}

impl L1ContainerConfig {
    /// Attaches a generic startup log consumer when diagnostics are requested.
    pub fn capture_logs(
        &self,
        request: ContainerRequest<GenericImage>,
        component: &str,
    ) -> Result<ContainerRequest<GenericImage>> {
        let Some(directory) = &self.diagnostics_dir else { return Ok(request) };
        std::fs::create_dir_all(directory).wrap_err("create L1 diagnostics directory")?;
        let path = directory.join(format!("{component}.stream.log"));
        let file = Mutex::new(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .wrap_err_with(|| format!("create L1 startup log {}", path.display()))?,
        );
        Ok(request.with_log_consumer(move |frame: &LogFrame| {
            if let Err(error) = file.lock().expect("L1 log lock poisoned").write_all(frame.bytes())
            {
                tracing::error!(error = %error, path = %path.display(), "failed to retain L1 log");
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use testcontainers::core::ContainerRequest;

    use super::L1Image;

    #[test]
    fn digest_reference_preserves_registry_port() {
        let reference = format!("localhost:5000/reth@sha256:{}", "ab".repeat(32));
        let request: ContainerRequest<_> = L1Image::new(&reference).unwrap().image().into();
        assert_eq!(request.descriptor(), reference);
    }

    #[test]
    fn rejects_mutable_images() {
        assert!(L1Image::new("reth:latest").is_err());
        assert!(L1Image::new("reth@sha256:abc").is_err());
    }
}
