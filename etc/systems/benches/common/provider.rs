//! Provider setup helpers for system benchmarks.

use std::time::Duration;

use alloy_primitives::{Address, U256};
use alloy_provider::{Identity, Provider, ProviderBuilder, RootProvider};
use base_common_network::Base;
use eyre::{Result, WrapErr};
use tokio::time::{sleep, timeout};
use url::Url;

/// Provider setup helpers for system benchmarks.
#[derive(Debug)]
pub struct BenchProvider;

impl BenchProvider {
    /// Connects an HTTP provider for the Base network type.
    pub fn connect_base(url: Url) -> RootProvider<Base> {
        ProviderBuilder::<Identity, Identity, Base>::default().connect_http(url)
    }

    /// Waits until all provided benchmark accounts are funded.
    pub async fn wait_for_balances(
        provider: &RootProvider<Base>,
        addresses: impl IntoIterator<Item = Address>,
        poll_interval: Duration,
        wait_timeout: Duration,
    ) -> Result<()> {
        timeout(wait_timeout, async {
            for address in addresses {
                loop {
                    let balance = provider.get_balance(address).await?;
                    if balance > U256::ZERO {
                        break;
                    }
                    sleep(poll_interval).await;
                }
            }
            Ok::<_, eyre::Error>(())
        })
        .await
        .wrap_err("timed out waiting for funded benchmark accounts")?
    }

    /// Waits until a benchmark account is funded.
    pub async fn wait_for_balance(
        provider: &RootProvider<Base>,
        address: Address,
        poll_interval: Duration,
        wait_timeout: Duration,
    ) -> Result<()> {
        timeout(wait_timeout, async {
            loop {
                let balance = provider.get_balance(address).await?;
                if balance > U256::ZERO {
                    return Ok::<_, eyre::Error>(());
                }
                sleep(poll_interval).await;
            }
        })
        .await
        .wrap_err("timed out waiting for funded benchmark account")?
    }
}
