//! A geth-style genesis file that starts a real node on a synthetic chain's pre-state.

use alloy_consensus::Header;
use alloy_primitives::{B256, KECCAK256_EMPTY};
use base_common_chains::ChainConfig;
use serde_json::{Map, Value, json};

use crate::WorldState;

/// The genesis file every node starts from.
///
/// The format is the one base/base takes as `--chain` (see `crates/common/rpc-types/src/genesis.rs`):
/// Base mainnet's chain ID, fork schedule, fee parameters, Beryl activation admin and legacy block
/// time, with the genesis block placed at the synthetic chain's parent block. The block number and
/// timestamp match mainnet's schedule, so the Denim subsecond schedule (anchored at the genesis
/// block) is mainnet's too.
#[derive(Debug)]
pub struct NodeGenesis;

impl NodeGenesis {
    /// The genesis JSON whose state is `state` and whose header inputs come from `parent`.
    ///
    /// The parent hash is zero, as for any genesis block: adapters then answer `BLOCKHASH` of
    /// earlier blocks with zero, as the nodes do.
    ///
    /// Fields a genesis file cannot set (gas used, roots of the empty body, the parent beacon
    /// block root, the requests hash) take each client's genesis defaults, so the genesis hash is
    /// read back from the nodes rather than taken from `parent`.
    ///
    /// With `zero_number` the genesis block is block zero instead (timestamp unchanged): the
    /// chain rebased onto it numbers its blocks from one (see [`crate::ChainGenerator::rebase`]),
    /// for nodes that cannot reopen a datadir whose genesis is above block zero.
    pub fn json(state: &WorldState, parent: &Header, zero_number: bool) -> Value {
        let c = ChainConfig::mainnet();
        let mut alloc = Map::new();
        for (address, account) in &state.accounts {
            let mut entry = Map::new();
            entry.insert("balance".into(), json!(format!("{:#x}", account.balance)));
            if account.nonce != 0 {
                entry.insert("nonce".into(), json!(format!("{:#x}", account.nonce)));
            }
            if account.code_hash != KECCAK256_EMPTY {
                let code = state
                    .codes
                    .get(&account.code_hash)
                    .unwrap_or_else(|| panic!("code of {address} is in the library"));
                entry.insert("code".into(), json!(code));
            }
            if !account.storage.is_empty() {
                let storage: Map<String, Value> = account
                    .storage
                    .iter()
                    .map(|(slot, value)| (slot.to_string(), json!(B256::from(*value))))
                    .collect();
                entry.insert("storage".into(), Value::Object(storage));
            }
            alloc.insert(format!("{address:#x}"), Value::Object(entry));
        }
        json!({
            "config": {
                "chainId": c.chain_id,
                "homesteadBlock": 0, "eip150Block": 0, "eip155Block": 0, "eip158Block": 0,
                "byzantiumBlock": 0, "constantinopleBlock": 0, "petersburgBlock": 0,
                "istanbulBlock": 0, "muirGlacierBlock": 0, "berlinBlock": 0, "londonBlock": 0,
                "arrowGlacierBlock": 0, "grayGlacierBlock": 0, "mergeNetsplitBlock": 0,
                "bedrockBlock": c.bedrock_block,
                "regolithTime": c.regolith_timestamp,
                "canyonTime": c.canyon_timestamp,
                "deltaTime": c.delta_timestamp,
                "ecotoneTime": c.ecotone_timestamp,
                "fjordTime": c.fjord_timestamp,
                "graniteTime": c.granite_timestamp,
                "holoceneTime": c.holocene_timestamp,
                "isthmusTime": c.isthmus_timestamp,
                "jovianTime": c.jovian_timestamp,
                "base": {
                    "azul": c.azul_timestamp,
                    "beryl": c.beryl_timestamp,
                    "cobalt": c.cobalt_timestamp,
                    "denim": c.denim_timestamp,
                },
                "activationAdminAddress": c.beryl_activation_admin_address(),
                "blockTime": c.block_time,
                "terminalTotalDifficulty": 0,
                "terminalTotalDifficultyPassed": true,
                "optimism": {
                    "eip1559Elasticity": c.eip1559_elasticity,
                    "eip1559Denominator": c.eip1559_denominator,
                    "eip1559DenominatorCanyon": c.eip1559_denominator_canyon,
                },
            },
            "nonce": "0x0",
            "number": format!("{:#x}", if zero_number { 0 } else { parent.number }),
            "timestamp": format!("{:#x}", parent.timestamp),
            "extraData": parent.extra_data,
            "gasLimit": format!("{:#x}", parent.gas_limit),
            "difficulty": "0x0",
            "mixHash": parent.mix_hash,
            "coinbase": parent.beneficiary,
            "parentHash": B256::ZERO,
            "baseFeePerGas": format!("{:#x}", parent.base_fee_per_gas.unwrap_or_default()),
            "excessBlobGas": format!("{:#x}", parent.excess_blob_gas.unwrap_or_default()),
            "blobGasUsed": "0x0",
            "gasUsed": "0x0",
            "alloc": alloc,
        })
    }
}
