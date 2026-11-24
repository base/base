//! `eth_config` (EIP-7910) as base-reth answers it: the current, next and last timestamp forks
//! of the chain, each with its EIP-2124 fork id, precompiles and system contracts.

use std::collections::BTreeMap;

use alloy_primitives::{Address, U64};
use base_common_genesis::BaseUpgrade;
use base_tiger_evm::BaseEvmTypes;
use evm2::{
    evm::{BEACON_ROOTS_ADDRESS, HISTORY_STORAGE_ADDRESS},
    precompile::PrecompileProvider,
};
use serde_json::{Value, json};

use crate::{Node, RpcError};

/// Every upgrade a chain may schedule by timestamp.
const TIMESTAMP_UPGRADES: [BaseUpgrade; 16] = [
    BaseUpgrade::Regolith,
    BaseUpgrade::Canyon,
    BaseUpgrade::Delta,
    BaseUpgrade::Ecotone,
    BaseUpgrade::Fjord,
    BaseUpgrade::Granite,
    BaseUpgrade::Holocene,
    BaseUpgrade::PectraBlobSchedule,
    BaseUpgrade::Isthmus,
    BaseUpgrade::Jovian,
    BaseUpgrade::Azul,
    BaseUpgrade::Beryl,
    BaseUpgrade::Cobalt,
    BaseUpgrade::Denim,
    BaseUpgrade::Everest,
    BaseUpgrade::Zenith,
];

impl Node {
    /// Answers `eth_config`; `None` for other methods.
    pub(super) fn rpc_config_method(&self, method: &str) -> Option<Result<Value, RpcError>> {
        (method == "eth_config").then(|| self.eth_config())
    }

    fn eth_config(&self) -> Result<Value, RpcError> {
        let upgrades = &self.spec.upgrades;
        let mut forks: Vec<u64> = TIMESTAMP_UPGRADES
            .iter()
            .filter_map(|upgrade| upgrades.activation_timestamp(*upgrade))
            .collect();
        forks.sort_unstable();
        forks.dedup();
        let head_timestamp = self.chain.read().expect("chain lock").head().header.timestamp;
        let current = forks
            .iter()
            .rposition(|fork| *fork <= head_timestamp)
            .ok_or_else(|| RpcError::internal("no active timestamp fork found".into()))?;
        let next = forks.get(current + 1).copied();
        Ok(json!({
            "current": self.fork_config(&forks, forks[current]),
            "next": next.map(|fork| self.fork_config(&forks, fork)),
            "last": next.map(|_| self.fork_config(&forks, *forks.last().expect("next exists"))),
        }))
    }

    /// One fork's object: Base zeroes the blob schedule.
    fn fork_config(&self, forks: &[u64], activation: u64) -> Value {
        let spec = self.spec.spec_at(activation);
        let mut precompiles = BTreeMap::new();
        for (address, id) in BaseEvmTypes::precompiles(spec, &self.spec).precompile_ids() {
            // base-reth's native precompile id for the policy registry.
            let name = match id.name() {
                "PolicyRegistry" => "PolicyRegistryPrecompile".to_owned(),
                name => name.to_owned(),
            };
            precompiles.insert(name, address);
        }
        let mut system_contracts: BTreeMap<&str, Address> = BTreeMap::new();
        if self.spec.is_upgrade_active(BaseUpgrade::Ecotone, activation) {
            system_contracts.insert("BEACON_ROOTS_ADDRESS", BEACON_ROOTS_ADDRESS);
        }
        if self.spec.is_upgrade_active(BaseUpgrade::Isthmus, activation) {
            system_contracts.insert("HISTORY_STORAGE_ADDRESS", HISTORY_STORAGE_ADDRESS);
        }
        json!({
            "activationTime": activation,
            "blobSchedule": { "baseFeeUpdateFraction": 0, "max": 0, "target": 0 },
            "chainId": U64::from(self.spec.chain_id),
            "forkId": format!("0x{:08x}", self.fork_hash(forks, activation)),
            "precompiles": precompiles,
            "systemContracts": system_contracts,
        })
    }

    /// The EIP-2124 fork hash at `activation`: CRC32 of the genesis hash and every fork after
    /// genesis up to `activation` (all of Base's block forks are at block 0).
    fn fork_hash(&self, forks: &[u64], activation: u64) -> u32 {
        let (genesis_hash, genesis_timestamp) = {
            let chain = self.chain.read().expect("chain lock");
            (chain.genesis.hash, chain.genesis.header.timestamp)
        };
        let mut crc = Crc32::new();
        crc.update(genesis_hash.as_slice());
        for fork in forks.iter().filter(|fork| **fork > genesis_timestamp && **fork <= activation) {
            crc.update(&fork.to_be_bytes());
        }
        crc.finish()
    }
}

/// CRC-32 (IEEE 802.3, as `zlib.crc32`), computed bitwise: fork ids hash a few dozen bytes.
#[derive(Debug, Clone, Copy)]
struct Crc32(u32);

impl Crc32 {
    const POLYNOMIAL: u32 = 0xedb8_8320;

    fn new() -> Self {
        Self(!0)
    }

    fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = (self.0 & 1).wrapping_neg();
                self.0 = (self.0 >> 1) ^ (Self::POLYNOMIAL & mask);
            }
        }
    }

    fn finish(self) -> u32 {
        !self.0
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::Crc32;
    use super::super::test_utils::{node, reply};

    #[test]
    fn crc32_is_ieee() {
        let mut crc = Crc32::new();
        crc.update(b"123456789");
        assert_eq!(crc.finish(), 0xcbf4_3926);
    }

    #[test]
    fn eth_config_has_no_next_fork_once_the_last_is_active() {
        let node = node();
        let config = reply(&node, r#"{"jsonrpc":"2.0","id":1,"method":"eth_config","params":[]}"#).unwrap()["result"].clone();
        assert_eq!((config["next"].clone(), config["last"].clone()), (json!(null), json!(null)));
        assert_eq!(config["current"]["activationTime"], json!(0));
        assert_eq!(config["current"]["precompiles"]["P256VERIFY"], json!("0x0000000000000000000000000000000000000100"));
        assert!(config["current"]["systemContracts"]["HISTORY_STORAGE_ADDRESS"].is_string());
    }
}
