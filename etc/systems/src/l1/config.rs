//! Configuration for L1 container identity, naming, ports, and diagnostics.

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
    /// Fixture-specific Reth image, pinned by registry digest. The system-test default is unchanged.
    pub reth_image: Option<L1Image>,
    /// Fixture-specific Lighthouse image used by both beacon and validator clients.
    pub lighthouse_image: Option<L1Image>,
    /// If set, stream component logs here, including failures during container startup.
    pub diagnostics_dir: Option<PathBuf>,
    /// If true, use stable container names (e.g., "l1-reth") instead of unique names
    pub use_stable_names: bool,
    /// If set, use this network instead of the default randomized network
    pub network_name: Option<String>,
    /// If true, let testcontainers own/create this uniquely named network and remove it on drop.
    /// Do not use this with caller-owned or stable networks.
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

    /// Creates a testcontainers image preserving the digest in its image descriptor.
    pub fn image(&self) -> GenericImage {
        // testcontainers joins name and tag with ':'. Split at the final colon, not the
        // registry's optional port, to preserve the complete repository@sha256 reference.
        let (name, digest) = self.reference.rsplit_once(':').expect("validated digest reference");
        GenericImage::new(name, digest)
    }
}

impl L1ContainerConfig {
    /// Adds log collection and artifact ownership labels before startup.
    /// The labels allow the acceptance runner to retain diagnostics and clean up after a killed test.
    pub fn capture_logs(
        &self,
        request: ContainerRequest<GenericImage>,
        component: &str,
    ) -> Result<ContainerRequest<GenericImage>> {
        let Some(directory) = &self.diagnostics_dir else { return Ok(request) };
        std::fs::create_dir_all(directory).wrap_err("create L1 diagnostics directory")?;
        let directory =
            directory.canonicalize().wrap_err("canonicalize L1 diagnostics directory")?;
        let artifacts = directory
            .to_str()
            .ok_or_else(|| eyre::eyre!("L1 diagnostics directory must be valid UTF-8"))?;
        let network = request.network().clone();
        let mut request = request
            .with_label("org.base.glamsterdam.artifacts", artifacts)
            .with_label("org.base.glamsterdam.component", component);
        if let Some(network) = network {
            request = request.with_label("org.base.glamsterdam.network", network);
        }
        let path = directory.join(format!("{component}.stream.log"));
        let file = Mutex::new(
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
                .wrap_err_with(|| format!("create component log {}", path.display()))?,
        );
        Ok(request.with_log_consumer(move |frame: &LogFrame| {
            let result = file.lock().expect("component log lock poisoned").write_all(frame.bytes());
            if let Err(error) = result {
                tracing::error!(%error, path = %path.display(), "failed to retain L1 component log");
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use testcontainers::{GenericImage, ImageExt, core::ContainerRequest};

    use super::{L1ContainerConfig, L1Image};

    #[test]
    fn diagnostics_label_identifies_canonical_artifact_directory() {
        let directory = tempfile::tempdir().unwrap();
        let config = L1ContainerConfig {
            diagnostics_dir: Some(directory.path().join("runtime/../runtime")),
            ..Default::default()
        };
        let request = config
            .capture_logs(
                GenericImage::new("alpine", "latest").with_network("glamsterdam-l1-12345678"),
                "reth",
            )
            .unwrap();
        assert_eq!(
            request.labels()["org.base.glamsterdam.artifacts"],
            directory.path().join("runtime").canonicalize().unwrap().to_str().unwrap()
        );
        assert_eq!(request.labels()["org.base.glamsterdam.component"], "reth");
        assert_eq!(request.labels()["org.base.glamsterdam.network"], "glamsterdam-l1-12345678");
    }

    #[test]
    fn no_diagnostics_does_not_claim_container_ownership() {
        let request = L1ContainerConfig::default()
            .capture_logs(GenericImage::new("alpine", "latest").into(), "reth")
            .unwrap();
        assert!(request.labels().is_empty());
    }

    #[test]
    fn digest_reference_preserves_registry_port() {
        let reference = format!("localhost:5000/reth@sha256:{}", "ab".repeat(32));
        let image = L1Image::new(&reference).unwrap();
        let request: ContainerRequest<_> = image.image().into();
        assert_eq!(request.descriptor(), reference);
    }

    #[test]
    fn rejects_mutable_or_malformed_images() {
        for reference in ["reth:latest", "reth:v2.6.0", "reth@sha256:abc", "@sha256:abc"] {
            assert!(L1Image::new(reference).is_err(), "accepted {reference}");
        }
    }
}
