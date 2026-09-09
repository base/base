//! Base chain parser used by CLI command tests.

use std::sync::Arc;

use crate::ChainSpecParser;
use base_cli_utils::parse_genesis;
use base_execution_chainspec::BaseChainSpec;

/// Chains supported by reth. First value should be used as the default.
pub const SUPPORTED_CHAINS: &[&str] = &["base", "base-sepolia", "base-devnet", "base-zeronet"];

/// Base chain specification parser.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct BaseTestChainSpecParser;

impl ChainSpecParser for BaseTestChainSpecParser {
    const SUPPORTED_CHAINS: &'static [&'static str] = SUPPORTED_CHAINS;

    fn parse(s: &str) -> eyre::Result<Arc<BaseChainSpec>> {
        let spec = match s {
            "base" => BaseChainSpec::mainnet(),
            "base-sepolia" => BaseChainSpec::sepolia(),
            "base-devnet" | "dev" => BaseChainSpec::devnet(),
            "base-zeronet" => BaseChainSpec::zeronet(),
            _ => BaseChainSpec::from_genesis(parse_genesis(s)?),
        };
        Ok(Arc::new(spec))
    }
}

#[cfg(test)]
mod tests {
    use alloy_hardforks::EthereumHardforks;

    use super::*;

    #[test]
    fn parse_known_chain_spec() {
        for &chain in BaseTestChainSpecParser::SUPPORTED_CHAINS {
            assert!(<BaseTestChainSpecParser as ChainSpecParser>::parse(chain).is_ok());
        }
    }

    #[test]
    fn parse_raw_chainspec_hardforks() {
        let s = r#"{
  "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
  "uncleHash": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
  "coinbase": "0x0000000000000000000000000000000000000000",
  "stateRoot": "0x76f118cb05a8bc558388df9e3b4ad66ae1f17ef656e5308cb8f600717251b509",
  "transactionsTrie": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
  "receiptTrie": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
  "bloom": "0x000...000",
  "difficulty": "0x00",
  "number": "0x00",
  "gasLimit": "0x016345785d8a0000",
  "gasUsed": "0x00",
  "timestamp": "0x01",
  "extraData": "0x00",
  "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
  "nonce": "0x0000000000000000",
  "baseFeePerGas": "0x07",
  "withdrawalsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
  "blobGasUsed": "0x00",
  "excessBlobGas": "0x00",
  "parentBeaconBlockRoot": "0x0000000000000000000000000000000000000000000000000000000000000000",
  "requestsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
  "hash": "0xc20e1a771553139cdc77e6c3d5f64a7165d972d327eee9632c9c7d0fe839ded4",
  "alloc": {},
  "config": {
    "ethash": {},
    "chainId": 1,
    "homesteadBlock": 0,
    "daoForkSupport": true,
    "eip150Block": 0,
    "eip155Block": 0,
    "eip158Block": 0,
    "byzantiumBlock": 0,
    "constantinopleBlock": 0,
    "petersburgBlock": 0,
    "istanbulBlock": 0,
    "berlinBlock": 0,
    "londonBlock": 0,
    "terminalTotalDifficulty": 0,
    "canyonTime": 0,
    "ecotoneTime": 0,
    "isthmusTime": 0,
    "base": { "azul": 0 }
  }
}"#;

        let spec = <BaseTestChainSpecParser as ChainSpecParser>::parse(s).unwrap();
        assert!(spec.is_shanghai_active_at_timestamp(0));
        assert!(spec.is_cancun_active_at_timestamp(0));
        assert!(spec.is_prague_active_at_timestamp(0));
        assert!(spec.is_osaka_active_at_timestamp(0));
    }
}
