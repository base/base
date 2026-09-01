//! Configuration for the telemetry client.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use base_retry::RetryConfig;
use url::Url;

/// Short git SHA of this build, or `unknown` when the build had no git metadata.
pub const GIT_SHA: &str = env!("BASE_TELEMETRY_GIT_SHA");

/// File name of the persisted node identity, relative to the node's data directory.
pub const TELEMETRY_ID_FILE_NAME: &str = "telemetry-id";

/// How often a node sends a report.
///
/// This is a steady-state floor rather than a compromise: head lag is only useful at a
/// resolution finer than the incidents it should catch.
pub const DEFAULT_REPORT_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// How often a node samples head lag between reports.
pub const DEFAULT_SAMPLE_INTERVAL: Duration = Duration::from_secs(60);

/// How long a single delivery attempt may take.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How many reports may sit in the delivery queue before new ones are dropped.
///
/// Small on purpose. If delivery is wedged, the useful thing to keep is the newest report, and
/// a deep queue only delays discovering that.
pub const DEFAULT_QUEUE_CAPACITY: usize = 4;

/// Upper bound on lag samples carried by a single report.
///
/// Guards against an operator configuring a long report interval against a short sample
/// interval and growing the payload without bound.
pub const MAX_LATENCY_SAMPLES: usize = 64;

/// Configuration for the telemetry client.
///
/// Reporting is inert unless [`TelemetryConfig::is_active`] holds, which requires both
/// `enabled` and an `endpoint`. Opting out is `enabled = false`; having no endpoint configured
/// is the separate, and currently normal, case of a build that has nowhere to report to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryConfig {
    /// Whether the operator has left telemetry on. Opt-out, so this defaults to `true`.
    pub enabled: bool,
    /// Where to POST reports. No default; without one the client sends nothing.
    pub endpoint: Option<Url>,
    /// Override used to tag our own nodes so they can be excluded from fleet numbers.
    pub instance_id: Option<String>,
    /// Where the persisted node identity lives, or `None` when nowhere could be resolved.
    ///
    /// `None` does not mean "use a default". It is the resolved answer that this node has no
    /// durable place to keep an identity: neither `--telemetry.id-path` nor `$HOME` is set. A
    /// node in that state reports nothing, because an identity minted under the working
    /// directory would not survive a restart and one operator would show up as a new node every
    /// time.
    pub id_path: Option<PathBuf>,
    /// Directory whose filesystem the disk fields describe.
    ///
    /// Separate from `id_path` on purpose. The identity is a few bytes of state the node keeps
    /// wherever it is convenient, while the disk fields are only worth collecting for the volume
    /// holding chain data, the one whose filling up stops the node. Deriving one from the other
    /// measures whichever volume happens to hold `$HOME`, which on a real deployment is the OS
    /// root and not the data disk.
    pub data_dir: Option<PathBuf>,
    /// How often to send a report.
    pub report_interval: Duration,
    /// How often to sample head lag between reports.
    pub sample_interval: Duration,
    /// How long a single delivery attempt may take.
    pub request_timeout: Duration,
    /// How many reports may sit in the delivery queue.
    pub queue_capacity: usize,
    /// Backoff applied to failed delivery attempts.
    pub retry: RetryConfig,
}

impl TelemetryConfig {
    /// Builds a config that reports to `endpoint`, using the defaults for everything else.
    pub fn new(id_path: impl Into<Option<PathBuf>>, endpoint: Option<Url>) -> Self {
        Self { endpoint, ..Self::disabled(id_path) }
    }

    /// Builds a config that sends nothing, for callers that need a value before an operator's
    /// choice is known.
    ///
    /// Takes `impl Into<Option<PathBuf>>` so a caller holding a path it knows it has can pass it
    /// bare. The field stays an `Option`, so nothing downstream can skip the question of what to
    /// do when there is no path.
    pub fn disabled(id_path: impl Into<Option<PathBuf>>) -> Self {
        Self {
            enabled: true,
            endpoint: None,
            instance_id: None,
            id_path: id_path.into(),
            data_dir: None,
            report_interval: DEFAULT_REPORT_INTERVAL,
            sample_interval: DEFAULT_SAMPLE_INTERVAL,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            retry: RetryConfig::default(),
        }
    }

    /// Returns whether this config will actually send anything.
    ///
    /// `const` buys nothing here — every caller asks at runtime — and it forfeits the freedom to
    /// add a check that cannot be const, such as validating the endpoint. It stays only because
    /// the workspace enables `clippy::missing_const_for_fn`, so dropping it means an `allow`
    /// attribute, and a suppressed lint is worse than a keyword. This crate is workspace-internal,
    /// so the day a non-const check is needed, removing `const` is a local edit.
    pub const fn is_active(&self) -> bool {
        self.enabled && self.endpoint.is_some()
    }

