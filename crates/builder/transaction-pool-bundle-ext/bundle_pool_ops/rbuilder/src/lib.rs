//! Implementation of [`BundlePoolOperations`] for the classic rbuilder that
//! supports [`EthSendBundle`]s.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use core::fmt;
use std::{fmt::Formatter, path::Path, sync::Arc, time::Duration};

use derive_more::From;
use rbuilder::live_builder::cli::LiveBuilderConfig;
use rbuilder::{
    building::{
        builders::{
            block_building_helper::BlockBuildingHelper, ordering_builder::OrderingBuilderConfig,
            UnfinishedBlockBuildingSink, UnfinishedBlockBuildingSinkFactory,
        },
        Sorting,
    },
    live_builder::{
        base_config::load_config_toml_and_env,
        config::{create_builders, BuilderConfig, Config, SpecificBuilderConfig},
        order_input::{rpc_server::RawCancelBundle, ReplaceableOrderPoolCommand},
        payload_events::MevBoostSlotData,
        SlotSource,
    },
    primitives::{Bundle, BundleReplacementKey, Order},
    telemetry,
};
use reth_db_api::Database;
use reth_primitives::{TransactionSigned, U256};
use reth_provider::{DatabaseProviderFactory, HeaderProvider, StateProviderFactory};
use reth_rpc_types::beacon::events::PayloadAttributesEvent;
use tokio::{
    sync::{
        mpsc::{self, error::SendError},
        watch,
    },
    task,
    time::sleep,
};
use tokio_util::sync::CancellationToken;
use tracing::error;
use transaction_pool_bundle_ext::BundlePoolOperations;

/// [`BundlePoolOperations`] implementation which uses components of the
/// [`rbuilder`] under the hood to handle classic [`EthSendBundle`]s.
pub struct BundlePoolOps {
    // Channel to stream new [`OrderPool`] events to the rbuilder
    orderpool_tx: mpsc::Sender<ReplaceableOrderPoolCommand>,
    // Channel to stream new payload attribute events to rbuilder
    payload_attributes_tx: mpsc::UnboundedSender<(PayloadAttributesEvent, Option<u64>)>,
    /// Channel containing the latest [`BlockBuildingHelper`] recieved from the rbuilder
    block_building_helper_rx: watch::Receiver<Option<Box<dyn BlockBuildingHelper>>>,
}

#[derive(Debug)]
struct OurSlotSource {
    /// Channel [`OurSlotSource`] uses to receive payload attributes from reth
    payload_attributes_rx: mpsc::UnboundedReceiver<(PayloadAttributesEvent, Option<u64>)>,
}

impl SlotSource for OurSlotSource {
    fn recv_slot_channel(self) -> mpsc::UnboundedReceiver<MevBoostSlotData> {
        let (slot_sender, slot_receiver) = mpsc::unbounded_channel();

        // Spawn a task that receives payload attributes, converts them
        // into [`MevBoostSlotData`] for rbuilder, then forwards them.
        tokio::spawn(async move {
            let mut recv = self.payload_attributes_rx;
            while let Some((payload_event, gas_limit)) = recv.recv().await {
                let mev_boost_data = MevBoostSlotData {
                    payload_attributes_event: payload_event,
                    suggested_gas_limit: gas_limit.unwrap_or(0),
                    relays: vec![],
                    slot_data: Default::default(),
                };

                if slot_sender.send(mev_boost_data).is_err() {
                    error!("Error sending MevBoostSlotData through channel");
                    break;
                }
            }
        });

        // Return the receiver end for SlotSource trait
        slot_receiver
    }
}

impl BundlePoolOps {
    pub async fn new<P, DB>(
        provider: P,
        rbuilder_config_path: impl AsRef<Path>,
    ) -> Result<Self, Error>
    where
        DB: Database + Clone + 'static,
        P: DatabaseProviderFactory<DB> + StateProviderFactory + HeaderProvider + Clone + 'static,
    {
        // Create the payload source to trigger new block building
        let cancellation_token = CancellationToken::new();
        let (payload_attributes_tx, payload_attributes_rx) = mpsc::unbounded_channel();
        let slot_source = OurSlotSource {
            payload_attributes_rx,
        };

        let (block_building_helper_tx, block_building_helper_rx) = watch::channel(None);
        let sink_factory = SinkFactory {
            block_building_helper_tx,
        };

        // Spawn the builder!
        let config: Config = load_config_toml_and_env(rbuilder_config_path)?;

        let builder_strategy = BuilderConfig {
            name: "mp-ordering".to_string(),
            builder: SpecificBuilderConfig::OrderingBuilder(OrderingBuilderConfig {
                discard_txs: true,
                sorting: Sorting::MaxProfit,
                failed_order_retries: 1,
                drop_failed_orders: true,
                coinbase_payment: false,
                build_duration_deadline_ms: None,
            }),
        };

        let builders = create_builders(
            vec![builder_strategy],
            config.base_config.live_root_hash_config().unwrap(),
            config.base_config.root_hash_task_pool().unwrap(),
            config.base_config.sbundle_mergeabe_signers(),
        );

        // Build and run the process
        let builder = config
            .base_config
            .create_builder_with_provider_factory::<P, DB, OurSlotSource>(
                cancellation_token,
                Box::new(sink_factory),
                slot_source,
                provider,
            )
            .await
            .unwrap()
            .with_builders(builders);
        let orderpool_tx = builder.orderpool_sender.clone();

        // Spawn in separate thread
        let _handle = task::spawn(async move {
            // Wait for 5 seconds for reth to init
            sleep(Duration::from_secs(5)).await;

            // Spawn redacted server that is safe for tdx builders to expose
            telemetry::servers::redacted::spawn(
                config.base_config().redacted_telemetry_server_address(),
            )
            .await
            .expect("Failed to start redacted telemetry server");

            // Spawn debug server that exposes detailed operational information
            telemetry::servers::full::spawn(
                config.base_config().full_telemetry_server_address(),
                config.version_for_telemetry(),
                config.base_config().log_enable_dynamic,
            )
            .await
            .expect("Failed to start full telemetry server");

            builder.run().await.unwrap();

            Ok::<(), ()>
        });

        Ok(BundlePoolOps {
            block_building_helper_rx,
            payload_attributes_tx,
            orderpool_tx,
        })
    }
}

