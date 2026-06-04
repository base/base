//! L1 execution layer genesis configuration.

use alloy_primitives::{Address, U256, address};
use serde_json::{Map, Value, json};

use super::accounts;

const PREFUND_CONTRACT_ADDRESS: Address = address!("4e59b44847b379578588920cA78FbF26c0B4956C");
const PREFUND_CONTRACT_CODE: &str = "0x7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe03601600081602082378035828234f58015156039578182fd5b8082525050506014600cf3";

/// Generates the L1 execution layer genesis configuration as a JSON value.
pub fn l1_el_genesis(chain_id: u64, genesis_time: u64, account_balance: U256) -> Value {
    let balance_hex = format!("{account_balance:#x}");
    let genesis_time_hex = format!("{:#x}", U256::from(genesis_time));
    let alloc = Value::Object(alloc_from_accounts(&accounts::anvil_addresses(), &balance_hex));

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

/// Generates the L1 execution layer genesis configuration as a JSON string.
pub fn l1_el_genesis_json(chain_id: u64, genesis_time: u64, account_balance: U256) -> String {
    serde_json::to_string_pretty(&l1_el_genesis(chain_id, genesis_time, account_balance))
        .expect("genesis JSON serialization should succeed")
}

fn alloc_from_accounts(accounts: &[Address], balance_hex: &str) -> Map<String, Value> {
    let mut alloc = Map::new();

    for account in accounts {
        alloc.insert(account.to_string(), json!({ "balance": balance_hex }));
    }

    alloc.insert(
        PREFUND_CONTRACT_ADDRESS.to_string(),
        json!({
            "balance": "0x0",
            "code": PREFUND_CONTRACT_CODE
        }),
    );

    alloc
}
