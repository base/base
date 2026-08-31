use alloy_primitives::{Address, U256};
use serde::{Deserialize, Deserializer, Serialize};

use super::test_config::{TxTypeConfig, WeightedTxType};
use crate::{
    utils::{BaselineError, Result},
    workload::{RealTokenAcquisition, RealTokenPairTokenSetup, RealTokenSetup},
};

/// Optional setup for real-token bidirectional swap workloads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealTokenSetupConfig {
    /// Enables the real-token setup path.
    pub enabled: bool,
    /// Explicit guard required when running against chain ID 8453.
    #[serde(default)]
    pub allow_chain_id_8453: bool,
    /// WETH contract address.
    pub weth: Address,
    /// Target WETH balance to leave each sender with after setup.
    pub weth_amount_per_sender: U256,
    /// Non-WETH token setup.
    pub pair_token: RealTokenPairTokenConfig,
    /// Router allowance amount. Defaults to the maximum `U256` when omitted.
    ///
    /// Accepts a numeric literal (decimal or `0x`-hex) or the string `max`,
    /// which maps to [`U256::MAX`] for backwards compatibility.
    #[serde(default, deserialize_with = "deserialize_approval_amount")]
    pub approval_amount: Option<U256>,
}

/// Deserializes [`RealTokenSetupConfig::approval_amount`], preserving the legacy
/// `approval_amount: "max"` sentinel (mapped to [`U256::MAX`]) alongside numeric
/// literals now that the field is typed as [`U256`].
fn deserialize_approval_amount<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<U256>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_yaml::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_yaml::Value::Null) => Ok(None),
        Some(serde_yaml::Value::String(s)) if s == "max" => Ok(Some(U256::MAX)),
        Some(other) => U256::deserialize(other).map(Some).map_err(serde::de::Error::custom),
    }
}

/// Non-WETH side of the real-token pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealTokenPairTokenConfig {
    /// Token contract address.
    pub token: Address,
    /// Target pair-token balance per sender.
    pub amount_per_sender: U256,
    /// Explicit setup route for acquiring the pair token.
    pub acquisition: RealTokenAcquisitionConfig,
}

/// Explicit setup route for acquiring pair tokens from WETH.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RealTokenAcquisitionConfig {
    /// Uniswap V3 `exactInputSingle` route.
    UniswapV3ExactInput {
        /// Router contract address.
        router: Address,
        /// Fee tier.
        #[serde(default = "default_uniswap_v3_fee")]
        fee: u32,
        /// WETH input amount per sender.
        amount_in: U256,
        /// Minimum pair-token output amount.
        #[serde(default = "default_zero_amount")]
        min_amount_out: U256,
    },
    /// Aerodrome Slipstream `exactInputSingle` route.
    AerodromeClExactInput {
        /// Router contract address.
        router: Address,
        /// Tick spacing.
        #[serde(default = "default_aerodrome_tick_spacing")]
        tick_spacing: i32,
        /// WETH input amount per sender.
        amount_in: U256,
        /// Minimum pair-token output amount.
        #[serde(default = "default_zero_amount")]
        min_amount_out: U256,
    },
}

pub(super) fn parse_real_token_setup(
    setup: Option<&RealTokenSetupConfig>,
    transactions: &[WeightedTxType],
    chain_id: u64,
) -> Result<Option<RealTokenSetup>> {
    let Some(setup) = setup else {
        return Ok(None);
    };
    if !setup.enabled {
        return Ok(None);
    }
    if chain_id == 8453 && !setup.allow_chain_id_8453 {
        return Err(BaselineError::Config(
            "real_token_setup on chain_id 8453 requires allow_chain_id_8453: true".into(),
        ));
    }

    let weth = setup.weth;
    let weth_amount_per_sender = setup.weth_amount_per_sender;
    if weth_amount_per_sender == U256::ZERO {
        return Err(BaselineError::Config(
            "real_token_setup weth_amount_per_sender must be > 0".into(),
        ));
    }

    let pair_token = setup.pair_token.token;
    if pair_token == weth {
        return Err(BaselineError::Config(
            "real_token_setup pair_token must differ from weth".into(),
        ));
    }
    let pair_amount_per_sender = setup.pair_token.amount_per_sender;
    if pair_amount_per_sender == U256::ZERO {
        return Err(BaselineError::Config(
            "real_token_setup pair_token amount_per_sender must be > 0".into(),
        ));
    }

    validate_real_token_pair_matches_swaps(transactions, weth, pair_token)?;

    let acquisition = parse_real_token_acquisition(&setup.pair_token.acquisition)?;
    let approval_amount = setup.approval_amount.unwrap_or(U256::MAX);
    if approval_amount == U256::ZERO {
        return Err(BaselineError::Config("real_token_setup approval_amount must be > 0".into()));
    }

    Ok(Some(RealTokenSetup {
        allow_chain_id_8453: setup.allow_chain_id_8453,
        weth,
        weth_amount_per_sender,
        pair_token: RealTokenPairTokenSetup {
            token: pair_token,
            amount_per_sender: pair_amount_per_sender,
            acquisition,
        },
        approval_amount,
    }))
}

