//! Zero-configuration helpers for load testing on local devnets.
//!
//! When the RPC URL points to localhost, the load test can automatically provision
//! the funder account from well-known Hardhat/Anvil test keys so no manual key setup
//! is required. If a specific funder key is already configured, it is topped up from
//! those same reserve accounts if its balance is insufficient.

use std::time::{Duration, Instant};

use alloy_network::{EthereumWallet, TransactionBuilder};
use alloy_primitives::{Address, TxHash, U256};
use alloy_provider::Provider;
use alloy_rpc_types::TransactionRequest;
use alloy_signer_local::PrivateKeySigner;
use tracing::{debug, info, warn};
use url::Url;

use crate::{
    BaselineError, Result,
    rpc::{RpcClient, create_wallet_provider},
};

/// Well-known Hardhat/Anvil deterministic test private keys (standard mnemonic
/// `test test test test test test test test test test test junk`, indices 0–9).
/// These accounts are pre-funded in most local devnet genesis files.
pub const HARDHAT_TEST_KEYS: &[&str] = &[
    "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
    "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
    "0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a",
    "0x7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6",
    "0x47e179ec197488593b187f80a00eb0da91f1b9d0b13f8733639f19c30a34926a",
    "0x8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba",
    "0x92db14e403b83dfe3df233f83dfa3a0d7096f21ca9b0d6d6b8d88b2b4ec1564e",
    "0x4bbbf85ce3377467afe5d46f804f221813b2bb87f24d81f60f1fcdbf7cbf4356",
    "0xdbda1821b80551c9d65939329250298aa3472ba22feea921c0cf5d620ea67b97",
    "0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6",
];

/// Returns `true` if the RPC URL points to a local devnet.
pub fn is_local_rpc(url: &Url) -> bool {
    matches!(url.host_str(), Some("localhost") | Some("127.0.0.1") | Some("0.0.0.0") | Some("::1"))
}

/// Returns the first Hardhat test account that has a non-zero balance on the devnet.
///
/// Used to automatically select a funder when no key has been configured.
pub async fn devnet_funder(client: &RpcClient) -> Option<PrivateKeySigner> {
    for key_str in HARDHAT_TEST_KEYS {
        let Ok(signer) = key_str.parse::<PrivateKeySigner>() else { continue };
        match client.get_balance(signer.address()).await {
            Ok(bal) if !bal.is_zero() => {
                info!(address = %signer.address(), balance = %bal, "selected Hardhat account as devnet funder");
                return Some(signer);
            }
            Ok(_) => {
                debug!(address = %signer.address(), "Hardhat account has zero balance, skipping")
            }
            Err(e) => {
                warn!(address = %signer.address(), error = %e, "failed to check Hardhat account balance")
            }
        }
    }
    None
}

/// Ensures `funder_address` holds at least `funding_amount_per_account × sender_count` ETH.
///
/// If the funder balance is already sufficient this is a no-op. Otherwise the deficit
/// is transferred from the first Hardhat test account that can cover it.  Self-transfers
/// (when the funder is itself a Hardhat account) are skipped.
pub async fn ensure_funder_balance(
    client: &RpcClient,
    rpc_url: Url,
    funder_address: Address,
    funding_amount_per_account: U256,
    sender_count: u32,
    chain_id: u64,
    max_gas_price: u128,
) -> Result<()> {
    let needed = funding_amount_per_account.saturating_mul(U256::from(sender_count));
    let current = client.get_balance(funder_address).await?;
    if current >= needed {
        debug!(
            funder = %funder_address,
            balance = %current,
            needed = %needed,
            "funder has sufficient balance"
        );
        return Ok(());
    }
    let deficit = needed.saturating_sub(current);
    info!(
        funder = %funder_address,
        current = %current,
        needed = %needed,
        deficit = %deficit,
        "topping up funder from devnet reserve accounts"
    );

    for key_str in HARDHAT_TEST_KEYS {
        let Ok(signer) = key_str.parse::<PrivateKeySigner>() else { continue };
        if signer.address() == funder_address {
            continue; // Skip self-transfer.
        }

        let balance = match client.get_balance(signer.address()).await {
            Ok(b) => b,
            Err(e) => {
                warn!(reserve = %signer.address(), error = %e, "could not check reserve balance");
                continue;
            }
        };

        let gas_price = client.get_gas_price().await.unwrap_or(0);
        let max_priority_fee = (gas_price / 10).max(1);
        let max_fee = gas_price.saturating_mul(2).max(max_priority_fee).min(max_gas_price);
        let gas_cost = U256::from(21_000u128.saturating_mul(max_fee));
        let available = balance.saturating_sub(gas_cost);
        if available < deficit {
            debug!(
                reserve = %signer.address(),
                available = %available,
                deficit = %deficit,
                "reserve has insufficient funds, trying next"
            );
            continue;
        }

        let wallet = EthereumWallet::from(signer.clone());
        let provider = create_wallet_provider(rpc_url.clone(), wallet);
        let nonce = provider
            .get_transaction_count(signer.address())
            .pending()
            .await
            .map_err(|e| BaselineError::Rpc(e.to_string()))?;

        let tx = TransactionRequest::default()
            .with_to(funder_address)
            .with_value(deficit)
            .with_nonce(nonce)
            .with_chain_id(chain_id)
            .with_gas_limit(21_000)
            .with_max_fee_per_gas(max_fee)
            .with_max_priority_fee_per_gas(max_priority_fee);

        let pending = provider
            .send_transaction(tx)
            .await
            .map_err(|e| BaselineError::Transaction(format!("devnet top-up tx failed: {e}")))?;
        let tx_hash: TxHash = *pending.tx_hash();
        info!(
            from = %signer.address(),
            to = %funder_address,
            amount = %deficit,
            tx_hash = %tx_hash,
            "devnet top-up tx sent"
        );

        // Poll for confirmation.
        let timeout = Duration::from_secs(30);
        let start = Instant::now();
        loop {
            if start.elapsed() >= timeout {
                return Err(BaselineError::Transaction(
                    "devnet top-up tx timed out waiting for confirmation".into(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            match client.get_transaction_receipt(tx_hash).await {
                Ok(Some(_)) => break,
                Ok(None) => continue,
                Err(e) => {
                    warn!(tx_hash = %tx_hash, error = %e, "error polling for top-up receipt");
                }
            }
        }

        info!(funder = %funder_address, "devnet top-up confirmed");
        return Ok(());
    }

    Err(BaselineError::Config(format!(
        "no Hardhat test account has sufficient funds to top up funder {funder_address}"
    )))
}
