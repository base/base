//! The live devnet: a Regolith genesis every client takes as `--chain`, and blocks built by one
//! node over the Engine API and imported by the others, so all serve the same chain.
//!
//! Regolith keeps the payloads simple (Engine API V2, no withdrawals, no beacon root): each block
//! holds the L1 info deposit (Bedrock calldata), and `--big` blocks also hold a user deposit with
//! 128 `KiB` of calldata so body and receipt responses can reach the 2 `MiB` soft limit.

use std::{fs, path::Path, time::Duration};

use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, address, hex, keccak256};
use base_common_consensus::{
    L1BlockInfoBedrock, L1InfoDepositSource, TxDeposit, UserDepositSource,
};
use serde_json::{Value, json};

use super::rpc::Rpc;
use crate::el::rng::Rng;

/// The devnet chain id.
pub(crate) const CHAIN_ID: u64 = 845_300;
/// The genesis timestamp.
pub(crate) const GENESIS_TIMESTAMP: u64 = 1_700_000_000;
/// Seconds between blocks.
const BLOCK_TIME: u64 = 2;
/// Calldata of a big block's user deposit.
const BIG_CALLDATA: usize = 128 * 1024;

/// The genesis file: every pre-merge fork, Bedrock and Regolith at genesis, nothing later.
pub(crate) fn genesis_json() -> Value {
    json!({
        "config": {
            "chainId": CHAIN_ID,
            "homesteadBlock": 0, "eip150Block": 0, "eip155Block": 0, "eip158Block": 0,
            "byzantiumBlock": 0, "constantinopleBlock": 0, "petersburgBlock": 0,
            "istanbulBlock": 0, "muirGlacierBlock": 0, "berlinBlock": 0, "londonBlock": 0,
            "arrowGlacierBlock": 0, "grayGlacierBlock": 0, "mergeNetsplitBlock": 0,
            "bedrockBlock": 0,
            "regolithTime": 0,
            "terminalTotalDifficulty": 0,
            "terminalTotalDifficultyPassed": true,
            "optimism": { "eip1559Elasticity": 6, "eip1559Denominator": 50 }
        },
        "nonce": "0x0",
        "timestamp": format!("{GENESIS_TIMESTAMP:#x}"),
        "extraData": "0x00",
        "gasLimit": "0x1c9c380",
        "difficulty": "0x0",
        "mixHash": format!("{:#x}", B256::ZERO),
        "coinbase": format!("{:#x}", Address::ZERO),
        "number": "0x0",
        "alloc": {
            "0x00000000000000000000000000000000000f022e": { "balance": "0xd3c21bcecceda1000000" }
        }
    })
}

/// Writes `genesis.json`, `jwt.hex` and one node key per client into `dir`.
pub(crate) fn setup(dir: &Path, seed: u64) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut rng = Rng::new(seed);
    let write = |name: &str, text: String| {
        fs::write(dir.join(name), text).map_err(|e| format!("{name}: {e}"))
    };
    write("genesis.json", serde_json::to_string_pretty(&genesis_json()).unwrap())?;
    write("jwt.hex", hex::encode(rng.array::<32>()))?;
    for client in ["tiger", "reth", "nethermind"] {
        let key = crate::el::wire::secret(&mut rng);
        let id = crate::el::wire::node_id(&key.public_key(secp256k1::SECP256K1));
        write(&format!("{client}.key"), hex::encode(key.secret_bytes()))?;
        write(&format!("{client}.id"), hex::encode(id))?;
    }
    println!("wrote genesis.json, jwt.hex, <client>.key and <client>.id to {}", dir.display());
    Ok(())
}

/// Reads `jwt.hex`.
pub(crate) fn jwt(dir: &Path) -> Result<[u8; 32], String> {
    let text = fs::read_to_string(dir.join("jwt.hex")).map_err(|e| format!("jwt.hex: {e}"))?;
    hex::decode(text.trim())
        .map_err(|e| e.to_string())?
        .try_into()
        .map_err(|_| "jwt.hex: 32 bytes".into())
}

fn quantity(value: &Value) -> u64 {
    value
        .as_str()
        .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .unwrap_or(0)
}

/// The L1 info deposit of block `number`.
fn l1_info_deposit(number: u64, timestamp: u64) -> Bytes {
    let l1_hash = keccak256(number.to_be_bytes());
    let info = L1BlockInfoBedrock::new(
        number,
        timestamp - 12,
        1_000_000_000,
        l1_hash,
        0,
        address!("8f23bb38f531600e5d8fddaaec41f13fab46e98c"),
        U256::from(188),
        U256::from(684_000),
    );
    let tx = TxDeposit {
        source_hash: L1InfoDepositSource::new(l1_hash, 0).source_hash(),
        from: address!("deaddeaddeaddeaddeaddeaddeaddeaddead0001"),
        to: TxKind::Call(address!("4200000000000000000000000000000000000015")),
        mint: 0,
        value: U256::ZERO,
        gas_limit: 1_000_000,
        is_system_transaction: false,
        input: info.encode_calldata(),
    };
    tx.encoded_2718().into()
}

/// A user deposit carrying [`BIG_CALLDATA`] bytes, to grow block bodies.
fn big_deposit(number: u64) -> Bytes {
    let mut rng = Rng::new(number);
    let tx = TxDeposit {
        source_hash: UserDepositSource::new(keccak256(number.to_le_bytes()), 0).source_hash(),
        from: address!("00000000000000000000000000000000000f022e"),
        to: TxKind::Call(address!("00000000000000000000000000000000000b16b1")),
        mint: 0,
        value: U256::ZERO,
        gas_limit: 4_000_000,
        is_system_transaction: false,
        input: rng.bytes(BIG_CALLDATA).into(),
    };
    tx.encoded_2718().into()
}

