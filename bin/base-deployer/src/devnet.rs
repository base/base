//! Local devnet primitives reused across the `base-deployer` commands.

use alloy_primitives::{Address, B256, FixedBytes, U256, address};
use alloy_signer_local::{MnemonicBuilder, coins_bip39::English};
use serde_json::{Map, Value, json};

const PREFUND_CONTRACT_ADDRESS: Address = address!("4e59b44847b379578588920cA78FbF26c0B4956C");
const PREFUND_CONTRACT_CODE: &str = "0x7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe03601600081602082378035828234f58015156039578182fd5b8082525050506014600cf3";

/// Standard mnemonic used for local devnets.
pub(crate) const TEST_MNEMONIC: &str = "test test test test test test test test test test test junk";
/// Deterministic builder enode ID used by the existing devnet setup.
pub(crate) const BUILDER_ENODE_ID: &str = "3255458e24278e31d5940f304b16300fdff3f6efd3e2a030b5818310ac67af45e28d057e6a332d07e0c5ab09d6947fd4eed1a646edbf224e2d2fec6f49f90abc";

/// Derived devnet account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Account {
    /// Account address.
    pub(crate) address: Address,
    /// Account private key.
    pub(crate) private_key: B256,
}

/// Role-specific account aliases for the generated devnet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoleAccounts {
    /// Deployer account.
    pub(crate) deployer: Account,
    /// Sequencer account.
    pub(crate) sequencer: Account,
    /// Batcher account.
    pub(crate) batcher: Account,
    /// Proposer account.
    pub(crate) proposer: Account,
    /// Challenger account.
    pub(crate) challenger: Account,
    /// Builder account.
    pub(crate) builder: Account,
}

/// Returns the default derived devnet accounts.
pub(crate) fn derived_accounts() -> Vec<Account> {
    (0..10).map(derive_account).collect()
}

/// Returns the default role account aliases.
pub(crate) fn role_accounts() -> RoleAccounts {
    let accounts = derived_accounts();
    RoleAccounts {
        deployer: accounts[0],
        sequencer: accounts[5],
        batcher: accounts[6],
        proposer: accounts[7],
        challenger: accounts[8],
        builder: accounts[9],
    }
}

/// Returns the deterministic sequencer P2P keys used by the docker devnet.
pub(crate) fn sequencer_p2p_keys() -> [B256; 2] {
    let accounts = derived_accounts();
    [accounts[3].private_key, accounts[4].private_key]
}

/// Generates the L1 execution layer genesis JSON.
pub(crate) fn l1_el_genesis(chain_id: u64, genesis_time: u64, account_balance: U256) -> Value {
    let balance_hex = format!("{account_balance:#x}");
    let genesis_time_hex = format!("{:#x}", U256::from(genesis_time));
    let alloc = Value::Object(alloc_from_accounts(&derived_accounts(), &balance_hex));

    json!({
        "config": {
            "chainId": chain_id,
            "homesteadBlock": 0,
            "eip150Block": 0,
            "eip155Block": 0,
            "eip158Block": 0,
            "byzantiumBlock": 0,
            "constantinopleBlock": 0,
            "petersburgBlock": 0,
            "istanbulBlock": 0,
            "berlinBlock": 0,
            "londonBlock": 0,
            "arrowGlacierBlock": 0,
            "grayGlacierBlock": 0,
            "terminalTotalDifficulty": 0,
            "shanghaiTime": 0,
            "cancunTime": 0,
            "pragueTime": 0,
            "osakaTime": 0,
            "blobSchedule": {
                "cancun": { "target": 3, "max": 6, "baseFeeUpdateFraction": 3338477 },
                "prague": { "target": 6, "max": 9, "baseFeeUpdateFraction": 5007716 },
                "osaka": { "target": 9, "max": 12, "baseFeeUpdateFraction": 5007716 },
                "bpo1": { "target": 10, "max": 15, "baseFeeUpdateFraction": 5007716 },
                "bpo2": { "target": 14, "max": 21, "baseFeeUpdateFraction": 5007716 }
            },
            "bpo1Time": 0,
            "bpo2Time": 0
        },
        "nonce": "0x0",
        "timestamp": genesis_time_hex,
        "extraData": "0x",
        "gasLimit": "0x1c9c380",
        "difficulty": "0x0",
        "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "coinbase": "0x0000000000000000000000000000000000000000",
        "alloc": alloc,
        "baseFeePerGas": "0x3b9aca00",
        "blobGasUsed": "0x0",
        "excessBlobGas": "0x0"
    })
}

