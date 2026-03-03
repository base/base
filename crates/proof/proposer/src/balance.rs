//! Periodic L1 balance monitoring for the proposer address.

use std::{sync::Arc, time::Duration};

use alloy_primitives::Address;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::rpc::L1Client;

/// Balance polling interval.
pub const BALANCE_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Periodically polls the L1 balance of `address` and records it as a Prometheus gauge.
pub async fn balance_monitor<L1: L1Client>(
    l1_client: Arc<L1>,
    address: Address,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            () = tokio::time::sleep(BALANCE_POLL_INTERVAL) => {
                match l1_client.get_balance(address).await {
                    Ok(balance) => {
                        // U256 -> f64 conversion: safe enough for gauge display.
                        let balance_f64: f64 = balance.to_string().parse().unwrap_or(f64::MAX);
                        metrics::gauge!(crate::ACCOUNT_BALANCE_WEI).set(balance_f64);
                    }
                    Err(e) => {
                        debug!(error = %e, "Failed to fetch account balance");
                    }
                }
            }
        }
    }
}
