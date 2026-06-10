#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod utils;
pub use utils::unique_name;

mod b20;
pub use b20::{B20CreateConfig, B20PrecompileClient};

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

mod host;
pub use host::{host_address, with_host_port_if_needed};

mod images;
pub use images::{OP_DEPLOYER_IMAGE, RETH_IMAGE};

mod l1;
pub use l1::{
    L1ContainerConfig, L1Stack, L1StackConfig, LighthouseBeaconContainer,
    LighthouseValidatorContainer, RethContainer,
};

mod l2;
pub use l2::{
    InProcessBatcher, InProcessBatcherConfig, InProcessBuilder, InProcessBuilderConfig,
    InProcessClient, InProcessClientConfig, InProcessConsensus, InProcessConsensusConfig,
    L2ContainerConfig, L2Stack, L2StackConfig,
};

mod network;
pub use network::{ensure_network_exists, ensure_network_exists_with_name, network_name};

mod rpc;
pub use rpc::SystemTestRpcClient;

mod setup;
pub use setup::{
    BUILDER_ENODE_ID, CL_BOOTNODE_ENR_PATH, CL_BOOTNODE_P2P_KEY, EL_BOOTNODE_ENODE,
    EL_BOOTNODE_ENODE_ID, EL_BOOTNODE_P2P_KEY, L1GenesisOutput, L2DeploymentOutput, SetupContainer,
    SetupImage,
};

mod smoke;
pub use smoke::{SystemTestStack, SystemTestStackBuilder};

mod system_config;
pub use system_config::{StableSystemTestConfig, SystemTestPorts};

mod urls;
pub use urls::SystemTestUrls;