/// Generates the L1 beacon configuration YAML.
pub(crate) fn l1_beacon_config_yaml(chain_id: u64, genesis_time: u64, slot_duration: u64) -> String {
    format!(
        r#"# Extends the minimal preset
PRESET_BASE: minimal
CONFIG_NAME: devnet

# Terminal PoW block
TERMINAL_TOTAL_DIFFICULTY: 0
TERMINAL_BLOCK_HASH: 0x0000000000000000000000000000000000000000000000000000000000000000
TERMINAL_BLOCK_HASH_ACTIVATION_EPOCH: 18446744073709551615

# Genesis
MIN_GENESIS_ACTIVE_VALIDATOR_COUNT: 1
MIN_GENESIS_TIME: {genesis_time}
GENESIS_FORK_VERSION: 0x10000000
GENESIS_DELAY: 0

# Forking
ALTAIR_FORK_VERSION: 0x20000000
ALTAIR_FORK_EPOCH: 0
BELLATRIX_FORK_VERSION: 0x30000000
BELLATRIX_FORK_EPOCH: 0
CAPELLA_FORK_VERSION: 0x40000000
CAPELLA_FORK_EPOCH: 0
DENEB_FORK_VERSION: 0x50000000
DENEB_FORK_EPOCH: 0
ELECTRA_FORK_VERSION: 0x60000000
ELECTRA_FORK_EPOCH: 0
FULU_FORK_VERSION: 0x70000000
FULU_FORK_EPOCH: 0

# Time parameters
SECONDS_PER_SLOT: {slot_duration}
SECONDS_PER_ETH1_BLOCK: 14
MIN_VALIDATOR_WITHDRAWABILITY_DELAY: 256
SHARD_COMMITTEE_PERIOD: 64
ETH1_FOLLOW_DISTANCE: 16

# Validator cycle
INACTIVITY_SCORE_BIAS: 4
INACTIVITY_SCORE_RECOVERY_RATE: 16
EJECTION_BALANCE: 16000000000
MIN_PER_EPOCH_CHURN_LIMIT: 2
CHURN_LIMIT_QUOTIENT: 32
MAX_PER_EPOCH_ACTIVATION_CHURN_LIMIT: 4

# Fork choice
PROPOSER_SCORE_BOOST: 40
REORG_HEAD_WEIGHT_THRESHOLD: 20
REORG_PARENT_WEIGHT_THRESHOLD: 160
REORG_MAX_EPOCHS_SINCE_FINALIZATION: 2

# Deposit contract
DEPOSIT_CHAIN_ID: {chain_id}
DEPOSIT_NETWORK_ID: {chain_id}
DEPOSIT_CONTRACT_ADDRESS: 0x0000000000000000000000000000000000000000

# Networking
MAX_PAYLOAD_SIZE: 10485760
MAX_REQUEST_BLOCKS: 1024
EPOCHS_PER_SUBNET_SUBSCRIPTION: 256
MIN_EPOCHS_FOR_BLOCK_REQUESTS: 272
ATTESTATION_PROPAGATION_SLOT_RANGE: 32
MAXIMUM_GOSSIP_CLOCK_DISPARITY: 500
MESSAGE_DOMAIN_INVALID_SNAPPY: 0x00000000
MESSAGE_DOMAIN_VALID_SNAPPY: 0x01000000
SUBNETS_PER_NODE: 2
ATTESTATION_SUBNET_COUNT: 64
ATTESTATION_SUBNET_EXTRA_BITS: 0
ATTESTATION_SUBNET_PREFIX_BITS: 6

# Deneb
MAX_REQUEST_BLOCKS_DENEB: 128
MIN_EPOCHS_FOR_BLOB_SIDECARS_REQUESTS: 4096
BLOB_SIDECAR_SUBNET_COUNT: 6
MAX_BLOBS_PER_BLOCK: 6
MAX_REQUEST_BLOB_SIDECARS: 768

# Electra
MAX_BLOBS_PER_BLOCK_ELECTRA: 9
BLOB_SIDECAR_SUBNET_COUNT_ELECTRA: 9
MAX_REQUEST_BLOB_SIDECARS_ELECTRA: 1152
MAX_EFFECTIVE_BALANCE_ELECTRA: 2048000000000
MIN_ACTIVATION_BALANCE: 32000000000
MIN_SLASHING_PENALTY_QUOTIENT_ELECTRA: 4096
WHISTLEBLOWER_REWARD_QUOTIENT_ELECTRA: 4096
MAX_ATTESTER_SLASHINGS_ELECTRA: 1
MAX_ATTESTATIONS_ELECTRA: 8
MAX_PENDING_PARTIALS_PER_WITHDRAWALS_SWEEP: 8
MAX_PENDING_DEPOSITS_PER_EPOCH: 16
PENDING_DEPOSITS_LIMIT: 134217728
PENDING_PARTIAL_WITHDRAWALS_LIMIT: 134217728
PENDING_CONSOLIDATIONS_LIMIT: 262144
MIN_PER_EPOCH_CHURN_LIMIT_ELECTRA: 128000000000
MAX_PER_EPOCH_ACTIVATION_EXIT_CHURN_LIMIT: 256000000000

# Fulu (with BPO2 parameters: target 14, max 21)
MAX_BLOBS_PER_BLOCK_FULU: 21
BLOB_SIDECAR_SUBNET_COUNT_FULU: 21
MAX_REQUEST_BLOB_SIDECARS_FULU: 2688
"#,
    )
}

