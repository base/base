//! Base Chain configuration.

use alloy_primitives::{Address, B256, U256, address, b256, uint};

/// Complete configuration for a Base chain
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainConfig {
    // Identity
    /// L2 chain ID.
    pub chain_id: u64,
    /// L1 chain ID.
    pub l1_chain_id: u64,

    // Block timing
    /// L2 block time in seconds.
    pub block_time: u64,
    /// Sequencer window size in blocks.
    pub seq_window_size: u64,
    /// Maximum sequencer drift in seconds.
    pub max_sequencer_drift: u64,
    /// Channel timeout in L1 blocks.
    pub channel_timeout: u64,

    // Hardfork schedule
    /// Bedrock activation block.
    pub bedrock_block: u64,
    /// Regolith activation timestamp.
    pub regolith_timestamp: u64,
    /// Canyon activation timestamp.
    pub canyon_timestamp: u64,
    /// Delta activation timestamp.
    pub delta_timestamp: u64,
    /// Ecotone activation timestamp.
    pub ecotone_timestamp: u64,
    /// Fjord activation timestamp.
    pub fjord_timestamp: u64,
    /// Granite activation timestamp.
    pub granite_timestamp: u64,
    /// Holocene activation timestamp.
    pub holocene_timestamp: u64,
    /// Pectra blob schedule activation timestamp (optional, sepolia-only).
    pub pectra_blob_schedule_timestamp: Option<u64>,
    /// Isthmus activation timestamp.
    pub isthmus_timestamp: u64,
    /// Jovian activation timestamp.
    pub jovian_timestamp: u64,
    /// Base V1 activation timestamp (optional, not yet scheduled on prod).
    pub base_v1_timestamp: Option<u64>,

    // Genesis
    /// L1 genesis block hash.
    pub genesis_l1_hash: B256,
    /// L1 genesis block number.
    pub genesis_l1_number: u64,
    /// L2 genesis block hash.
    pub genesis_l2_hash: B256,
    /// L2 genesis block number.
    pub genesis_l2_number: u64,
    /// L2 genesis timestamp.
    pub genesis_l2_time: u64,
    /// Genesis batcher address.
    pub genesis_batcher_address: Address,
    /// Genesis overhead.
    pub genesis_overhead: U256,
    /// Genesis scalar.
    pub genesis_scalar: U256,
    /// Genesis gas limit.
    pub genesis_gas_limit: u64,

    // Base fee params
    /// EIP-1559 elasticity multiplier.
    pub eip1559_elasticity: u64,
    /// EIP-1559 denominator (pre-Canyon).
    pub eip1559_denominator: u64,
    /// EIP-1559 denominator (Canyon and later).
    pub eip1559_denominator_canyon: u64,

    // Contract addresses
    /// Batch inbox address on L1.
    pub batch_inbox_address: Address,
    /// Deposit contract (`OptimismPortal`) address on L1.
    pub deposit_contract_address: Address,
    /// `SystemConfig` proxy address on L1.
    pub system_config_address: Address,
    /// Protocol versions address on L1.
    pub protocol_versions_address: Address,

    // Roles
    /// Unsafe block signer address.
    pub unsafe_block_signer: Option<Address>,

    // Gas limits
    /// Maximum gas limit for L2 blocks.
    pub max_gas_limit: u64,

    // Networking
    /// Raw bootnode strings (ENR or enode format) for consensus discovery.
    pub bootnodes: &'static [&'static str],

    // Execution genesis
    /// Embedded genesis JSON for reth alloc tables.
    pub genesis_json: &'static str,
}

impl ChainConfig {
    /// Base Mainnet chain configuration.
    pub const fn mainnet() -> &'static Self {
        &MAINNET
    }

    /// Base Sepolia chain configuration.
    pub const fn sepolia() -> &'static Self {
        &SEPOLIA
    }

    /// Base alpha (devnet-0-sepolia-dev-0) chain configuration.
    pub const fn alpha() -> &'static Self {
        &ALPHA
    }

    /// Local dev chain configuration (all forks active at genesis).
    pub const fn devnet() -> &'static Self {
        &DEVNET
    }

    /// Base Zeronet chain configuration.
    pub const fn zeronet() -> &'static Self {
        &ZERONET
    }

    /// Returns all known chain configurations, including devnet.
    pub const fn all() -> [&'static Self; 5] {
        [&MAINNET, &SEPOLIA, &ALPHA, &DEVNET, &ZERONET]
    }

    /// Looks up a chain config by L2 chain ID.
    pub const fn by_chain_id(id: u64) -> Option<&'static Self> {
        match id {
            8453 => Some(&MAINNET),
            84532 => Some(&SEPOLIA),
            11763072 => Some(&ALPHA),
            763360 => Some(&ZERONET),
            _ => None,
        }
    }
}