    /// Returns the default identity path for a chain, `$HOME/.base/<l2_chain_id>/telemetry-id`,
    /// or `None` when `$HOME` is unset.
    ///
    /// This mirrors where the consensus node already puts its checkpoint database. The
    /// execution node has a reth data directory and should pass that instead.
    pub fn default_id_path(l2_chain_id: u64) -> Option<PathBuf> {
        Self::id_path_under(std::env::var_os("HOME").map(PathBuf::from).as_deref(), l2_chain_id)
    }

    /// Returns the identity path for a chain under `home`, or `None` when there is no usable home
    /// directory.
    ///
    /// `None` rather than a working-directory-relative path. `$HOME` is routinely unset for a
    /// container started by systemd, and `./.base/<chain>/telemetry-id` resolves against a
    /// working directory such a deployment does not persist: the node would mint a fresh
    /// identity on every restart and one operator would appear as an unbounded number of nodes.
    /// The caller turns `None` into a warning naming `--telemetry.id-path` and a run with
    /// telemetry off, because telemetry must never be the reason a node fails to start.
    ///
    /// Split from the environment lookup so the no-home case is testable without mutating the
    /// process environment out from under every other test in the binary.
    pub fn id_path_under(home: Option<&Path>, l2_chain_id: u64) -> Option<PathBuf> {
        home.filter(|home| !home.as_os_str().is_empty()).map(|home| {
            home.join(".base").join(l2_chain_id.to_string()).join(TELEMETRY_ID_FILE_NAME)
        })
    }

    /// Returns how many samples a report will carry, given the configured intervals.
    pub fn samples_per_report(&self) -> usize {
        if self.sample_interval.is_zero() {
            return 1;
        }
        let per_report = self.report_interval.as_secs() / self.sample_interval.as_secs().max(1);
        (per_report as usize).clamp(1, MAX_LATENCY_SAMPLES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> TelemetryConfig {
        TelemetryConfig::disabled(PathBuf::from("/tmp/telemetry-id"))
    }

    #[test]
    fn test_config_without_an_endpoint_is_inert() {
        let config = config();
        assert!(config.enabled, "telemetry is opt-out, so it starts enabled");
        assert!(!config.is_active(), "an enabled config with no endpoint must still send nothing");
    }

    #[test]
    fn test_opting_out_overrides_a_configured_endpoint() {
        let mut config = config();
        config.endpoint = Some(Url::parse("http://127.0.0.1:8080/v1/ingest").expect("valid url"));
        assert!(config.is_active());

        config.enabled = false;
        assert!(!config.is_active(), "--telemetry.enabled=false must win over an endpoint");
    }

    #[test]
    fn test_sample_count_is_bounded() {
        let mut config = config();
        config.report_interval = Duration::from_secs(60 * 60 * 24);
        config.sample_interval = Duration::from_secs(1);
        assert_eq!(config.samples_per_report(), MAX_LATENCY_SAMPLES);
    }

    #[test]
    fn test_sample_count_is_at_least_one() {
        let mut config = config();
        config.report_interval = Duration::from_secs(1);
        config.sample_interval = Duration::from_secs(60);
        assert_eq!(config.samples_per_report(), 1);

        config.sample_interval = Duration::ZERO;
        assert_eq!(
            config.samples_per_report(),
            1,
            "a zero sample interval must not divide by zero"
        );
    }

    #[test]
    fn test_measured_directory_is_not_derived_from_the_identity_path() {
        let config = config();
        assert_eq!(
            config.data_dir, None,
            "the identity path must not stand in for the data volume: it defaults under $HOME, \
             so the disk fields would describe the OS root rather than the chain data disk"
        );
    }

    #[test]
    fn test_id_path_is_chain_scoped_under_home() {
        let path = TelemetryConfig::id_path_under(Some(Path::new("/home/base")), 8453);
        assert_eq!(path, Some(PathBuf::from("/home/base/.base/8453/telemetry-id")));
    }

    #[test]
    fn test_id_path_without_a_home_is_none_rather_than_relative() {
        for home in [None, Some(Path::new(""))] {
            assert_eq!(
                TelemetryConfig::id_path_under(home, 8453),
                None,
                "an unset $HOME must not resolve to a working-directory path: a container that \
                 does not persist its working directory would re-mint an identity every restart"
            );
        }
    }
}
