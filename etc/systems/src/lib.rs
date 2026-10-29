#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use tracing_subscriber as _;

mod utils;
pub use utils::unique_name;

mod b20;
pub use b20::{B20CreateConfig, B20PrecompileClient};

mod block_export;
pub use block_export::{
    BlockExportWriter, EXPORT_BLOCKS_FILE, EXPORT_MANIFEST_FILE, ExportedBlock, ReplayManifest,
    read_export_manifest, read_exported_blocks,
};

mod benchmark_cli;
mod benchmark_fixtures;
pub use benchmark_cli::{
    AggregateBenchmarkArgs, BenchmarkCli, BenchmarkCommand, LocalBenchmarkArgs,
    LocalBenchmarkResult, LocalBenchmarkWorkload, LocalBenchmarkWorkloadConfig,
    LocalBenchmarkWorkloadResult, LocalBenchmarkWorkloadResults, SnapshotBenchmarkArgs,
};
pub use benchmark_fixtures::{
    DOUBLE_COUNTER_PLACEHOLDER, DOUBLE_COUNTER_SENTINEL, DeployedFixtures, PendingFixtures,
};

mod benchmark_report;
pub use benchmark_report::{
    SnapshotBenchmarkReportConfig, SnapshotBenchmarkResult, SnapshotBlockMetrics,
    VisualizerBlockMetrics, VisualizerMetadata, VisualizerRun, VisualizerRunResult,
    VisualizerSequencerMetrics, VisualizerValidatorMetrics,
};

mod calibration;
pub use calibration::{CALIBRATION_FILE, Calibration, TxProfile};

mod da_cli;
pub use da_cli::{DA_RESULT_FILE, DaBenchmarkArgs, DaResult};

mod replay_cli;
pub use replay_cli::{
    LatencySummary, REPLAY_METRICS_FILE, REPLAY_RESULT_FILE, ReplayBenchmarkArgs,
    ReplayBlockTiming, ReplayMode, ReplayResult,
};

mod replay_proofs;
pub use replay_proofs::{
    CountSummary, ProofsMethodSummary, ProofsSamples, ProofsSummary, ReplayProofsProbe,
};

mod config;
pub use config::{
    ANVIL_ACCOUNT_0, ANVIL_ACCOUNT_1, ANVIL_ACCOUNT_2, ANVIL_ACCOUNT_3, ANVIL_ACCOUNT_4,
    ANVIL_ACCOUNT_5, ANVIL_ACCOUNT_6, ANVIL_ACCOUNT_7, ANVIL_ACCOUNT_8, ANVIL_ACCOUNT_9, Account,
    BATCHER, BUILDER, CHALLENGER, DEPLOYER, PROPOSER, SEQUENCER, TEST_MNEMONIC, anvil_addresses,
    l1_beacon_config_yaml, l1_el_genesis, l1_el_genesis_json, l2_intent_toml,
};

mod containers;
pub use containers::{L1_BEACON_HTTP_PORT, L1_BEACON_NAME, L1_RETH_NAME, L1_VALIDATOR_NAME};

mod deployer;
pub use deployer::{DeployerContainer, DeploymentArtifacts, RoleAddresses};

mod docker;
pub use docker::{
    cleanup_system_test_network, is_system_test_running, list_system_test_containers,
    stop_system_test_containers,
};

mod devnet_cli;
pub use devnet_cli::{DevnetCli, DevnetCommand, SharedL1Args, SnapshotArgs, SnapshotRuntime};

mod host;
pub use host::{host_address, with_host_port_if_needed};

mod images;
pub use images::{OP_DEPLOYER_IMAGE, RETH_IMAGE};

mod l1;
pub use l1::{
    L1ContainerConfig, L1Execution, L1ReorgDriver, L1ReplacementBranch, L1RpcProxy, L1Stack,
    L1StackConfig, LighthouseBeaconContainer, LighthouseValidatorContainer, RethContainer,
};

mod l2;
pub use l2::{
    BuilderEngineSharingConfig, ChainSpecSource, ConsensusTask, InProcessBatcher,
    InProcessBatcherConfig, InProcessBuilder, InProcessBuilderConfig, InProcessClient,
    InProcessClientConfig, InProcessConsensus, InProcessConsensusConfig, InProcessFollowConsensus,
    InProcessFollowConsensusConfig, InProcessNodeRuntime, InProcessStandaloneSequencer,
    InProcessStandaloneSequencerConfig, L2ClientConsensus, L2ClientConsensusMode,
    L2ContainerConfig, L2Stack, L2StackConfig, ShadowSequencer, ShadowSequencerConfig,
    ShadowSequencersConfig, SnapshotBoundary, SnapshotL2Stack, SnapshotL2StackConfig,
    TestNodeRuntime,
};

mod network;
pub use network::{ensure_network_exists, ensure_network_exists_with_name, network_name};

mod ports;
pub use ports::PortPool;

mod prover_service;
pub use prover_service::InProcessProverService;

mod prometheus_metrics;
pub use prometheus_metrics::{PrometheusBlockCollector, PrometheusSnapshot};

mod rpc;
pub use rpc::{SystemTestProviderExt, SystemTestRpcClient};

mod setup;
pub use setup::{
    BUILDER_ENODE_ID, CL_BOOTNODE_ENR_PATH, CL_BOOTNODE_P2P_KEY, EL_BOOTNODE_ENODE,
    EL_BOOTNODE_ENODE_ID, EL_BOOTNODE_P2P_KEY, L1GenesisOutput, L2DeploymentOutput, SetupContainer,
    SetupImage,
};

mod shared_l1;
pub use shared_l1::{SHARED_L1_RUNTIME_ENV, SharedL1, SharedL1Runtime};

mod smoke;
#[cfg(feature = "upgrade-signal")]
pub use smoke::RuntimeUpgradeSignalGuard;
pub use smoke::{SystemTestStack, SystemTestStackBuilder};

mod system_config;
pub use system_config::{
    DevnetBlockInterval, DevnetConfig, DevnetL1Mode, DevnetL2State, DevnetPrefund,
    DevnetSnapshotConfig, DevnetSnapshotHead, ResolvedSnapshotChain, SnapshotChainConfig,
    StableSystemTestConfig, SystemTestPorts,
};

#[cfg(feature = "upgrade-signal")]
mod upgrade_signal;
#[cfg(feature = "upgrade-signal")]
pub use upgrade_signal::{MockProtocolVersionsClient, UpgradeSignalStackOptions};

mod urls;
pub use urls::SystemTestUrls;

mod zk_host;
pub use zk_host::InProcessZkHost;
