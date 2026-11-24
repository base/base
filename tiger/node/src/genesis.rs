//! The chain a node runs: Base mainnet, Base Sepolia, or a geth-style genesis file with Base's
//! extensions (the format base-reth takes as `--chain`, see
//! `crates/common/rpc-types/src/genesis.rs` and `crates/execution/chainspec/src/spec.rs`).

use std::{collections::BTreeMap, sync::Arc};

use alloy_consensus::{EMPTY_ROOT_HASH, Header, constants::EMPTY_WITHDRAWALS};
use alloy_eips::{eip1559::INITIAL_BASE_FEE, eip7685::EMPTY_REQUESTS_HASH};
use alloy_genesis::Genesis as GethGenesis;
use alloy_primitives::{Address, B256, Bytes, KECCAK256_EMPTY, U256, b256, keccak256};
use base_common_consensus::Predeploys;
use base_common_genesis::{BaseFeature, BaseUpgradeConfig, FeeConfig, UpgradeConfig};
use base_tiger_evm::ChainSpec;
use serde::Deserialize;

use crate::witness::{GenesisAccount, StateUpdate, build_state, storage_root};

/// Base fee parameters of chains whose genesis has no `optimism` section (Ethereum's).
const ETHEREUM_FEE_CONFIG: FeeConfig =
    FeeConfig { eip1559_elasticity: 2, eip1559_denominator: 8, eip1559_denominator_canyon: 8 };

/// A chain's parameters and genesis block.
#[derive(Debug, Clone)]
pub struct Genesis {
    /// Execution and validation parameters.
    pub chain: ChainSpec,
    /// The genesis header.
    pub header: Header,
    /// The genesis block hash (Base mainnet and Sepolia have legacy hashes that differ from
    /// the header's own).
    pub hash: B256,
    /// The genesis state, when known. Built-in chains carry none: a node joins them from a
    /// datadir or a stateless start, never by replaying from genesis.
    pub alloc: Option<Arc<BTreeMap<Address, GenesisAccount>>>,
}

/// Base's fields in a genesis file's `config`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BaseConfig {
    regolith_time: Option<u64>,
    canyon_time: Option<u64>,
    ecotone_time: Option<u64>,
    fjord_time: Option<u64>,
    granite_time: Option<u64>,
    holocene_time: Option<u64>,
    isthmus_time: Option<u64>,
    jovian_time: Option<u64>,
    #[serde(default)]
    base: BaseUpgradeConfig,
    activation_admin_address: Option<Address>,
    block_time: Option<u64>,
    optimism: Option<FeeInfo>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FeeInfo {
    eip1559_elasticity: Option<u64>,
    eip1559_denominator: Option<u64>,
    eip1559_denominator_canyon: Option<u64>,
}

impl Genesis {
    /// Base mainnet's legacy genesis hash.
    const BASE_MAINNET_HASH: B256 =
        b256!("f712aa9241cc24369b143cf6dce85f0902a9731e70d66818a3a5845b296c73dd");
    /// Base Sepolia's legacy genesis hash.
    const BASE_SEPOLIA_HASH: B256 =
        b256!("0dcc9e089e30b90ddfc55be9a37dd15bc551aeee999d2e2b51414c54eaf934e4");

    /// The chain named by `--chain`: `base`, `base-sepolia`, or a path to a genesis file.
    pub fn load(chain: &str) -> Result<Self, String> {
        match chain {
            "base" | "base-mainnet" => Ok(Self::built_in(ChainSpec::base_mainnet(), Self::BASE_MAINNET_HASH)),
            "base-sepolia" => Ok(Self::built_in(ChainSpec::base_sepolia(), Self::BASE_SEPOLIA_HASH)),
            path => {
                let json = std::fs::read_to_string(path)
                    .map_err(|error| format!("reading genesis {path}: {error}"))?;
                Self::parse(&json)
            }
        }
    }

    fn built_in(chain: ChainSpec, hash: B256) -> Self {
        let header = Header {
            number: chain.genesis_number,
            timestamp: chain.genesis_timestamp,
            ..Header::default()
        };
        Self { chain, header, hash, alloc: None }
    }