fn validate_real_token_pair_matches_swaps(
    transactions: &[WeightedTxType],
    weth: Address,
    pair_token: Address,
) -> Result<()> {
    let mut saw_swap = false;
    for tx in transactions {
        let (token_in, token_out) = match &tx.tx_type {
            TxTypeConfig::UniswapV3 { token_in, token_out, .. }
            | TxTypeConfig::AerodromeCl { token_in, token_out, .. } => (*token_in, *token_out),
            TxTypeConfig::Transfer
            | TxTypeConfig::Calldata { .. }
            | TxTypeConfig::Erc20 { .. }
            | TxTypeConfig::B20
            | TxTypeConfig::Precompile { .. }
            | TxTypeConfig::Storage { .. }
            | TxTypeConfig::Osaka { .. } => continue,
        };
        saw_swap = true;
        let matches_forward = token_in == weth && token_out == pair_token;
        let matches_reverse = token_in == pair_token && token_out == weth;
        if !matches_forward && !matches_reverse {
            return Err(BaselineError::Config(format!(
                "real_token_setup only supports the configured WETH/pair token across swap txs; found {token_in} -> {token_out}"
            )));
        }
    }

    if !saw_swap {
        return Err(BaselineError::Config(
            "real_token_setup requires at least one uniswap_v3 or aerodrome_cl transaction".into(),
        ));
    }

    Ok(())
}

fn parse_real_token_acquisition(
    config: &RealTokenAcquisitionConfig,
) -> Result<RealTokenAcquisition> {
    match config {
        RealTokenAcquisitionConfig::UniswapV3ExactInput {
            router,
            fee,
            amount_in,
            min_amount_out,
        } => {
            let max_u24: u32 = (1 << 24) - 1;
            if *fee > max_u24 {
                return Err(BaselineError::Config(format!(
                    "real_token_setup acquisition fee {fee} exceeds u24 max ({max_u24})"
                )));
            }
            if *amount_in == U256::ZERO {
                return Err(BaselineError::Config(
                    "real_token_setup acquisition amount_in must be > 0".into(),
                ));
            }
            Ok(RealTokenAcquisition::UniswapV3ExactInput {
                router: *router,
                fee: *fee,
                amount_in: *amount_in,
                min_amount_out: *min_amount_out,
            })
        }
        RealTokenAcquisitionConfig::AerodromeClExactInput {
            router,
            tick_spacing,
            amount_in,
            min_amount_out,
        } => {
            if !(-8_388_608..=8_388_607).contains(tick_spacing) {
                return Err(BaselineError::Config(format!(
                    "real_token_setup acquisition tick_spacing {tick_spacing} exceeds i24 range"
                )));
            }
            if *amount_in == U256::ZERO {
                return Err(BaselineError::Config(
                    "real_token_setup acquisition amount_in must be > 0".into(),
                ));
            }
            Ok(RealTokenAcquisition::AerodromeClExactInput {
                router: *router,
                tick_spacing: *tick_spacing,
                amount_in: *amount_in,
                min_amount_out: *min_amount_out,
            })
        }
    }
}

const fn default_zero_amount() -> U256 {
    U256::ZERO
}

const fn default_uniswap_v3_fee() -> u32 {
    3000
}