/// Generates the L2 intent configuration for `op-deployer`.
pub(crate) fn l2_intent_toml(l1_chain_id: u64, l2_chain_id: u64) -> String {
    let roles = role_accounts();
    let l2_chain_id_hex = format!("{l2_chain_id:#x}");
    let deployer = format!("{:#x}", roles.deployer.address);
    let sequencer = format!("{:#x}", roles.sequencer.address);
    let batcher = format!("{:#x}", roles.batcher.address);
    let proposer = format!("{:#x}", roles.proposer.address);
    let challenger = format!("{:#x}", roles.challenger.address);

    format!(
        r#"configType = "custom"
l1ChainID = {l1_chain_id}
fundDevAccounts = true
l1ContractsLocator = "embedded"
l2ContractsLocator = "embedded"

[superchainRoles]
  SuperchainProxyAdminOwner = "{deployer}"
  SuperchainGuardian = "{deployer}"
  ProtocolVersionsOwner = "{deployer}"
  Challenger = "{challenger}"

[[chains]]
  id = "{l2_chain_id_hex}"
  baseFeeVaultRecipient = "{deployer}"
  l1FeeVaultRecipient = "{deployer}"
  sequencerFeeVaultRecipient = "{deployer}"
  operatorFeeVaultRecipient = "{deployer}"
  eip1559DenominatorCanyon = 250
  eip1559Denominator = 50
  eip1559Elasticity = 6
  gasLimit = 60000000
  operatorFeeScalar = 0
  operatorFeeConstant = 0
  chainFeesRecipient = "{deployer}"
  minBaseFee = 1000000000
  daFootprintGasScalar = 0
  [chains.roles]
    l1ProxyAdminOwner = "{deployer}"
    l2ProxyAdminOwner = "{deployer}"
    systemConfigOwner = "{deployer}"
    unsafeBlockSigner = "{sequencer}"
    batcher = "{batcher}"
    proposer = "{proposer}"
    challenger = "{challenger}"
"#,
    )
}

fn derive_account(index: u32) -> Account {
    let path = format!("m/44'/60'/0'/0/{index}");
    let signer = MnemonicBuilder::<English>::default()
        .phrase(TEST_MNEMONIC)
        .derivation_path(&path)
        .expect("valid derivation path")
        .build()
        .expect("valid mnemonic signer");
    let key_bytes = FixedBytes::<32>::from_slice(signer.credential().to_bytes().as_slice());
    Account { address: signer.address(), private_key: B256::from(key_bytes) }
}

fn alloc_from_accounts(accounts: &[Account], balance_hex: &str) -> Map<String, Value> {
    let mut alloc = Map::new();

    for account in accounts {
        alloc.insert(format!("{:#x}", account.address), json!({ "balance": balance_hex }));
    }

    alloc.insert(
        format!("{:#x}", PREFUND_CONTRACT_ADDRESS),
        json!({
            "balance": "0x0",
            "code": PREFUND_CONTRACT_CODE,
        }),
    );

    alloc
}