const MAINNET: ChainConfig = ChainConfig {
    chain_id: 8453,
    l1_chain_id: 1,

    block_time: 2,
    seq_window_size: 3600,
    max_sequencer_drift: 600,
    channel_timeout: 300,

    bedrock_block: 0,
    regolith_timestamp: 0,
    canyon_timestamp: 1_704_992_401,
    delta_timestamp: 1_708_560_000,
    ecotone_timestamp: 1_710_374_401,
    fjord_timestamp: 1_720_627_201,
    granite_timestamp: 1_726_070_401,
    holocene_timestamp: 1_736_445_601,
    pectra_blob_schedule_timestamp: None,
    isthmus_timestamp: 1_746_806_401,
    jovian_timestamp: 1_764_691_201,
    base_v1_timestamp: None,

    genesis_l1_hash: b256!("5c13d307623a926cd31415036c8b7fa14572f9dac64528e857a470511fc30771"),
    genesis_l1_number: 17_481_768,
    genesis_l2_hash: b256!("f712aa9241cc24369b143cf6dce85f0902a9731e70d66818a3a5845b296c73dd"),
    genesis_l2_number: 0,
    genesis_l2_time: 1_686_789_347,
    genesis_batcher_address: address!("5050f69a9786f081509234f1a7f4684b5e5b76c9"),
    genesis_overhead: uint!(0xbc_U256),
    genesis_scalar: uint!(0xa6fe0_U256),
    genesis_gas_limit: 30_000_000,

    eip1559_elasticity: 6,
    eip1559_denominator: 50,
    eip1559_denominator_canyon: 250,

    batch_inbox_address: address!("ff00000000000000000000000000000000008453"),
    deposit_contract_address: address!("49048044d57e1c92a77f79988d21fa8faf74e97e"),
    system_config_address: address!("73a79fab69143498ed3712e519a88a918e1f4072"),
    protocol_versions_address: address!("8062abc286f5e7d9428a0ccb9abd71e50d93b935"),

    unsafe_block_signer: Some(address!("Af6E19BE0F9cE7f8afd49a1824851023A8249e8a")),

    max_gas_limit: 105_000_000,

    bootnodes: &[
        "enr:-J24QNz9lbrKbN4iSmmjtnr7SjUMk4zB7f1krHZcTZx-JRKZd0kA2gjufUROD6T3sOWDVDnFJRvqBBo62zuF-hYCohOGAYiOoEyEgmlkgnY0gmlwhAPniryHb3BzdGFja4OFQgCJc2VjcDI1NmsxoQKNVFlCxh_B-716tTs-h1vMzZkSs1FTu_OYTNjgufplG4N0Y3CCJAaDdWRwgiQG",
        "enr:-J24QH-f1wt99sfpHy4c0QJM-NfmsIfmlLAMMcgZCUEgKG_BBYFc6FwYgaMJMQN5dsRBJApIok0jFn-9CS842lGpLmqGAYiOoDRAgmlkgnY0gmlwhLhIgb2Hb3BzdGFja4OFQgCJc2VjcDI1NmsxoQJ9FTIv8B9myn1MWaC_2lJ-sMoeCDkusCsk4BYHjjCq04N0Y3CCJAaDdWRwgiQG",
        "enr:-J24QDXyyxvQYsd0yfsN0cRr1lZ1N11zGTplMNlW4xNEc7LkPXh0NAJ9iSOVdRO95GPYAIc6xmyoCCG6_0JxdL3a0zaGAYiOoAjFgmlkgnY0gmlwhAPckbGHb3BzdGFja4OFQgCJc2VjcDI1NmsxoQJwoS7tzwxqXSyFL7g0JM-KWVbgvjfB8JA__T7yY_cYboN0Y3CCJAaDdWRwgiQG",
        "enr:-J24QHmGyBwUZXIcsGYMaUqGGSl4CFdx9Tozu-vQCn5bHIQbR7On7dZbU61vYvfrJr30t0iahSqhc64J46MnUO2JvQaGAYiOoCKKgmlkgnY0gmlwhAPnCzSHb3BzdGFja4OFQgCJc2VjcDI1NmsxoQINc4fSijfbNIiGhcgvwjsjxVFJHUstK9L1T8OTKUjgloN0Y3CCJAaDdWRwgiQG",
        "enr:-J24QG3ypT4xSu0gjb5PABCmVxZqBjVw9ca7pvsI8jl4KATYAnxBmfkaIuEqy9sKvDHKuNCsy57WwK9wTt2aQgcaDDyGAYiOoGAXgmlkgnY0gmlwhDbGmZaHb3BzdGFja4OFQgCJc2VjcDI1NmsxoQIeAK_--tcLEiu7HvoUlbV52MspE0uCocsx1f_rYvRenIN0Y3CCJAaDdWRwgiQG",
        "enode://87a32fd13bd596b2ffca97020e31aef4ddcc1bbd4b95bb633d16c1329f654f34049ed240a36b449fda5e5225d70fe40bc667f53c304b71f8e68fc9d448690b51@3.231.138.188:30301",
        "enode://ca21ea8f176adb2e229ce2d700830c844af0ea941a1d8152a9513b966fe525e809c3a6c73a2c18a12b74ed6ec4380edf91662778fe0b79f6a591236e49e176f9@184.72.129.189:30301",
        "enode://acf4507a211ba7c1e52cdf4eef62cdc3c32e7c9c47998954f7ba024026f9a6b2150cd3f0b734d9c78e507ab70d59ba61dfe5c45e1078c7ad0775fb251d7735a2@3.220.145.177:30301",
        "enode://8a5a5006159bf079d06a04e5eceab2a1ce6e0f721875b2a9c96905336219dbe14203d38f70f3754686a6324f786c2f9852d8c0dd3adac2d080f4db35efc678c5@3.231.11.52:30301",
        "enode://cdadbe835308ad3557f9a1de8db411da1a260a98f8421d62da90e71da66e55e98aaa8e90aa7ce01b408a54e4bd2253d701218081ded3dbe5efbbc7b41d7cef79@54.198.153.150:30301",
    ],

    genesis_json: include_str!("../res/genesis/base.json"),
};

