//! Configuration files.
use std::path::Path;

use base_execution_network_types::PeersConfig;
use base_execution_network_types::SessionsConfig;
use base_execution_network_types::TrustedPeer;
use base_execution_state_maintenance::StaticFilesConfig;
use base_execution_state_types::PruneConfig;
use base_execution_sync_pipeline::StageConfig;

const EXTENSION: &str = "toml";

/// Configuration for the reth node.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    /// Nodes to bootstrap P2P discovery with, as `enode://` URLs or `enr:` records.
    ///
    /// Takes precedence over the chain spec bootnodes, and is overridden by `--bootnodes`.
    pub bootnodes: Vec<TrustedPeer>,
    /// Configuration for each stage in the pipeline.
    pub stages: StageConfig,
    /// Configuration for pruning.
    #[serde(default)]
    pub prune: PruneConfig,
    /// Configuration for the discovery service.
    pub peers: PeersConfig,
    /// Configuration for peer sessions.
    pub sessions: SessionsConfig,
    /// Configuration for static files.
    #[serde(default)]
    pub static_files: StaticFilesConfig,
}

impl Config {
    /// Sets the pruning configuration.
    pub fn set_prune_config(&mut self, prune_config: PruneConfig) {
        self.prune = prune_config;
    }
}

impl Config {
    /// Load a [`Config`] from a specified path.
    ///
    /// A new configuration file is created with default values if none
    /// exists.
    pub fn from_path(path: impl AsRef<Path>) -> eyre::Result<Self> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(cfg_string) => toml_0_9_12_spec_1_1_0::from_str(&cfg_string)
                .map_err(|e| eyre::eyre!("Failed to parse TOML: {e}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| eyre::eyre!("Failed to create directory: {e}"))?;
                }
                let cfg = Self::default();
                let s = toml_0_9_12_spec_1_1_0::to_string_pretty(&cfg)
                    .map_err(|e| eyre::eyre!("Failed to serialize to TOML: {e}"))?;
                std::fs::write(path, s)
                    .map_err(|e| eyre::eyre!("Failed to write configuration file: {e}"))?;
                Ok(cfg)
            }
            Err(e) => Err(eyre::eyre!("Failed to load configuration: {e}")),
        }
    }

    /// Returns the [`PeersConfig`] for the node.
    ///
    /// If a peers file is provided, the basic nodes from the file are added to the configuration.
    pub fn peers_config_with_basic_nodes_from_file(
        &self,
        peers_file: Option<&Path>,
    ) -> PeersConfig {
        self.peers
            .clone()
            .with_basic_nodes_from_file(peers_file)
            .unwrap_or_else(|_| self.peers.clone())
    }

    /// Save the configuration to toml file.
    pub fn save(&self, path: &Path) -> Result<(), std::io::Error> {
        if path.extension() != Some(std::ffi::OsStr::new(EXTENSION)) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("reth config file extension must be '{EXTENSION}'"),
            ));
        }

        std::fs::write(
            path,
            toml_0_9_12_spec_1_1_0::to_string(self)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, str::FromStr, time::Duration};

    use base_execution_network_types::TrustedPeer;

    use super::{Config, EXTENSION};

    fn with_tempdir(filename: &str, proc: fn(&std::path::Path)) {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join(filename).with_extension(EXTENSION);

        proc(&config_path);

        temp_dir.close().unwrap()
    }

    /// Run a test function with a temporary config path as fixture.
    fn with_config_path(test_fn: fn(&Path)) {
        // Create a temporary directory for the config file
        let config_dir = tempfile::tempdir().expect("creating test fixture failed");
        // Create the config file path
        let config_path =
            config_dir.path().join("example-app").join("example-config").with_extension("toml");
        // Run the test function with the config path
        test_fn(&config_path);
        config_dir.close().expect("removing test fixture failed");
    }

    #[test]
    fn test_load_path_works() {
        with_config_path(|path| {
            let config = Config::from_path(path).expect("load_path failed");
            assert_eq!(config, Config::default());
        })
    }

    #[test]
    fn test_load_path_reads_existing_config() {
        with_config_path(|path| {
            let config = Config::default();

            // Create the parent directory if it doesn't exist
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("Failed to create directories");
            }

            // Write the config to the file
            std::fs::write(path, toml_0_9_12_spec_1_1_0::to_string(&config).unwrap())
                .expect("Failed to write config");

            // Load the config from the file and compare it
            let loaded = Config::from_path(path).expect("load_path failed");
            assert_eq!(config, loaded);
        })
    }

    #[test]
    pub fn test_load_config_with_removed_era_stage() {
        with_tempdir("reth", |path| {
            std::fs::write(
                path,
                "[stages.era]\nfolder = '/tmp/era'\n\n[stages.headers]\ncommit_threshold = 42\n",
            )
            .unwrap();

            let config =
                Config::from_path(path).expect("legacy ERA settings must not prevent startup");
            assert_eq!(config.stages.headers.commit_threshold, 42);

            config.save(path).unwrap();
            let saved: toml_0_9_12_spec_1_1_0::Value =
                toml_0_9_12_spec_1_1_0::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            assert!(saved["stages"].get("era").is_none());
            assert_eq!(Config::from_path(path).unwrap(), config);
        });
    }

    #[test]
    fn test_load_path_fails_on_invalid_toml() {
        with_config_path(|path| {
            let invalid_toml = "invalid toml data";

            // Create the parent directory if it doesn't exist
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("Failed to create directories");
            }

            // Write invalid TOML data to the file
            std::fs::write(path, invalid_toml).expect("Failed to write invalid TOML");

            // Attempt to load the config should fail
            let result = Config::from_path(path);
            assert!(result.is_err());
        })
    }

    #[test]
    fn test_load_path_creates_directory_if_not_exists() {
        with_config_path(|path| {
            // Ensure the directory does not exist
            let parent = path.parent().unwrap();
            assert!(!parent.exists());

            // Load the configuration, which should create the directory and a default config file
            let config = Config::from_path(path).expect("load_path failed");
            assert_eq!(config, Config::default());

            // The directory and file should now exist
            assert!(parent.exists());
            assert!(path.exists());
        });
    }

    #[test]
    fn test_store_config() {
        with_tempdir("config-store-test", |config_path| {
            let config = Config::default();
            std::fs::write(
                config_path,
                toml_0_9_12_spec_1_1_0::to_string(&config).expect("Failed to serialize config"),
            )
            .expect("Failed to write config file");
        })
    }

    #[test]
    fn test_store_config_method() {
        with_tempdir("config-store-test-method", |config_path| {
            let config = Config::default();
            config.save(config_path).expect("Failed to store config");
        })
    }

    #[test]
    fn test_load_config() {
        with_tempdir("config-load-test", |config_path| {
            let config = Config::default();

            // Write the config to a file
            std::fs::write(
                config_path,
                toml_0_9_12_spec_1_1_0::to_string(&config).expect("Failed to serialize config"),
            )
            .expect("Failed to write config file");

            // Load the config from the file
            let loaded_config = Config::from_path(config_path).unwrap();

            // Compare the loaded config with the original config
            assert_eq!(config, loaded_config);
        })
    }

    #[test]
    fn test_load_execution_stage() {
        with_tempdir("config-load-test", |config_path| {
            let mut config = Config::default();
            config.stages.execution.max_duration = Some(Duration::from_secs(10 * 60));

            // Write the config to a file
            std::fs::write(
                config_path,
                toml_0_9_12_spec_1_1_0::to_string(&config).expect("Failed to serialize config"),
            )
            .expect("Failed to write config file");

            // Load the config from the file
            let loaded_config = Config::from_path(config_path).unwrap();

            // Compare the loaded config with the original config
            assert_eq!(config, loaded_config);
        })
    }

    // ensures config deserialization is backwards compatible
    #[test]
    fn test_backwards_compatibility() {
        let alpha_0_0_8 = r"#
[stages.headers]
downloader_max_concurrent_requests = 100
downloader_min_concurrent_requests = 5
downloader_max_buffered_responses = 100
downloader_request_limit = 1000
commit_threshold = 10000

[stages.bodies]
downloader_request_limit = 200
downloader_stream_batch_size = 1000
downloader_max_buffered_blocks_size_bytes = 2147483648
downloader_min_concurrent_requests = 5
downloader_max_concurrent_requests = 100

[stages.sender_recovery]
commit_threshold = 5000000

[stages.execution]
max_blocks = 500000
max_changes = 5000000

[stages.account_hashing]
clean_threshold = 500000
commit_threshold = 100000

[stages.storage_hashing]
clean_threshold = 500000
commit_threshold = 100000

[stages.merkle]
clean_threshold = 50000

[stages.transaction_lookup]
chunk_size = 5000000

[stages.index_account_history]
commit_threshold = 100000

[stages.index_storage_history]
commit_threshold = 100000

[peers]
refill_slots_interval = '1s'
trusted_nodes = []
connect_trusted_nodes_only = false
max_backoff_count = 5
ban_duration = '12h'

[peers.connection_info]
max_outbound = 100
max_inbound = 30

[peers.reputation_weights]
bad_message = -16384
bad_block = -16384
bad_transactions = -16384
already_seen_transactions = 0
timeout = -4096
bad_protocol = -2147483648
failed_to_connect = -25600
dropped = -4096

[peers.backoff_durations]
low = '30s'
medium = '3m'
high = '15m'
max = '1h'

[sessions]
session_command_buffer = 32
session_event_buffer = 260

[sessions.limits]

[sessions.initial_internal_request_timeout]
secs = 20
nanos = 0

[sessions.protocol_breach_request_timeout]
secs = 120
nanos = 0

[prune]
block_interval = 5

[prune.parts]
sender_recovery = { distance = 16384 }
transaction_lookup = 'full'
receipts = { before = 1920000 }
account_history = { distance = 16384 }
storage_history = { distance = 16384 }
[prune.parts.receipts_log_filter]
'0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48' = { before = 17000000 }
'0xdac17f958d2ee523a2206206994597c13d831ec7' = { distance = 1000 }
#";
        let _conf: Config = toml_0_9_12_spec_1_1_0::from_str(alpha_0_0_8).unwrap();

        let alpha_0_0_11 = r"#
[prune.segments]
sender_recovery = { distance = 16384 }
transaction_lookup = 'full'
receipts = { before = 1920000 }
account_history = { distance = 16384 }
storage_history = { distance = 16384 }
[prune.segments.receipts_log_filter]
'0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48' = { before = 17000000 }
'0xdac17f958d2ee523a2206206994597c13d831ec7' = { distance = 1000 }
#";
        let _conf: Config = toml_0_9_12_spec_1_1_0::from_str(alpha_0_0_11).unwrap();

        let alpha_0_0_18 = r"#
[stages.headers]
downloader_max_concurrent_requests = 100
downloader_min_concurrent_requests = 5
downloader_max_buffered_responses = 100
downloader_request_limit = 1000
commit_threshold = 10000

[stages.total_difficulty]
commit_threshold = 100000

[stages.bodies]
downloader_request_limit = 200
downloader_stream_batch_size = 1000
downloader_max_buffered_blocks_size_bytes = 2147483648
downloader_min_concurrent_requests = 5
downloader_max_concurrent_requests = 100

[stages.sender_recovery]
commit_threshold = 5000000

[stages.execution]
max_blocks = 500000
max_changes = 5000000
max_cumulative_gas = 1500000000000
[stages.execution.max_duration]
secs = 600
nanos = 0

[stages.account_hashing]
clean_threshold = 500000
commit_threshold = 100000

[stages.storage_hashing]
clean_threshold = 500000
commit_threshold = 100000

[stages.merkle]
clean_threshold = 50000

[stages.transaction_lookup]
commit_threshold = 5000000

[stages.index_account_history]
commit_threshold = 100000

[stages.index_storage_history]
commit_threshold = 100000

[peers]
refill_slots_interval = '5s'
trusted_nodes = []
connect_trusted_nodes_only = false
max_backoff_count = 5
ban_duration = '12h'

[peers.connection_info]
max_outbound = 100
max_inbound = 30
max_concurrent_outbound_dials = 10

[peers.reputation_weights]
bad_message = -16384
bad_block = -16384
bad_transactions = -16384
already_seen_transactions = 0
timeout = -4096
bad_protocol = -2147483648
failed_to_connect = -25600
dropped = -4096
bad_announcement = -1024

[peers.backoff_durations]
low = '30s'
medium = '3m'
high = '15m'
max = '1h'

[sessions]
session_command_buffer = 32
session_event_buffer = 260

[sessions.limits]

[sessions.initial_internal_request_timeout]
secs = 20
nanos = 0

[sessions.protocol_breach_request_timeout]
secs = 120
nanos = 0
#";
        let conf: Config = toml_0_9_12_spec_1_1_0::from_str(alpha_0_0_18).unwrap();
        assert_eq!(conf.stages.execution.max_duration, Some(Duration::from_secs(10 * 60)));

        let alpha_0_0_19 = r"#
[stages.headers]
downloader_max_concurrent_requests = 100
downloader_min_concurrent_requests = 5
downloader_max_buffered_responses = 100
downloader_request_limit = 1000
commit_threshold = 10000

[stages.total_difficulty]
commit_threshold = 100000

[stages.bodies]
downloader_request_limit = 200
downloader_stream_batch_size = 1000
downloader_max_buffered_blocks_size_bytes = 2147483648
downloader_min_concurrent_requests = 5
downloader_max_concurrent_requests = 100

[stages.sender_recovery]
commit_threshold = 5000000

[stages.execution]
max_blocks = 500000
max_changes = 5000000
max_cumulative_gas = 1500000000000
max_duration = '10m'

[stages.account_hashing]
clean_threshold = 500000
commit_threshold = 100000

[stages.storage_hashing]
clean_threshold = 500000
commit_threshold = 100000

[stages.merkle]
clean_threshold = 50000

[stages.transaction_lookup]
commit_threshold = 5000000

[stages.index_account_history]
commit_threshold = 100000

[stages.index_storage_history]
commit_threshold = 100000

[peers]
refill_slots_interval = '5s'
trusted_nodes = []
connect_trusted_nodes_only = false
max_backoff_count = 5
ban_duration = '12h'

[peers.connection_info]
max_outbound = 100
max_inbound = 30
max_concurrent_outbound_dials = 10

[peers.reputation_weights]
bad_message = -16384
bad_block = -16384
bad_transactions = -16384
already_seen_transactions = 0
timeout = -4096
bad_protocol = -2147483648
failed_to_connect = -25600
dropped = -4096
bad_announcement = -1024

[peers.backoff_durations]
low = '30s'
medium = '3m'
high = '15m'
max = '1h'

[sessions]
session_command_buffer = 32
session_event_buffer = 260

[sessions.limits]

[sessions.initial_internal_request_timeout]
secs = 20
nanos = 0

[sessions.protocol_breach_request_timeout]
secs = 120
nanos = 0
#";
        let _conf: Config = toml_0_9_12_spec_1_1_0::from_str(alpha_0_0_19).unwrap();
    }

    // ensures prune config deserialization is backwards compatible
    #[test]
    fn test_backwards_compatibility_prune_full() {
        let s = r"#
[prune]
block_interval = 5

[prune.segments]
sender_recovery = { distance = 16384 }
transaction_lookup = 'full'
receipts = { distance = 16384 }
#";
        let _conf: Config = toml_0_9_12_spec_1_1_0::from_str(s).unwrap();
    }

    #[test]
    fn test_conf_trust_nodes_only() {
        let trusted_nodes_only = r"#
[peers]
trusted_nodes_only = true
#";
        let conf: Config = toml_0_9_12_spec_1_1_0::from_str(trusted_nodes_only).unwrap();
        assert!(conf.peers.trusted_nodes_only);

        let trusted_nodes_only = r"#
[peers]
connect_trusted_nodes_only = true
#";
        let conf: Config = toml_0_9_12_spec_1_1_0::from_str(trusted_nodes_only).unwrap();
        assert!(conf.peers.trusted_nodes_only);
    }

    #[test]
    fn test_can_support_dns_in_trusted_nodes() {
        let reth_toml = r#"
    [peers]
    trusted_nodes = [
        "enode://0401e494dbd0c84c5c0f72adac5985d2f2525e08b68d448958aae218f5ac8198a80d1498e0ebec2ce38b1b18d6750f6e61a56b4614c5a6c6cf0981c39aed47dc@34.159.32.127:30303",
        "enode://e9675164b5e17b9d9edf0cc2bd79e6b6f487200c74d1331c220abb5b8ee80c2eefbf18213989585e9d0960683e819542e11d4eefb5f2b4019e1e49f9fd8fff18@berav2-bootnode.staketab.org:30303"
    ]
    "#;

        let conf: Config = toml_0_9_12_spec_1_1_0::from_str(reth_toml).unwrap();
        assert_eq!(conf.peers.trusted_nodes.len(), 2);

        let expected_enodes = vec![
            "enode://0401e494dbd0c84c5c0f72adac5985d2f2525e08b68d448958aae218f5ac8198a80d1498e0ebec2ce38b1b18d6750f6e61a56b4614c5a6c6cf0981c39aed47dc@34.159.32.127:30303",
            "enode://e9675164b5e17b9d9edf0cc2bd79e6b6f487200c74d1331c220abb5b8ee80c2eefbf18213989585e9d0960683e819542e11d4eefb5f2b4019e1e49f9fd8fff18@berav2-bootnode.staketab.org:30303",
        ];

        for enode in expected_enodes {
            let node = TrustedPeer::from_str(enode).unwrap();
            assert!(conf.peers.trusted_nodes.contains(&node));
        }
    }

    #[test]
    fn test_bootnodes() {
        let reth_toml = r#"
    bootnodes = [
        "enode://0401e494dbd0c84c5c0f72adac5985d2f2525e08b68d448958aae218f5ac8198a80d1498e0ebec2ce38b1b18d6750f6e61a56b4614c5a6c6cf0981c39aed47dc@34.159.32.127:30303",
        "enode://e9675164b5e17b9d9edf0cc2bd79e6b6f487200c74d1331c220abb5b8ee80c2eefbf18213989585e9d0960683e819542e11d4eefb5f2b4019e1e49f9fd8fff18@berav2-bootnode.staketab.org:30303",
        "enr:-IS4QHCYrYZbAKWCBRlAy5zzaDZXJBGkcnh4MHcBFZntXNFrdvJjX04jRzjzCBOonrkTfj499SZuOh8R33Ls8RRcy5wBgmlkgnY0gmlwhH8AAAGJc2VjcDI1NmsxoQPKY0yuDUmstAHYpMa2_oxVtw0RW_QAdpzBQA8yWM0xOIN1ZHCCdl8"
    ]
    "#;

        let conf: Config = toml_0_9_12_spec_1_1_0::from_str(reth_toml).unwrap();
        assert_eq!(conf.bootnodes.len(), 3);
        assert_eq!(
            conf.bootnodes[1].host,
            url::Host::<String>::Domain("berav2-bootnode.staketab.org".to_string())
        );
        // the ENR omits the tcp key, so the udp port is used for the RLPx dial guess
        assert_eq!(
            conf.bootnodes[2],
            TrustedPeer::from_str("enode://ca634cae0d49acb401d8a4c6b6fe8c55b70d115bf400769cc1400f3258cd31387574077f301b421bc84df7266c44e9e6d569fc56be00812904767bf5ccd1fc7f@127.0.0.1:30303").unwrap()
        );

        // bootnodes are written back as enode URLs, including the ones read as ENRs
        let serialized = toml_0_9_12_spec_1_1_0::to_string(&conf).unwrap();
        assert_eq!(toml_0_9_12_spec_1_1_0::from_str::<Config>(&serialized).unwrap(), conf);
    }

    #[test]
    fn test_bootnodes_default_empty() {
        let conf: Config = toml_0_9_12_spec_1_1_0::from_str("").unwrap();
        assert!(conf.bootnodes.is_empty());
    }
}