const fn default_aerodrome_tick_spacing() -> i32 {
    100
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};

    use super::super::test_config::TestConfig;
    use crate::workload::RealTokenAcquisition;

    #[test]
    fn parse_real_token_setup_for_random_direction_swap_parity() {
        let yaml = r#"
transaction_submission_rpcs: http://localhost:8545
flashblocks_ws: ws://localhost:7111
chain_id: 8453
real_token_setup:
  enabled: true
  allow_chain_id_8453: true
  weth: "0x4200000000000000000000000000000000000006"
  weth_amount_per_sender: "800000000000000000"
  pair_token:
    token: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
    amount_per_sender: "1000000000"
    acquisition:
      type: uniswap_v3_exact_input
      router: "0x2626664c2603336E57B271c5C0b26F421741e481"
      fee: 500
      amount_in: "10000000000000000"
transactions:
  - weight: 50
    type: uniswap_v3
    router: "0x2626664c2603336E57B271c5C0b26F421741e481"
    token_in: "0x4200000000000000000000000000000000000006"
    token_out: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
    fee: 500
  - weight: 50
    type: aerodrome_cl
    router: "0xBE6D8f0d05cC4be24d5167a3eF062215bE6D18a5"
    token_in: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
    token_out: "0x4200000000000000000000000000000000000006"
    tick_spacing: 100
"#;
        let config = TestConfig::from_yaml(yaml).unwrap();
        let setup = config.parse_real_token_setup(8453).unwrap().unwrap();
        assert_eq!(
            setup.weth,
            "0x4200000000000000000000000000000000000006".parse::<Address>().unwrap()
        );
        assert_eq!(
            setup.pair_token.token,
            "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913".parse::<Address>().unwrap()
        );
        assert_eq!(setup.pair_token.amount_per_sender, U256::from(1_000_000_000u64));
        assert!(matches!(
            setup.pair_token.acquisition,
            RealTokenAcquisition::UniswapV3ExactInput { fee: 500, .. }
        ));
    }

    #[test]
    fn real_token_setup_approval_amount_max_sentinel() {
        let yaml = r#"
transaction_submission_rpcs: http://localhost:8545
flashblocks_ws: ws://localhost:7111
chain_id: 8453
real_token_setup:
  enabled: true
  allow_chain_id_8453: true
  weth: "0x4200000000000000000000000000000000000006"
  weth_amount_per_sender: "800000000000000000"
  approval_amount: "max"
  pair_token:
    token: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
    amount_per_sender: "1000000000"
    acquisition:
      type: uniswap_v3_exact_input
      router: "0x2626664c2603336E57B271c5C0b26F421741e481"
      fee: 500
      amount_in: "10000000000000000"
transactions:
  - weight: 100
    type: uniswap_v3
    router: "0x2626664c2603336E57B271c5C0b26F421741e481"
    token_in: "0x4200000000000000000000000000000000000006"
    token_out: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
    fee: 500
"#;
        let config = TestConfig::from_yaml(yaml).unwrap();
        let setup = config.parse_real_token_setup(8453).unwrap().unwrap();
        assert_eq!(setup.approval_amount, U256::MAX);
    }

    #[test]
    fn real_token_setup_approval_amount_numeric_literal() {
        let yaml = r#"
transaction_submission_rpcs: http://localhost:8545
flashblocks_ws: ws://localhost:7111
chain_id: 8453
real_token_setup:
  enabled: true
  allow_chain_id_8453: true
  weth: "0x4200000000000000000000000000000000000006"
  weth_amount_per_sender: "800000000000000000"
  approval_amount: "12345"
  pair_token:
    token: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
    amount_per_sender: "1000000000"
    acquisition:
      type: uniswap_v3_exact_input
      router: "0x2626664c2603336E57B271c5C0b26F421741e481"
      fee: 500
      amount_in: "10000000000000000"
transactions:
  - weight: 100
    type: uniswap_v3
    router: "0x2626664c2603336E57B271c5C0b26F421741e481"
    token_in: "0x4200000000000000000000000000000000000006"
    token_out: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
    fee: 500
"#;
        let config = TestConfig::from_yaml(yaml).unwrap();
        let setup = config.parse_real_token_setup(8453).unwrap().unwrap();
        assert_eq!(setup.approval_amount, U256::from(12345u64));
    }

    #[test]
    fn real_token_setup_requires_mainnet_guard() {
        let yaml = r#"
transaction_submission_rpcs: http://localhost:8545
flashblocks_ws: ws://localhost:7111
real_token_setup:
  enabled: true
  weth: "0x4200000000000000000000000000000000000006"
  weth_amount_per_sender: "800000000000000000"
  pair_token:
    token: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
    amount_per_sender: "1000000000"
    acquisition:
      type: aerodrome_cl_exact_input
      router: "0xBE6D8f0d05cC4be24d5167a3eF062215bE6D18a5"
      tick_spacing: 100
      amount_in: "10000000000000000"
transactions:
  - weight: 100
    type: uniswap_v3
    router: "0x2626664c2603336E57B271c5C0b26F421741e481"
    token_in: "0x4200000000000000000000000000000000000006"
    token_out: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
    fee: 500
"#;
        let config = TestConfig::from_yaml(yaml).unwrap();
        let err = config.parse_real_token_setup(8453).unwrap_err();
        assert!(err.to_string().contains("allow_chain_id_8453"));
    }

    #[test]
    fn real_token_setup_rejects_non_pair_swap_tokens() {
        let yaml = r#"
transaction_submission_rpcs: http://localhost:8545
flashblocks_ws: ws://localhost:7111
real_token_setup:
  enabled: true
  allow_chain_id_8453: true
  weth: "0x4200000000000000000000000000000000000006"
  weth_amount_per_sender: "800000000000000000"
  pair_token:
    token: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
    amount_per_sender: "1000000000"
    acquisition:
      type: uniswap_v3_exact_input
      router: "0x2626664c2603336E57B271c5C0b26F421741e481"
      fee: 500
      amount_in: "10000000000000000"
transactions:
  - weight: 100
    type: uniswap_v3
    router: "0x2626664c2603336E57B271c5C0b26F421741e481"
    token_in: "0x4200000000000000000000000000000000000006"
    token_out: "0x1111111111111111111111111111111111111111"
    fee: 500
"#;
        let config = TestConfig::from_yaml(yaml).unwrap();
        let err = config.parse_real_token_setup(8453).unwrap_err();
        assert!(err.to_string().contains("WETH/pair token"));
    }
}