const SEPOLIA: ChainConfig = ChainConfig {
    chain_id: 84532,
    l1_chain_id: 11155111,

    block_time: 2,
    seq_window_size: 3600,
    max_sequencer_drift: 600,
    channel_timeout: 300,

    bedrock_block: 0,
    regolith_timestamp: 0,
    canyon_timestamp: 1_699_981_200,
    delta_timestamp: 1_703_203_200,
    ecotone_timestamp: 1_708_534_800,
    fjord_timestamp: 1_716_998_400,
    granite_timestamp: 1_723_478_400,
    holocene_timestamp: 1_732_633_200,
    pectra_blob_schedule_timestamp: Some(1_742_486_400),
    isthmus_timestamp: 1_744_905_600,
    jovian_timestamp: 1_763_568_001,
    base_v1_timestamp: Some(1_776_708_000),

    genesis_l1_hash: b256!("cac9a83291d4dec146d6f7f69ab2304f23f5be87b1789119a0c5b1e4482444ed"),
    genesis_l1_number: 4_370_868,
    genesis_l2_hash: b256!("0dcc9e089e30b90ddfc55be9a37dd15bc551aeee999d2e2b51414c54eaf934e4"),
    genesis_l2_number: 0,
    genesis_l2_time: 1_695_768_288,
    genesis_batcher_address: address!("6cdebe940bc0f26850285caca097c11c33103e47"),
    genesis_overhead: uint!(0x834_U256),
    genesis_scalar: uint!(0xf4240_U256),
    genesis_gas_limit: 25_000_000,

    eip1559_elasticity: 10,
    eip1559_denominator: 50,
    eip1559_denominator_canyon: 250,

    batch_inbox_address: address!("ff00000000000000000000000000000000084532"),
    deposit_contract_address: address!("49f53e41452c74589e85ca1677426ba426459e85"),
    system_config_address: address!("f272670eb55e895584501d564afeb048bed26194"),
    protocol_versions_address: address!("79add5713b383daa0a138d3c4780c7a1804a8090"),

    unsafe_block_signer: Some(address!("b830b99c95Ea32300039624Cb567d324D4b1D83C")),

    max_gas_limit: 45_000_000,

    bootnodes: &[
        "enode://548f715f3fc388a7c917ba644a2f16270f1ede48a5d88a4d14ea287cc916068363f3092e39936f1a3e7885198bef0e5af951f1d7b1041ce8ba4010917777e71f@18.210.176.114:30301",
        "enode://6f10052847a966a725c9f4adf6716f9141155b99a0fb487fea3f51498f4c2a2cb8d534e680ee678f9447db85b93ff7c74562762c3714783a7233ac448603b25f@107.21.251.55:30301",
        "enr:-J64QFa3qMsONLGphfjEkeYyF6Jkil_jCuJmm7_a42ckZeUQGLVzrzstZNb1dgBp1GGx9bzImq5VxJLP-BaptZThGiWGAYrTytOvgmlkgnY0gmlwhGsV-zeHb3BzdGFja4S0lAUAiXNlY3AyNTZrMaEDahfSECTIS_cXyZ8IyNf4leANlZnrsMEWTkEYxf4GMCmDdGNwgiQGg3VkcIIkBg",
        "enr:-J64QBwRIWAco7lv6jImSOjPU_W266lHXzpAS5YOh7WmgTyBZkgLgOwo_mxKJq3wz2XRbsoBItbv1dCyjIoNq67mFguGAYrTxM42gmlkgnY0gmlwhBLSsHKHb3BzdGFja4S0lAUAiXNlY3AyNTZrMaEDmoWSi8hcsRpQf2eJsNUx-sqv6fH4btmo2HsAzZFAKnKDdGNwgiQGg3VkcIIkBg",
    ],

    genesis_json: include_str!("../res/genesis/sepolia_base.json"),
};

