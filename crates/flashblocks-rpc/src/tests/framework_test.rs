#[cfg(test)]
mod tests {
    use crate::rpc::{EthApiExt, EthApiOverrideServer};
    use crate::state::FlashblocksState;
    use crate::subscription::{Flashblock, FlashblocksReceiver, Metadata};
    use crate::tests::{BLOCK_INFO_TXN, BLOCK_INFO_TXN_HASH};
    use alloy_consensus::Receipt;
    use alloy_eips::BlockNumberOrTag;
    use alloy_primitives::map::HashMap;
    use alloy_primitives::{Address, Bytes, B256, U256};
    use alloy_provider::Provider;
    use alloy_rpc_types_engine::PayloadId;
    use base_reth_test_utils::harness::TestHarness;
    use eyre::Result;
    use once_cell::sync::OnceCell;
    use op_alloy_consensus::OpDepositReceipt;
    use reth_exex::ExExEvent;
    use reth_optimism_primitives::OpReceipt;
    use rollup_boost::{ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1};
    use std::sync::Arc;
    use tokio::sync::{mpsc, oneshot};
    use tokio_stream::StreamExt;

    pub struct TestSetup {
        sender: mpsc::Sender<(Flashblock, oneshot::Sender<()>)>,
        harness: TestHarness,
    }

    impl TestSetup {
        pub async fn new() -> Result<Self> {
            let (sender, mut receiver) = mpsc::channel::<(Flashblock, oneshot::Sender<()>)>(100);
            let harness = TestHarness::new(|builder| {
                let fb_cell: Arc<OnceCell<Arc<FlashblocksState<_>>>> = Arc::new(OnceCell::new());

                builder
                    .install_exex("flashblocks-canon", {
                        let fb_cell = fb_cell.clone();
                        move |mut ctx| async move {
                            let fb = fb_cell
                                .get_or_init(|| {
                                    Arc::new(FlashblocksState::new(ctx.provider().clone()))
                                })
                                .clone();
                            Ok(async move {
                                while let Some(note) = ctx.notifications.try_next().await? {
                                    if let Some(committed) = note.committed_chain() {
                                        for b in committed.blocks_iter() {
                                            fb.on_canonical_block_received(b);
                                        }
                                        let _ = ctx.events.send(ExExEvent::FinishedHeight(
                                            committed.tip().num_hash(),
                                        ));
                                    }
                                }
                                Ok(())
                            })
                        }
                    })
                    .extend_rpc_modules(move |ctx| {
                        let fb = fb_cell
                            .get_or_init(|| Arc::new(FlashblocksState::new(ctx.provider().clone())))
                            .clone();

                        fb.start();

                        let api_ext = EthApiExt::new(
                            ctx.registry.eth_api().clone(),
                            ctx.registry.eth_handlers().filter.clone(),
                            fb.clone(),
                        );

                        ctx.modules.replace_configured(api_ext.into_rpc())?;

                        tokio::spawn(async move {
                            while let Some((payload, tx)) = receiver.recv().await {
                                fb.on_flashblock_received(payload);
                                tx.send(()).unwrap();
                            }
                        });

                        Ok(())
                    })
                    .launch()
            })
            .await?;

            Ok(Self { sender, harness })
        }

        pub async fn send_flashblock(&self, flashblock: Flashblock) -> Result<()> {
            let (tx, rx) = oneshot::channel();
            self.sender.send((flashblock, tx)).await?;
            rx.await?;
            Ok(())
        }
    }

    fn create_first_payload() -> Flashblock {
        Flashblock {
            payload_id: PayloadId::new([0; 8]),
            index: 0,
            base: Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: B256::default(),
                parent_hash: B256::default(),
                fee_recipient: Address::ZERO,
                prev_randao: B256::default(),
                block_number: 1,
                gas_limit: 30_000_000,
                timestamp: 0,
                extra_data: Bytes::new(),
                base_fee_per_gas: U256::ZERO,
            }),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                transactions: vec![BLOCK_INFO_TXN],
                ..Default::default()
            },
            metadata: Metadata {
                block_number: 1,
                receipts: {
                    let mut receipts = HashMap::default();
                    receipts.insert(
                        BLOCK_INFO_TXN_HASH,
                        OpReceipt::Deposit(OpDepositReceipt {
                            inner: Receipt {
                                status: true.into(),
                                cumulative_gas_used: 10000,
                                logs: vec![],
                            },
                            deposit_nonce: Some(4012991u64),
                            deposit_receipt_version: None,
                        }),
                    );
                    receipts
                },
                new_account_balances: HashMap::default(),
            },
        }
    }

    #[tokio::test]
    async fn test_get_pending_block() -> Result<()> {
        reth_tracing::init_test_tracing();
        let setup = TestSetup::new().await?;
        let provider = setup.harness.provider();

        let latest_block = provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await?
            .expect("latest block expected");
        assert_eq!(latest_block.number(), 0);

        // Querying pending block when it does not exist yet
        let pending_block = provider
            .get_block_by_number(BlockNumberOrTag::Pending)
            .await?;
        assert_eq!(pending_block.is_none(), true);

        let base_payload = create_first_payload();
        setup.send_flashblock(base_payload).await?;

        // Query pending block after sending the base payload with an empty delta
        let pending_block = provider
            .get_block_by_number(alloy_eips::BlockNumberOrTag::Pending)
            .await?
            .expect("pending block expected");

        assert_eq!(pending_block.number(), 1);
        assert_eq!(pending_block.transactions.hashes().len(), 1); // L1Info transaction

        Ok(())
    }
}