    /// Parses a genesis file, building its chain spec and genesis header as base-reth does.
    pub fn parse(json: &str) -> Result<Self, String> {
        let genesis: GethGenesis =
            serde_json::from_str(json).map_err(|error| format!("parsing genesis: {error}"))?;
        let config: BaseConfig = genesis
            .config
            .extra_fields
            .deserialize_as()
            .map_err(|error| format!("parsing genesis config: {error}"))?;
        let fee_config = match config.optimism {
            Some(FeeInfo {
                eip1559_elasticity: Some(elasticity),
                eip1559_denominator: Some(denominator),
                eip1559_denominator_canyon,
            }) => FeeConfig {
                eip1559_elasticity: elasticity,
                eip1559_denominator: denominator,
                eip1559_denominator_canyon: eip1559_denominator_canyon.unwrap_or(denominator),
            },
            _ => ETHEREUM_FEE_CONFIG,
        };
        let upgrades = UpgradeConfig {
            regolith_time: config.regolith_time,
            canyon_time: config.canyon_time,
            delta_time: None,
            ecotone_time: config.ecotone_time,
            fjord_time: config.fjord_time,
            granite_time: config.granite_time,
            holocene_time: config.holocene_time,
            pectra_blob_schedule_time: None,
            isthmus_time: config.isthmus_time,
            jovian_time: config.jovian_time,
            base: config.base,
        };
        let chain = ChainSpec {
            chain_id: genesis.config.chain_id,
            upgrades,
            fee_config,
            genesis_number: genesis.number.unwrap_or_default(),
            genesis_timestamp: genesis.timestamp,
            block_time: config.block_time.unwrap_or_default(),
            activation_admin: config.activation_admin_address,
        };
        if chain.upgrades.base.beryl.is_some()
            && chain.activation_admin.is_none_or(|admin| admin.is_zero())
        {
            return Err(format!(
                "Beryl-enabled chain {} needs a non-zero activationAdminAddress",
                chain.chain_id
            ));
        }
        if genesis.config.london_block != Some(0) {
            return Err("only chains with London at genesis are supported".into());
        }

        let alloc: BTreeMap<Address, GenesisAccount> = genesis
            .alloc
            .iter()
            .map(|(address, account)| {
                let code = account.code.clone().unwrap_or_default();
                let storage = account
                    .storage
                    .iter()
                    .flatten()
                    .filter(|(_, value)| !value.is_zero())
                    .map(|(slot, value)| (U256::from_be_bytes(slot.0), U256::from_be_bytes(value.0)))
                    .collect();
                let code_hash = if code.is_empty() { KECCAK256_EMPTY } else { keccak256(&code) };
                let account = GenesisAccount {
                    nonce: account.nonce.unwrap_or_default(),
                    balance: account.balance,
                    code_hash,
                    code,
                    storage,
                };
                (*address, account)
            })
            .collect();
        let state = build_state(&alloc);
        let header = Self::header(&genesis, &chain, &alloc, &state);
        let hash = header.hash_slow();
        Ok(Self { chain, header, hash, alloc: Some(Arc::new(alloc)) })
    }

    /// The genesis header (`reth_chainspec::make_genesis_header` plus Base's Isthmus
    /// withdrawals root).
    fn header(
        genesis: &GethGenesis,
        chain: &ChainSpec,
        alloc: &BTreeMap<Address, GenesisAccount>,
        state: &StateUpdate,
    ) -> Header {
        let timestamp = genesis.timestamp;
        let cancun = chain.is_active(BaseFeature::BlobHeader, timestamp);
        // From Isthmus the withdrawals root is the message passer's storage root (the empty root
        // when it has no storage, the same value as before Isthmus).
        let withdrawals_root = if chain.is_active(BaseFeature::L2ToL1MessageRoot, timestamp) {
            Some(alloc.get(&Predeploys::L2_TO_L1_MESSAGE_PASSER).map_or(EMPTY_ROOT_HASH, |passer| {
                storage_root(&passer.storage)
            }))
        } else {
            chain.is_active(BaseFeature::Withdrawals, timestamp).then_some(EMPTY_WITHDRAWALS)
        };
        Header {
            number: genesis.number.unwrap_or_default(),
            parent_hash: genesis.parent_hash.unwrap_or_default(),
            gas_limit: genesis.gas_limit,
            difficulty: genesis.difficulty,
            nonce: genesis.nonce.into(),
            extra_data: genesis.extra_data.clone(),
            state_root: state.root,
            timestamp,
            mix_hash: genesis.mix_hash,
            beneficiary: genesis.coinbase,
            base_fee_per_gas: Some(genesis.base_fee_per_gas.map_or(INITIAL_BASE_FEE, |fee| fee as u64)),
            withdrawals_root,
            parent_beacon_block_root: cancun.then_some(B256::ZERO),
            blob_gas_used: cancun.then(|| genesis.blob_gas_used.unwrap_or(0)),
            excess_blob_gas: cancun.then(|| genesis.excess_blob_gas.unwrap_or(0)),
            requests_hash: chain
                .is_active(BaseFeature::EmptyRequestsHash, timestamp)
                .then_some(EMPTY_REQUESTS_HASH),
            ..Header::default()
        }
    }
}

/// Bytecodes of the genesis state, by hash.
pub fn genesis_codes(alloc: &BTreeMap<Address, GenesisAccount>) -> impl Iterator<Item = Bytes> + '_ {
    alloc.values().filter(|account| !account.code.is_empty()).map(|account| account.code.clone())
}
