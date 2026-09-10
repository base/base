//! Configuration and genesis generation for system test infrastructure.

mod accounts;
pub use accounts::{
    ANVIL_ACCOUNT_0, ANVIL_ACCOUNT_1, ANVIL_ACCOUNT_2, ANVIL_ACCOUNT_3, ANVIL_ACCOUNT_4,
    ANVIL_ACCOUNT_5, ANVIL_ACCOUNT_6, ANVIL_ACCOUNT_7, ANVIL_ACCOUNT_8, ANVIL_ACCOUNT_9, Account,
    BATCHER, BUILDER, CHALLENGER, DEPLOYER, PROPOSER, SEQUENCER, TEST_MNEMONIC, anvil_addresses,
};

mod l1_beacon;
/// L1 beacon chain configuration generator.
pub use l1_beacon::l1_beacon_config_yaml;

mod l1_genesis;
/// L1 execution layer genesis configuration generators.
pub use l1_genesis::{l1_el_genesis, l1_el_genesis_json};

mod l2_intent;
/// L2 intent configuration generator for op-deployer.
pub use l2_intent::l2_intent_toml;
