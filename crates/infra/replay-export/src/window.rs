use std::{ops::RangeInclusive, time::Duration};

use alloy_provider::Provider;
use serde::{Deserialize, Serialize};

/// Canonical block endpoints and their half-open replay window, in Unix milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportWindow {
    /// RPC chain ID.
    pub chain_id: u64,
    /// Inclusive first block.
    pub from_block: u64,
    /// Inclusive last block.
    pub to_block: u64,
    /// First canonical block timestamp.
    pub from_timestamp_ms: u64,
    /// Last canonical block timestamp.
    pub to_timestamp_ms: u64,
    /// Requested warmup duration in milliseconds.
    pub warmup_ms: u64,
    /// Inclusive record-window start.
    pub start_ms: u64,
    /// Exclusive record-window end.
    pub end_ms: u64,
}

/// Invalid range, missing canonical block, or RPC discovery failure.
#[derive(Debug, thiserror::Error)]
pub enum WindowError {
    /// Invalid range, warmup precision, or timestamp arithmetic.
    #[error("invalid replay window: {0}")]
    Invalid(&'static str),
    /// Requested block is unavailable.
    #[error("canonical block {0} not found")]
    MissingBlock(u64),
    /// RPC discovery failed.
    #[error(transparent)]
    Rpc(#[from] alloy_transport::TransportError),
    /// RPC discovery timed out.
    #[error(transparent)]
    Timeout(#[from] tokio::time::error::Elapsed),
}

impl ExportWindow {
    /// Fixed end-window allowance, independent of subsecond execution slots.
    pub const END_ALLOWANCE_MS: u64 = 2_000;

    /// Resolve inclusive block endpoints without querying a successor block.
    /// Returns an error for invalid inputs, unavailable headers, or RPC failure.
    pub async fn resolve(
        provider: &impl Provider,
        blocks: RangeInclusive<u64>,
        warmup: Duration,
    ) -> Result<Self, WindowError> {
        if blocks.start() > blocks.end() || !warmup.subsec_nanos().is_multiple_of(1_000_000) {
            return Err(WindowError::Invalid("reversed range or fractional millisecond warmup"));
        }
        let warmup_ms = u64::try_from(warmup.as_millis())
            .map_err(|_| WindowError::Invalid("warmup overflow"))?;
        let (from, to, chain_id) = tokio::time::timeout(Duration::from_secs(30), async {
            let from = provider
                .get_block_by_number((*blocks.start()).into())
                .hashes()
                .await?
                .ok_or(WindowError::MissingBlock(*blocks.start()))?;
            let to = provider
                .get_block_by_number((*blocks.end()).into())
                .hashes()
                .await?
                .ok_or(WindowError::MissingBlock(*blocks.end()))?;
            Ok::<_, WindowError>((
                from.header.timestamp,
                to.header.timestamp,
                provider.get_chain_id().await?,
            ))
        })
        .await??;
        let from_timestamp_ms =
            from.checked_mul(1000).ok_or(WindowError::Invalid("timestamp overflow"))?;
        let to_timestamp_ms =
            to.checked_mul(1000).ok_or(WindowError::Invalid("timestamp overflow"))?;
        if from_timestamp_ms > to_timestamp_ms {
            return Err(WindowError::Invalid("decreasing canonical timestamps"));
        }
        Ok(Self {
            chain_id,
            from_block: *blocks.start(),
            to_block: *blocks.end(),
            from_timestamp_ms,
            to_timestamp_ms,
            warmup_ms,
            start_ms: from_timestamp_ms.saturating_sub(warmup_ms),
            end_ms: to_timestamp_ms
                .checked_add(Self::END_ALLOWANCE_MS)
                .ok_or(WindowError::Invalid("timestamp overflow"))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ops::RangeInclusive,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        time::Duration,
    };

    use alloy_provider::ProviderBuilder;
    use alloy_rpc_types_eth::Block;
    use jsonrpsee::{RpcModule, server::ServerBuilder};

    use super::ExportWindow;

    #[rstest::rstest]
    #[case(10..=11, Duration::from_secs(8), Some((52_000, 182_000)))]
    #[case(10..=10, Duration::ZERO, Some((60_000, 62_000)))]
    #[case(9..=9, Duration::from_secs(8), Some((0, 2_000)))]
    #[case(10..=10, Duration::from_secs(61), Some((0, 62_000)))]
    #[case(RangeInclusive::new(11, 10), Duration::ZERO, None)]
    #[case(10..=12, Duration::ZERO, None)]
    #[case(12..=12, Duration::ZERO, None)]
    #[case(10..=13, Duration::ZERO, None)]
    #[case(14..=14, Duration::ZERO, None)]
    #[case(15..=15, Duration::ZERO, None)]
    #[case(10..=11, Duration::from_nanos(1), None)]
    #[case(10..=11, Duration::MAX, None)]
    #[tokio::test]
    async fn resolves_window_when_reading_canonical_headers(
        #[case] blocks: RangeInclusive<u64>,
        #[case] warmup: Duration,
        #[case] expected: Option<(u64, u64)>,
    ) {
        let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", server.local_addr().unwrap());
        let requested = Arc::new(AtomicU64::new(0));
        let mut rpc = RpcModule::new(Arc::clone(&requested));
        rpc.register_method("eth_chainId", |_, _, _| "0x2105").unwrap();
        rpc.register_method("eth_getBlockByNumber", |params, requested, _| {
            let (number, _): (String, bool) = params.parse().unwrap();
            let number = u64::from_str_radix(number.trim_start_matches("0x"), 16).unwrap();
            requested.fetch_max(number, Ordering::Relaxed);
            let timestamp = match number {
                9 => 0,
                10 => 60,
                11 => 180,
                12 => return Ok(None),
                13 => 59,
                14 => u64::MAX,
                15 => u64::MAX / 1000,
                _ => panic!("unexpected block request: {number}"),
            };
            let mut block: Block = Block::default();
            block.header.number = number;
            block.header.timestamp = timestamp;
            Ok::<_, jsonrpsee::types::ErrorObjectOwned>(Some(block))
        })
        .unwrap();
        let handle = server.start(rpc);
        let provider = ProviderBuilder::new().connect_http(url.parse().unwrap());

        let result = ExportWindow::resolve(&provider, blocks.clone(), warmup).await;

        if let Some(expected) = expected {
            let window = result.unwrap();
            assert_eq!((window.start_ms, window.end_ms), expected);
            assert_eq!(
                (window.from_block, window.to_block, window.chain_id),
                (*blocks.start(), *blocks.end(), 8453)
            );
        } else {
            assert!(result.is_err());
        }
        assert!(requested.load(Ordering::Relaxed) <= *blocks.end());
        handle.stop().unwrap();
    }
}