const ALPHA: ChainConfig = ChainConfig {
    chain_id: 11763072,
    l1_chain_id: 11155111,

    block_time: 2,
    seq_window_size: 3600,
    max_sequencer_drift: 600,
    channel_timeout: 300,

    bedrock_block: 0,
    regolith_timestamp: 0,
    canyon_timestamp: 1_698_436_800,
    delta_timestamp: 1_706_555_000,
    ecotone_timestamp: 1_706_634_000,
    fjord_timestamp: 1_715_961_600,
    granite_timestamp: 1_723_046_400,
    holocene_timestamp: 1_731_682_800,
    pectra_blob_schedule_timestamp: Some(1_742_486_400),
    isthmus_timestamp: 1_744_300_800,
    jovian_timestamp: 1_762_185_600,
    base_v1_timestamp: Some(1_774_890_000),

    genesis_l1_hash: b256!("86252c512dc5bd7201d0532b31d50696ba84344a7cda545e04a98073a8e13d87"),
    genesis_l1_number: 4_344_216,
    genesis_l2_hash: b256!("1ab91449a7c65b8cd6c06f13e2e7ea2d10b6f9cbf5def79f362f2e7e501d2928"),
    genesis_l2_number: 0,
    genesis_l2_time: 1_695_433_056,
    genesis_batcher_address: address!("212dd524932bc43478688f91045f2682913ad8ee"),
    genesis_overhead: uint!(0x834_U256),
    genesis_scalar: uint!(0xf4240_U256),
    genesis_gas_limit: 25_000_000,

    eip1559_elasticity: 6,
    eip1559_denominator: 50,
    eip1559_denominator_canyon: 250,

    batch_inbox_address: address!("ff00000000000000000000000000000011763072"),
    deposit_contract_address: address!("579c82a835b884336b632eebecc78fa08d3291ec"),
    system_config_address: address!("7f67dc4959cb3e532b10a99f41bdd906c46fdfde"),
    protocol_versions_address: address!("252cbe9517f731c618961d890d534183822dcc8d"),

    unsafe_block_signer: None,

    max_gas_limit: 25_000_000,

    bootnodes: &[],

    genesis_json: include_str!("../res/genesis/devnet_0_sepolia_dev_0_base.json"),
};

const DEVNET: ChainConfig = ChainConfig {
    chain_id: 1337,
    l1_chain_id: 900,

    block_time: 2,
    seq_window_size: 3600,
    max_sequencer_drift: 600,
    channel_timeout: 300,

    bedrock_block: 0,
    regolith_timestamp: 0,
    canyon_timestamp: 0,
    delta_timestamp: 0,
    ecotone_timestamp: 0,
    fjord_timestamp: 0,
    granite_timestamp: 0,
    holocene_timestamp: 0,
    pectra_blob_schedule_timestamp: None,
    isthmus_timestamp: 0,
    jovian_timestamp: 0,
    base_v1_timestamp: Some(0),

    genesis_l1_hash: B256::ZERO,
    genesis_l1_number: 0,
    genesis_l2_hash: B256::ZERO,
    genesis_l2_number: 0,
    genesis_l2_time: 0,
    genesis_batcher_address: Address::ZERO,
    genesis_overhead: U256::ZERO,
    genesis_scalar: U256::ZERO,
    genesis_gas_limit: 30_000_000,

    eip1559_elasticity: 6,
    eip1559_denominator: 50,
    eip1559_denominator_canyon: 250,

    batch_inbox_address: Address::ZERO,
    deposit_contract_address: Address::ZERO,
    system_config_address: Address::ZERO,
    protocol_versions_address: Address::ZERO,

    unsafe_block_signer: None,

    max_gas_limit: 30_000_000,

    bootnodes: &[],

    genesis_json: include_str!("../res/genesis/dev.json"),
};