/// Copies the source node's existing chain to nodes that joined later: each block is rebuilt as an
/// execution payload from `eth_getBlockByNumber` plus `debug_getRawTransaction` and sent with
/// `engine_newPayloadV2`, so every node holds byte-identical blocks.
pub(crate) fn replay(source_http: &Rpc, targets: &[(String, Rpc, Rpc)]) -> Result<(), String> {
    let genesis_hash =
        source_http.call("eth_getBlockByNumber", json!(["0x0", false]))?["hash"].clone();
    let head =
        quantity(&source_http.call("eth_getBlockByNumber", json!(["latest", false]))?["number"]);
    for number in 1..=head {
        let block =
            source_http.call("eth_getBlockByNumber", json!([format!("{number:#x}"), false]))?;
        let mut txs = Vec::new();
        for hash in block["transactions"].as_array().ok_or("block without transactions")? {
            txs.push(source_http.call("debug_getRawTransaction", json!([hash]))?);
        }
        let payload = json!({
            "parentHash": block["parentHash"],
            "feeRecipient": block["miner"],
            "stateRoot": block["stateRoot"],
            "receiptsRoot": block["receiptsRoot"],
            "logsBloom": block["logsBloom"],
            "prevRandao": block["mixHash"],
            "blockNumber": block["number"],
            "gasLimit": block["gasLimit"],
            "gasUsed": block["gasUsed"],
            "timestamp": block["timestamp"],
            "extraData": block["extraData"],
            "baseFeePerGas": block["baseFeePerGas"],
            "blockHash": block["hash"],
            "transactions": txs,
        });
        let hash = block["hash"].clone();
        for (name, node, _) in targets {
            let status = node.call("engine_newPayloadV2", json!([payload]))?;
            let state = json!({ "headBlockHash": hash, "safeBlockHash": hash, "finalizedBlockHash": genesis_hash });
            let fcu = node.call("engine_forkchoiceUpdatedV2", json!([state, null]))?;
            let fcu_status = fcu["payloadStatus"]["status"].as_str().unwrap_or("");
            if status["status"] != "VALID" || fcu_status != "VALID" {
                return Err(format!(
                    "{name} block {number}: newPayload {status}, forkchoice {fcu_status}"
                ));
            }
        }
        println!("block {number} {hash} replayed to {} nodes", targets.len());
    }
    Ok(())
}

/// Builds `blocks` blocks on `builder` (Engine API; reads on `builder_http`) and imports each
/// into every node in `all` (`(name, auth, http)`, builder included), checking that every node
/// answers `VALID`.
pub(crate) fn build(
    builder: &Rpc,
    builder_http: &Rpc,
    all: &[(String, Rpc, Rpc)],
    blocks: u64,
    big: u64,
) -> Result<(), String> {
    let genesis = builder_http.call("eth_getBlockByNumber", json!(["0x0", false]))?;
    let genesis_hash = genesis["hash"].clone();
    for (name, _, http) in all {
        let theirs = http.call("eth_getBlockByNumber", json!(["0x0", false]))?;
        if theirs["hash"] != genesis_hash {
            return Err(format!(
                "{name} has genesis {} but the builder {}",
                theirs["hash"], genesis_hash
            ));
        }
    }
    for i in 0..blocks {
        let head = builder_http.call("eth_getBlockByNumber", json!(["latest", false]))?;
        let number = quantity(&head["number"]) + 1;
        let timestamp = quantity(&head["timestamp"]) + BLOCK_TIME;
        let mut txs = vec![l1_info_deposit(number, timestamp)];
        if i < big {
            txs.push(big_deposit(number));
        }
        let attributes = json!({
            "timestamp": format!("{timestamp:#x}"),
            "prevRandao": keccak256(number.to_be_bytes()),
            "suggestedFeeRecipient": "0x4200000000000000000000000000000000000011",
            "transactions": txs,
            "noTxPool": true,
            "gasLimit": "0x1c9c380",
        });
        let state = json!({ "headBlockHash": head["hash"], "safeBlockHash": head["hash"], "finalizedBlockHash": genesis_hash });
        let fcu = builder.call("engine_forkchoiceUpdatedV2", json!([state, attributes]))?;
        let id = fcu["payloadId"].clone();
        if id.is_null() {
            return Err(format!("no payload id: {fcu}"));
        }
        std::thread::sleep(Duration::from_millis(200));
        let payload = builder.call("engine_getPayloadV2", json!([id]))?["executionPayload"].clone();
        let hash = payload["blockHash"].clone();
        for (name, node, _) in all {
            let status = node.call("engine_newPayloadV2", json!([payload]))?;
            let mut status = status["status"].as_str().unwrap_or("").to_string();
            if status == "SYNCING" || status == "ACCEPTED" {
                std::thread::sleep(Duration::from_millis(300));
                status = node.call("engine_newPayloadV2", json!([payload]))?["status"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
            }
            let head_state = json!({ "headBlockHash": hash, "safeBlockHash": hash, "finalizedBlockHash": genesis_hash });
            let fcu = node.call("engine_forkchoiceUpdatedV2", json!([head_state, null]))?;
            let fcu_status = fcu["payloadStatus"]["status"].as_str().unwrap_or("");
            if status != "VALID" || fcu_status != "VALID" {
                return Err(format!(
                    "{name} block {number}: newPayload {status}, forkchoice {fcu_status}"
                ));
            }
        }
        println!("block {number} {hash} imported by {} nodes", all.len());
    }
    Ok(())
}
