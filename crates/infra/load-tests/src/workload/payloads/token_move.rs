//! Batched ERC-20 transfers through the Disperse contract.

use std::sync::{Arc, OnceLock};

use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, U256, address};
use alloy_provider::Provider;
use alloy_rpc_types::TransactionRequest;
use alloy_sol_types::{SolCall, sol};
use async_trait::async_trait;

use super::Payload;
use crate::{
    BaselineError, Result,
    rpc::RpcResultExt,
    workload::{ChainPrepContext, SeededRng},
};

/// Disperse (disperse.app) on Base mainnet. `disperseTokenSimple` always pulls from
/// `msg.sender`, so approving it lets no one else spend a sender's tokens.
pub(crate) const DISPERSE: Address = address!("D152f549545093347A162Dce210e7293f1452150");

sol! {
    function disperseTokenSimple(address token, address[] recipients, uint256[] values) external;
}

/// Covers intrinsic gas and the Disperse call.
const TOKEN_MOVE_GAS_BASE: u64 = 50_000;

/// Covers one real-USDC `transferFrom` to a fresh recipient (~29.5k on a Base mainnet fork).
const TOKEN_MOVE_GAS_PER_TRANSFER: u64 = 35_000;

/// Token and recipient pool shared by measured and calibration generators.
#[derive(Debug, Clone)]
pub struct TokenMoveContext {
    /// Real-token setup pair token, approved to Disperse during setup.
    pub token: Address,
    /// Funded sender accounts that receive the extra transfers in each batch.
    pub recipients: Vec<Address>,
}

/// Sends `transfers_per_tx` one-unit transfers of the pair token in one Disperse call.
///
/// The first transfer goes to the runner recipient, so `fresh_recipient_ratio` applies to it;
/// the rest go to random senders.
#[derive(Debug, Clone)]
pub struct TokenMovePayload {
    /// Number of token transfers per transaction.
    pub transfers_per_tx: u32,
    /// Filled during [`Payload::prepare`].
    pub context: Arc<OnceLock<TokenMoveContext>>,
}

#[async_trait]
impl Payload for TokenMovePayload {
    fn name(&self) -> &'static str {
        "token_move"
    }

    fn uses_runner_recipient(&self) -> bool {
        true
    }

    async fn prepare(&self, ctx: &mut ChainPrepContext<'_>) -> Result<()> {
        if self.context.get().is_some() {
            return Ok(());
        }
        let setup = ctx.real_token_setup.ok_or_else(|| {
            BaselineError::Config("token_move requires enabled real_token_setup".into())
        })?;
        // A call to an address without code succeeds without moving tokens.
        if ctx.client.get_code_at(DISPERSE).await.rpc("read Disperse code")?.is_empty() {
            return Err(BaselineError::Config(format!(
                "token_move requires Disperse at {DISPERSE}"
            )));
        }
        super::real_token_lifecycle::prepare(ctx, setup).await?;
        let recipients = ctx.accounts.accounts().iter().map(|a| a.address).collect();
        self.context
            .set(TokenMoveContext { token: setup.pair_token.token, recipients })
            .map_err(|_| BaselineError::Config("token_move prepared twice concurrently".into()))
    }

    fn generate(&self, rng: &mut SeededRng, _from: Address, to: Address) -> TransactionRequest {
        let context = self.context.get().expect("token_move setup must complete before generation");
        let recipients = (0..self.transfers_per_tx)
            .map(|i| {
                if i == 0 {
                    to
                } else {
                    context.recipients[rng.gen_range(0..context.recipients.len())]
                }
            })
            .collect();
        let call = disperseTokenSimpleCall {
            token: context.token,
            recipients,
            values: vec![U256::from(1); self.transfers_per_tx as usize],
        };
        TransactionRequest::default()
            .with_to(DISPERSE)
            .with_input(call.abi_encode())
            .with_gas_limit(
                TOKEN_MOVE_GAS_BASE
                    + u64::from(self.transfers_per_tx) * TOKEN_MOVE_GAS_PER_TRANSFER,
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{TestConfig, TxTypeConfig, WeightedTxType};

    #[test]
    fn config_requires_token_setup_and_bounded_batch() {
        let mut config: TestConfig = serde_yaml::from_str(include_str!(
            "../../../examples/mainnet-mix-mainnet-snapshot.yaml"
        ))
        .expect("mainnet mix must parse");
        for (transfers_per_tx, accepted) in [(0, false), (800, true), (801, false)] {
            let mut config = config.clone();
            config.transactions.push(WeightedTxType {
                weight: 1,
                tx_type: TxTypeConfig::TokenMove { transfers_per_tx },
            });
            assert_eq!(config.to_load_config(None).is_ok(), accepted, "{transfers_per_tx}");
        }
        config.real_token_setup = None;
        config.transactions = vec![WeightedTxType {
            weight: 1,
            tx_type: TxTypeConfig::TokenMove { transfers_per_tx: 20 },
        }];
        let error = config.to_load_config(None).expect_err("token_move must require token setup");
        assert!(error.to_string().contains("token_move requires enabled real_token_setup"));
    }

    #[test]
    fn generate_batches_transfers() {
        let token = address!("00000000000000000000000000000000000000c0");
        let to = address!("00000000000000000000000000000000000000b2");
        let pool = address!("00000000000000000000000000000000000000d3");
        let payload = TokenMovePayload {
            transfers_per_tx: 3,
            context: Arc::new(OnceLock::from(TokenMoveContext { token, recipients: vec![pool] })),
        };

        let tx = payload.generate(&mut SeededRng::new(1), Address::ZERO, to);

        assert_eq!(tx.to, Some(DISPERSE.into()));
        assert_eq!(tx.gas, Some(TOKEN_MOVE_GAS_BASE + 3 * TOKEN_MOVE_GAS_PER_TRANSFER));
        let call = disperseTokenSimpleCall::abi_decode(tx.input.input().expect("input set"))
            .expect("disperse call must decode");
        assert_eq!(call.token, token);
        assert_eq!(call.recipients, [to, pool, pool]);
        assert_eq!(call.values, [U256::from(1); 3]);
    }
}