const ZERONET: ChainConfig = ChainConfig {
    chain_id: 763360,
    l1_chain_id: 560048,

    block_time: 2,
    seq_window_size: 3600,
    max_sequencer_drift: 600,
    channel_timeout: 300,

    bedrock_block: 0,
    regolith_timestamp: 0,
    canyon_timestamp: 0,
    delta_timestamp: 0,
    ecotone_timestamp: 0,
    fjord_timestamp: 0,
    granite_timestamp: 0,
    holocene_timestamp: 0,
    pectra_blob_schedule_timestamp: None,
    isthmus_timestamp: 0,
    jovian_timestamp: 0,
    base_v1_timestamp: Some(1_775_152_800),

    genesis_l1_hash: b256!("b7d4b69971ff31d5179be5e1b83f5a4f438f4cd1db886a6630623b7047f32cfd"),
    genesis_l1_number: 2_450_277,
    genesis_l2_hash: b256!("1842d6ef4c40e2a4794458e167f6d327269df919b626979111c37ad3a96047bf"),
    genesis_l2_number: 0,
    genesis_l2_time: 1_773_959_340,
    genesis_batcher_address: address!("4c810fec547f6c143db51953af51a1de79bead21"),
    genesis_overhead: U256::ZERO,
    genesis_scalar: uint!(0x010000000000000000000000000000000000000000000000000c3c9d00000558_U256),
    genesis_gas_limit: 25_000_000,

    eip1559_elasticity: 6,
    eip1559_denominator: 50,
    eip1559_denominator_canyon: 250,

    batch_inbox_address: address!("00975f9c430b216f84ec52374d7f5eb8eec3139a"),
    deposit_contract_address: address!("7b9fb81a8e041814903c9385b22d88ac303df699"),
    system_config_address: address!("cc7c76564bea74a963a0bd75e0bc9bce3ff0ea80"),
    protocol_versions_address: address!("646c8604cf62b23e0cf094f2e790c6c75547ff85"),

    unsafe_block_signer: Some(address!("cf17274338d3128f6C96d9af54511a17e8b38a08")),

    max_gas_limit: 25_000_000,

    bootnodes: &[
        "enr:-J-4QDS5Z5P4BoDbOlLGOcdXjcv2Nc5_PgP28lIxP4lKU6qYR-m10c8rHdcHk0DdmTvZpndoSpuK__688dmX-tlOsNKGAZ22NI20gmlkgnY0gmlwhCzGBHaHb3BzdGFja4WA-80FAIlzZWNwMjU2azGhA4Qs8_ZWeMdUNldNdjnAxd018gjWofqKoW4_pr0qzvTtg3RjcIIkBoN1ZHCCJAY",
        "enode://cd4528698249ad8b36fa7b1cad75aa5683ad355e6f0776629eaff1d83cfbb575062330d711efefbfa0d531c86969c2daf9a88fb28cddbbad216f46ac367981eb@44.198.4.118:30301",
        "enr:-J-4QKgMF6zAv7u_75LTXLJKgLtEn4HcI8gaqsDAl78nfw7VQE-EN6dUZCZW4_CI42MAOWUCinrV8rP5hbBu3aje-u-GAZ22LUBogmlkgnY0gmlwhDQCC1-Hb3BzdGFja4WA-80FAIlzZWNwMjU2azGhArwjzoKlEKQiEXtuZ0qT23Wy_3IeEXbAJo7VKDO2Yovig3RjcIIkBoN1ZHCCJAY",
        "enode://ea188fb5482ff8eb372956d674ecb6d09cbd42e6874121957a47b2ad252f54953c49866d2dcabcfc272fcc63e163a67b097fe4354283e56ddf077fc017b2a127@52.2.11.95:30301",
    ],

    genesis_json: include_str!("../res/genesis/zeronet_base.json"),
};
