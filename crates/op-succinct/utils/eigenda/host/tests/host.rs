#[cfg(feature = "integration")]
mod host {
    use std::sync::Arc;

    use alloy_eips::BlockId;
    use alloy_primitives::B256;
    use anyhow::Result;
    use op_succinct_eigenda_host_utils::host::EigenDAOPSuccinctHost;
    use op_succinct_host_utils::{fetcher::OPSuccinctDataFetcher, host::OPSuccinctHost};

    /// Test context with pre-initialized host and block range.
    struct TestContext {
        host: EigenDAOPSuccinctHost,
        l2_start_block: u64,
        l2_end_block: u64,
        finalized_l1_hash: B256,
    }

    impl TestContext {
        async fn new() -> Result<Self> {
            dotenv::dotenv().ok();

            let fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;
            let host = EigenDAOPSuccinctHost::new(Arc::new(fetcher));

            let finalized_l2 = host.fetcher.get_l2_header(BlockId::finalized()).await?;
            let finalized_l1 = host.fetcher.get_l1_header(BlockId::finalized()).await?;

            Ok(Self {
                host,
                l2_start_block: finalized_l2.number.saturating_sub(1),
                l2_end_block: finalized_l2.number,
                finalized_l1_hash: finalized_l1.hash_slow(),
            })
        }
    }

    #[tokio::test]
    async fn test_fetch_constructs_valid_args() -> Result<()> {
        let ctx = TestContext::new().await?;
        let args = ctx.host.fetch(ctx.l2_start_block, ctx.l2_end_block, None, false).await?;

        assert_ne!(args.kona_cfg.l1_head, B256::ZERO);

        let expected_proxy = std::env::var("EIGENDA_PROXY_ADDRESS").ok();
        assert!(expected_proxy.is_some(), "EIGENDA_PROXY_ADDRESS must be set");
        assert_eq!(args.eigenda_proxy_address, expected_proxy);
        assert_eq!(args.recency_window, 0);
        assert_eq!(args.verbose, 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_fetch_uses_explicit_l1_head() -> Result<()> {
        let ctx = TestContext::new().await?;
        let args = ctx
            .host
            .fetch(ctx.l2_start_block, ctx.l2_end_block, Some(ctx.finalized_l1_hash), false)
            .await?;

        assert_eq!(args.kona_cfg.l1_head, ctx.finalized_l1_hash);
        Ok(())
    }

    #[tokio::test]
    async fn test_calculate_safe_l1_head_respects_finalized_boundary() -> Result<()> {
        let ctx = TestContext::new().await?;
        let safe_l1_head =
            ctx.host.calculate_safe_l1_head(&ctx.host.fetcher, ctx.l2_end_block, false).await?;

        assert_ne!(safe_l1_head, B256::ZERO);

        let safe_l1_header = ctx.host.fetcher.get_l1_header(safe_l1_head.into()).await?;
        let finalized_l1 = ctx.host.fetcher.get_l1_header(BlockId::finalized()).await?;
        assert!(safe_l1_header.number <= finalized_l1.number);

        Ok(())
    }

    #[tokio::test]
    #[should_panic(expected = "L2 start block is greater than or equal to L2 end block")]
    async fn test_fetch_rejects_invalid_block_range() {
        let ctx = TestContext::new().await.unwrap();
        ctx.host.fetch(ctx.l2_end_block, ctx.l2_end_block, None, false).await.unwrap();
    }
}