impl BundlePoolOperations for BundlePoolOps {
    /// Signed eth transaction
    type Transaction = TransactionSigned;
    type Bundle = Bundle;
    type CancelBundleReq = RawCancelBundle;
    type Error = Error;

    async fn add_bundle(&self, bundle: Self::Bundle) -> Result<(), Self::Error> {
        self.orderpool_tx
            .send(ReplaceableOrderPoolCommand::Order(Order::Bundle(bundle)))
            .await?;
        Ok(())
    }

    async fn cancel_bundle(
        &self,
        cancel_bundle_request: Self::CancelBundleReq,
    ) -> Result<(), Self::Error> {
        let key = BundleReplacementKey::new(
            cancel_bundle_request.replacement_uuid,
            cancel_bundle_request.signing_address,
        );
        self.orderpool_tx
            .send(ReplaceableOrderPoolCommand::CancelBundle(key))
            .await?;
        Ok(())
    }

    fn get_transactions(
        &self,
        requested_slot: U256,
    ) -> Result<impl IntoIterator<Item = Self::Transaction>, Self::Error> {
        match *self.block_building_helper_rx.borrow() {
            Some(ref block_builder) => {
                let rbuilder_slot = block_builder.building_context().block_env.number;
                if rbuilder_slot != requested_slot {
                    return Ok(vec![]);
                }
                let orders = block_builder.built_block_trace().included_orders.clone();
                let orders = orders
                    .iter()
                    .flat_map(|order| order.txs.iter())
                    .map(|er| er.clone().into_internal_tx_unsecure().into_signed())
                    .collect::<Vec<_>>();
                Ok(orders)
            }
            None => Ok(vec![]),
        }
    }

    fn notify_payload_attributes_event(
        &self,
        payload_attributes: PayloadAttributesEvent,
        gas_limit: Option<u64>,
    ) -> Result<(), Self::Error> {
        self.payload_attributes_tx
            .send((payload_attributes, gas_limit))?;
        Ok(())
    }
}

struct Sink {
    /// Channel for rbuilder to notify us of new [`BlockBuildingHelper`]s as it builds blocks
    block_building_helper_tx: watch::Sender<Option<Box<dyn BlockBuildingHelper>>>,
}

impl derive_more::Debug for Sink {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sink").finish()
    }
}

struct SinkFactory {
    /// Channel for rbuilder to notify us of new [`BlockBuildingHelper`]s as it builds blocks
    block_building_helper_tx: watch::Sender<Option<Box<dyn BlockBuildingHelper>>>,
}

impl derive_more::Debug for SinkFactory {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("SinkFactory").finish()
    }
}

impl UnfinishedBlockBuildingSinkFactory for SinkFactory {
    fn create_sink(
        &mut self,
        _slot_data: MevBoostSlotData,
        _cancel: CancellationToken,
    ) -> Arc<dyn UnfinishedBlockBuildingSink> {
        Arc::new(Sink {
            block_building_helper_tx: self.block_building_helper_tx.clone(),
        })
    }
}

impl UnfinishedBlockBuildingSink for Sink {
    fn new_block(&self, block: Box<dyn BlockBuildingHelper>) {
        self.block_building_helper_tx.send(Some(block)).unwrap()
    }

    fn can_use_suggested_fee_recipient_as_coinbase(&self) -> bool {
        true
    }
}

#[allow(clippy::large_enum_variant)]
/// [`BundlePoolOperations`] error type.
#[derive(Debug, From)]
pub enum Error {
    #[from]
    Eyre(eyre::Error),

    #[from]
    SendPayloadAttributes(SendError<(PayloadAttributesEvent, Option<u64>)>),

    #[from]
    SendReplaceableOrderPoolCommand(SendError<ReplaceableOrderPoolCommand>),
}
