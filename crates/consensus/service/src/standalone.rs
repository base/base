//! L1-free sequencing components for extending an existing L2 snapshot.

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_eips::{BlockNumHash, BlockNumberOrTag, eip2718::Encodable2718};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, keccak256};
use alloy_rpc_types_engine::PayloadAttributes;
use async_trait::async_trait;
use base_common_consensus::{Predeploys, TxDeposit};
use base_common_genesis::{BaseUpgrade, RollupConfig, SystemConfig};
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_consensus_derive::{
    AttributesBuilder, BuilderError, PipelineError, PipelineErrorKind, PipelineResult, Signal,
};
use base_consensus_engine::{Engine, EngineClient, EngineState};
use base_protocol::{BaseTimeUpdateTx, BlockInfo, L1BlockInfoTx, L2BlockInfo, L2BlockMetadata};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    ConductorClient, DerivationClientResult, EngineActor, EngineDerivationClient, EngineProcessor,
    L1OriginSelectorError, NodeActor, NodeOperatingMode, OriginSelector, PayloadBuilder,
    QueuedSequencerEngineClient, RecoveryModeGuard, SequencerActor,
    SequencerEngineRequestCoordinator, UnsafePayloadGossipClient, UnsafePayloadGossipClientError,
};

/// Builds payload attributes by extending the L1 epoch captured in an L2 snapshot.
///
/// This intentionally does not perform L1 derivation. It preserves the snapshot's L1 origin and
/// fee parameters, increments the L2 sequence number, and reads transactions from the normal EL
/// transaction pool. It is suitable for unsafe-chain development and benchmarking only.
#[derive(Debug, Clone)]
pub struct StandaloneAttributesBuilder {
    rollup_config: Arc<RollupConfig>,
    l1_info: L1BlockInfoTx,
    system_config: SystemConfig,
    prefund: Option<StandalonePrefund>,
    boundary_sequence_number: u64,
}

/// One-time account funding applied to the first descendant of a snapshot boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StandalonePrefund {
    /// Account receiving the minted ETH.
    pub address: Address,
    /// Amount of wei minted to the account.
    pub amount: u128,
}

impl StandaloneAttributesBuilder {
    /// Creates a standalone attributes builder from snapshot boundary metadata.
    pub fn new(
        rollup_config: Arc<RollupConfig>,
        l1_info: L1BlockInfoTx,
        system_config: SystemConfig,
        prefund: Option<StandalonePrefund>,
    ) -> Self {
        Self {
            rollup_config,
            l1_info,
            system_config,
            prefund,
            boundary_sequence_number: l1_info.sequence_number(),
        }
    }
}

#[async_trait]
impl AttributesBuilder for StandaloneAttributesBuilder {
    async fn prepare_payload_attributes(
        &mut self,
        l2_parent: L2BlockInfo,
        epoch: BlockNumHash,
    ) -> PipelineResult<BasePayloadAttributes> {
        if epoch != self.l1_info.id() || l2_parent.l1_origin != epoch {
            return Err(PipelineErrorKind::Reset(
                BuilderError::Custom("standalone L1 origin does not match L2 parent".to_string())
                    .into(),
            ));
        }

        let next_l2_block_number = l2_parent.block_info.number.saturating_add(1);
        let (next_l2_time, next_l2_timestamp_millis_part) =
            self.rollup_config.l2_block_timestamp_parts(next_l2_block_number);
        let l1_info = self.l1_info.with_sequence_number(l2_parent.seq_num.saturating_add(1));
        let l1_info_deposit = l1_info.into_deposit_tx(&self.rollup_config, next_l2_time);
        let mut encoded_l1_info = Vec::new();
        l1_info_deposit.encode_2718(&mut encoded_l1_info);

        let mut transactions = vec![Bytes::from(encoded_l1_info)];
        // Sub-second progression is conveyed solely by a `BaseTimeUpdateTx` deposit in the
        // transactions list (the payload attributes no longer carry a millis field). Denim is the
        // upgrade that activates the native sub-second block cadence, matching the production
        // stateful attributes builder.
        if self.rollup_config.is_denim_active(next_l2_time) {
            let base_time =
                BaseTimeUpdateTx::new(next_l2_timestamp_millis_part).map_err(|error| {
                    PipelineError::AttributesBuilder(BuilderError::BaseTimeUpdate(error)).crit()
                })?;
            let deposit = base_time.into_deposit_tx(next_l2_block_number);
            let mut encoded = Vec::new();
            deposit.encode_2718(&mut encoded);
            transactions.push(encoded.into());
        }

        if let Some(prefund) =
            self.prefund.filter(|_| l2_parent.seq_num == self.boundary_sequence_number)
        {
            let deposit = TxDeposit {
                source_hash: keccak256(prefund.address),
                from: prefund.address,
                to: TxKind::Call(prefund.address),
                mint: prefund.amount,
                value: U256::ZERO,
                gas_limit: 21_000,
                is_system_transaction: false,
                input: Bytes::new(),
            };
            let mut encoded = Vec::new();
            deposit.encode_2718(&mut encoded);
            transactions.push(encoded.into());
        }

        Ok(BasePayloadAttributes {
            payload_attributes: PayloadAttributes {
                timestamp: next_l2_time,
                prev_randao: B256::ZERO,
                suggested_fee_recipient: Predeploys::SEQUENCER_FEE_VAULT,
                withdrawals: self.rollup_config.is_canyon_active(next_l2_time).then(Vec::new),
                parent_beacon_block_root: self
                    .rollup_config
                    .is_ecotone_active(next_l2_time)
                    .then_some(B256::ZERO),
                slot_number: None,
                target_gas_limit: None,
            },
            transactions: Some(transactions),
            no_tx_pool: Some(false),
            gas_limit: Some(self.system_config.gas_limit),
            eip_1559_params: self.system_config.eip_1559_params(
                &self.rollup_config,
                l2_parent.block_info.timestamp,
                next_l2_time,
            ),
            min_base_fee: self
                .rollup_config
                .is_jovian_active(next_l2_time)
                .then(|| self.system_config.min_base_fee.unwrap_or_default()),
        })
    }
}

/// Rollup schedule and gas parameters for extending a snapshot head at the Denim cadence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandaloneDenimSchedule {
    /// Rollup configuration whose Denim schedule covers every descendant of the head.
    pub rollup_config: RollupConfig,
    /// Gas and fee parameters for the head's descendants.
    pub system_config: SystemConfig,
}

impl StandaloneDenimSchedule {
    /// Activates Denim at the first legacy slot after a pre-Denim head, or keeps the configured
    /// schedule when Denim is already active at the head.
    ///
    /// Activation divides the head's gas limit and multiplies its EIP-1559 denominator by
    /// [`RollupConfig::DENIM_GAS_PARAMETER_SCALING_FACTOR`], as derivation does for the first
    /// Denim block. A Denim head keeps its parameters, so a restart never rescales them. The
    /// execution layer must activate Denim at the same timestamp.
    pub fn new(
        mut rollup_config: RollupConfig,
        head: &L2BlockMetadata,
    ) -> Result<Self, StandaloneScheduleError> {
        let head_info = head.l2_block_info.block_info;
        let expected = rollup_config.l2_block_timestamp(head_info.number);
        if head_info.timestamp != expected {
            return Err(StandaloneScheduleError::HeadTimestamp {
                expected,
                actual: head_info.timestamp,
            });
        }
        let mut system_config = head.system_config;
        if rollup_config.is_denim_active(head_info.timestamp) {
            return Ok(Self { rollup_config, system_config });
        }
        if let Some(upgrade) = BaseUpgrade::CONTRACT_VARIANTS
            .into_iter()
            .take_while(|upgrade| *upgrade != BaseUpgrade::Denim)
            .find(|upgrade| {
                rollup_config
                    .upgrade_activation_timestamp(*upgrade)
                    .is_some_and(|activation| activation > head_info.timestamp)
            })
        {
            return Err(StandaloneScheduleError::PendingUpgrade(upgrade));
        }

        rollup_config.set_upgrade_activation_timestamp(
            BaseUpgrade::Denim,
            head_info.timestamp + rollup_config.block_time,
        );
        system_config.gas_limit /= u64::from(RollupConfig::DENIM_GAS_PARAMETER_SCALING_FACTOR);
        system_config.eip1559_denominator = system_config.eip1559_denominator.map(|denominator| {
            denominator.saturating_mul(RollupConfig::DENIM_GAS_PARAMETER_SCALING_FACTOR)
        });
        Ok(Self { rollup_config, system_config })
    }
}

/// Error choosing a Denim schedule for a snapshot head.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StandaloneScheduleError {
    /// The head's timestamp is not on the configured block schedule.
    #[error("snapshot head timestamp {actual} does not match scheduled timestamp {expected}")]
    HeadTimestamp {
        /// Timestamp the rollup schedule assigns to the head.
        expected: u64,
        /// Timestamp in the head header.
        actual: u64,
    },
    /// An upgrade ordered before Denim activates after the head.
    #[error("{0} activates after the snapshot head, so Denim cannot activate first")]
    PendingUpgrade(BaseUpgrade),
}

/// Selects the snapshot's captured L1 origin for every standalone L2 block.
#[derive(Debug, Clone, Copy)]
pub struct StandaloneOriginSelector {
    origin: BlockInfo,
}

impl StandaloneOriginSelector {
    /// Creates a fixed origin selector from a decoded L1 info transaction.
    pub fn new(l1_info: L1BlockInfoTx) -> Self {
        Self {
            origin: BlockInfo::new(
                l1_info.block_hash(),
                l1_info.id().number,
                B256::ZERO,
                l1_info.time(),
            ),
        }
    }
}

#[async_trait]
impl OriginSelector for StandaloneOriginSelector {
    async fn next_l1_origin(
        &mut self,
        unsafe_head: L2BlockInfo,
    ) -> Result<BlockInfo, L1OriginSelectorError> {
        if unsafe_head.l1_origin != self.origin.id() {
            return Err(L1OriginSelectorError::OriginNotFound(unsafe_head.l1_origin.hash));
        }

        // Pool activation enforces maximum sequencer drift using the selected origin timestamp.
        // Standalone mode intentionally has no advancing L1, so use the parent L2 timestamp as
        // the advisory origin time while retaining the snapshot origin's exact number and hash.
        Ok(BlockInfo { timestamp: unsafe_head.block_info.timestamp, ..self.origin })
    }
}

/// Discards derivation notifications for an L1-free standalone sequencer.
#[derive(Debug, Clone, Copy, Default)]
pub struct StandaloneDerivationClient;

#[async_trait]
impl EngineDerivationClient for StandaloneDerivationClient {
    async fn notify_sync_completed(&self, _safe_head: L2BlockInfo) -> DerivationClientResult<()> {
        Ok(())
    }

    async fn send_new_engine_safe_head(
        &self,
        _safe_head: L2BlockInfo,
    ) -> DerivationClientResult<()> {
        Ok(())
    }

    async fn send_signal(&self, _signal: Signal) -> DerivationClientResult<()> {
        Ok(())
    }
}

/// Discards unsafe payload gossip after the payload has been built locally.
#[derive(Debug, Clone, Copy, Default)]
pub struct StandaloneUnsafePayloadGossipClient;

#[async_trait]
impl UnsafePayloadGossipClient for StandaloneUnsafePayloadGossipClient {
    async fn schedule_execution_payload_gossip(
        &self,
        _payload: base_common_rpc_types_engine::BaseExecutionPayloadEnvelope,
    ) -> Result<(), UnsafePayloadGossipClientError> {
        Ok(())
    }
}

/// An L1-free sequencer that extends the unsafe chain of an existing L2 snapshot.
///
/// This node runs the production engine and sequencer actors, but intentionally omits derivation,
/// L1 watching, P2P, batching, and safe/finalized head advancement. It is intended only for local
/// development and execution benchmarking.
#[derive(Debug)]
pub struct StandaloneSequencerNode<E: EngineClient> {
    /// The snapshot-bound rollup configuration.
    pub rollup_config: Arc<RollupConfig>,
    /// The execution engine client connected to the snapshot-backed builder EL.
    pub engine_client: Arc<E>,
    /// The attributes builder seeded from the snapshot boundary.
    pub attributes_builder: StandaloneAttributesBuilder,
    /// The fixed-origin selector seeded from the snapshot boundary.
    pub origin_selector: StandaloneOriginSelector,
    /// Paces blocks from the head at startup instead of absolute chain time.
    ///
    /// Block timestamps still follow the rollup schedule; only local deadlines shift by the
    /// head's age. The first descendant is due one block interval after startup, and a restart
    /// resumes from the current head without producing blocks for the downtime.
    pub pace_from_head: bool,
}

impl<E: EngineClient + 'static> StandaloneSequencerNode<E> {
    /// Creates an L1-free sequencer from validated snapshot boundary metadata.
    pub fn new(
        rollup_config: Arc<RollupConfig>,
        engine_client: Arc<E>,
        l1_info: L1BlockInfoTx,
        system_config: SystemConfig,
        prefund: Option<StandalonePrefund>,
    ) -> Self {
        Self {
            attributes_builder: StandaloneAttributesBuilder::new(
                Arc::clone(&rollup_config),
                l1_info,
                system_config,
                prefund,
            ),
            origin_selector: StandaloneOriginSelector::new(l1_info),
            rollup_config,
            engine_client,
            pace_from_head: false,
        }
    }

    /// Runs the standalone sequencer until an actor fails or shutdown is requested.
    pub async fn start(&self) -> Result<(), String> {
        self.start_with_cancellation(CancellationToken::new()).await
    }

    /// Runs the standalone sequencer with a caller-provided cancellation token.
    pub async fn start_with_cancellation(
        &self,
        cancellation: CancellationToken,
    ) -> Result<(), String> {
        let schedule_delay = if self.pace_from_head {
            let head = self
                .engine_client
                .l2_block_info_by_label(BlockNumberOrTag::Latest)
                .await
                .map_err(|error| format!("failed to read standalone head: {error}"))?
                .ok_or("standalone execution node has no head")?;
            let head_time = UNIX_EPOCH
                + Duration::from_millis(
                    self.rollup_config.l2_block_timestamp_millis(head.block_info.number),
                );
            SystemTime::now().duration_since(head_time).unwrap_or_default()
        } else {
            Duration::ZERO
        };
        let (engine_actor_request_tx, engine_actor_request_rx) = mpsc::channel(1024);
        let (unsafe_head_tx, unsafe_head_rx) = watch::channel(L2BlockInfo::default());
        let (engine_state_tx, engine_state_rx) = watch::channel(EngineState::default());
        let (engine_queue_length_tx, _engine_queue_length_rx) = watch::channel(0);
        let engine = Engine::new(EngineState::default(), engine_state_tx, engine_queue_length_tx);
        let processor = EngineProcessor::new_skip_reset(
            Arc::clone(&self.engine_client),
            Arc::clone(&self.rollup_config),
            StandaloneDerivationClient,
            engine,
        );
        let coordinator = SequencerEngineRequestCoordinator::new(
            processor,
            NodeOperatingMode::Sequencer,
            None,
            false,
            unsafe_head_tx,
        );
        let engine_actor =
            EngineActor::new(cancellation.clone(), engine_actor_request_rx, coordinator);

        let sequencer_engine_client = Arc::new(QueuedSequencerEngineClient {
            engine_actor_request_tx,
            unsafe_head_rx,
            engine_state_rx,
        });
        let (_sequencer_admin_tx, sequencer_admin_rx) = mpsc::channel(1024);
        let recovery_mode = RecoveryModeGuard::new(false);
        let sequencer_actor: SequencerActor<_, ConductorClient, _, _, _> = SequencerActor {
            admin_api_rx: sequencer_admin_rx,
            builder: PayloadBuilder {
                attributes_builder: self.attributes_builder.clone(),
                engine_client: Arc::clone(&sequencer_engine_client),
                origin_selector: self.origin_selector,
                recovery_mode: recovery_mode.clone(),
                rollup_config: Arc::clone(&self.rollup_config),
            },
            cancellation_token: cancellation.clone(),
            conductor: None,
            engine_client: sequencer_engine_client,
            is_active: true,
            mode: NodeOperatingMode::Sequencer,
            shadow_funding: None,
            recovery_mode,
            rollup_config: Arc::clone(&self.rollup_config),
            unsafe_payload_gossip_client: StandaloneUnsafePayloadGossipClient,
            sealer: None,
            pending_stop: None,
            seal_offset: base_protocol::DEFAULT_SEAL_OFFSET,
            schedule_delay,
        };

        crate::service::spawn_and_wait!(
            cancellation,
            actors = [Some((sequencer_actor, ())), Some((engine_actor, ())),]
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Transaction as _;
    use alloy_eips::{BlockNumHash, eip2718::Decodable2718};
    use alloy_primitives::{Address, B64, B256, U256};
    use base_common_consensus::BaseTxEnvelope;
    use base_common_genesis::{BaseUpgrade, RollupConfig, SystemConfig};
    use base_consensus_derive::AttributesBuilder;
    use base_protocol::{
        BaseTimeUpdateTx, BlockInfo, L1BlockInfoBedrock, L1BlockInfoTx, L2BlockInfo,
        L2BlockMetadata,
    };

    use super::{
        StandaloneAttributesBuilder, StandaloneDenimSchedule, StandaloneOriginSelector,
        StandalonePrefund, StandaloneScheduleError,
    };
    use crate::OriginSelector;

    fn snapshot_boundary() -> (L1BlockInfoTx, L2BlockInfo) {
        let origin_hash = B256::repeat_byte(0x11);
        let l1_info = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock::new(
            100,
            1_000,
            7,
            origin_hash,
            9,
            Address::repeat_byte(0x22),
            U256::from(3),
            U256::from(4),
        ));
        let parent = L2BlockInfo::new(
            BlockInfo::new(B256::repeat_byte(0x33), 200, B256::repeat_byte(0x44), 2_000),
            BlockNumHash { number: 100, hash: origin_hash },
            9,
        );
        (l1_info, parent)
    }

    fn anchored_rollup(parent: L2BlockInfo, first_timestamp: u64) -> RollupConfig {
        let mut rollup = RollupConfig { block_time: 2, ..Default::default() };
        rollup.genesis.l2.number = 0;
        rollup.genesis.l2_time =
            first_timestamp - parent.block_info.number.saturating_add(1) * rollup.block_time;
        rollup
    }

    #[tokio::test]
    async fn builds_next_block_in_snapshot_epoch() {
        let (l1_info, parent) = snapshot_boundary();
        let rollup = std::sync::Arc::new(anchored_rollup(parent, 2_002));
        let mut builder = StandaloneAttributesBuilder::new(
            std::sync::Arc::clone(&rollup),
            l1_info,
            SystemConfig { gas_limit: 30_000_000, ..Default::default() },
            None,
        );

        let attributes = builder.prepare_payload_attributes(parent, l1_info.id()).await.unwrap();

        assert_eq!(attributes.payload_attributes.timestamp, 2_000 + rollup.block_time);
        assert_eq!(attributes.gas_limit, Some(30_000_000));
        assert_eq!(attributes.no_tx_pool, Some(false));
        let transactions = attributes.transactions.unwrap();
        let mut encoded = transactions[0].as_ref();
        let envelope = BaseTxEnvelope::decode_2718(&mut encoded).unwrap();
        let deposit = envelope.as_deposit().unwrap();
        let next_l1_info = L1BlockInfoTx::decode_calldata(deposit.input().as_ref()).unwrap();
        assert_eq!(next_l1_info.id(), l1_info.id());
        assert_eq!(next_l1_info.sequence_number(), 10);
    }

    #[tokio::test]
    async fn prefunds_only_first_snapshot_descendant() {
        let (l1_info, parent) = snapshot_boundary();
        let address = Address::repeat_byte(0x55);
        let mut builder = StandaloneAttributesBuilder::new(
            std::sync::Arc::new(anchored_rollup(parent, 2_002)),
            l1_info,
            SystemConfig { gas_limit: 30_000_000, ..Default::default() },
            Some(StandalonePrefund { address, amount: 1_000_000 }),
        );

        let first = builder.prepare_payload_attributes(parent, l1_info.id()).await.unwrap();
        assert_eq!(first.transactions.unwrap().len(), 2);

        let later_parent = L2BlockInfo { seq_num: parent.seq_num + 1, ..parent };
        let later = builder.prepare_payload_attributes(later_parent, l1_info.id()).await.unwrap();
        assert_eq!(later.transactions.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn first_descendant_uses_anchored_rollup_schedule() {
        let (l1_info, parent) = snapshot_boundary();
        let mut builder = StandaloneAttributesBuilder::new(
            std::sync::Arc::new(anchored_rollup(parent, 10_000)),
            l1_info,
            SystemConfig { gas_limit: 30_000_000, ..Default::default() },
            None,
        );

        let first = builder.prepare_payload_attributes(parent, l1_info.id()).await.unwrap();
        assert_eq!(first.payload_attributes.timestamp, 10_000);
    }

    #[tokio::test]
    async fn subsecond_descendants_include_base_time_progression() {
        let (l1_info, parent) = snapshot_boundary();
        let mut rollup = anchored_rollup(parent, 2_002);
        rollup.set_upgrade_activation_timestamp(base_common_genesis::BaseUpgrade::Denim, 2_002);
        let mut builder = StandaloneAttributesBuilder::new(
            std::sync::Arc::new(rollup),
            l1_info,
            SystemConfig { gas_limit: 30_000_000, ..Default::default() },
            None,
        );

        for (offset, expected) in
            [(1, (2_002, 0)), (2, (2_002, 200)), (5, (2_002, 800)), (6, (2_003, 0))]
        {
            let block_parent = L2BlockInfo {
                block_info: BlockInfo {
                    number: parent.block_info.number + offset - 1,
                    timestamp: expected.0,
                    ..parent.block_info
                },
                seq_num: parent.seq_num + offset - 1,
                ..parent
            };
            let attributes =
                builder.prepare_payload_attributes(block_parent, l1_info.id()).await.unwrap();
            assert_eq!(attributes.payload_attributes.timestamp, expected.0);
            let transactions = attributes.transactions.unwrap();
            let mut encoded = transactions[1].as_ref();
            let envelope = BaseTxEnvelope::decode_2718(&mut encoded).unwrap();
            let base_time =
                BaseTimeUpdateTx::decode_calldata(envelope.as_deposit().unwrap().input().as_ref())
                    .unwrap();
            assert_eq!(base_time.timestamp_millis_part(), expected.1);
        }
    }

    #[tokio::test]
    async fn fixed_origin_rejects_another_epoch() {
        let (l1_info, mut parent) = snapshot_boundary();
        let mut selector = StandaloneOriginSelector::new(l1_info);
        parent.l1_origin.hash = B256::repeat_byte(0xff);

        let result = selector.next_l1_origin(parent).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn fixed_origin_tracks_parent_time_for_pool_activation() {
        let (l1_info, mut parent) = snapshot_boundary();
        let mut selector = StandaloneOriginSelector::new(l1_info);
        parent.block_info.timestamp = 10_000;

        let origin = selector.next_l1_origin(parent).await.unwrap();

        assert_eq!(origin.id(), l1_info.id());
        assert_eq!(origin.timestamp, 10_000);
    }

    #[tokio::test]
    async fn denim_schedule_activates_and_scales_once_after_pre_denim_head() {
        let (l1_info, parent) = snapshot_boundary();
        let head = L2BlockMetadata {
            l2_block_info: parent,
            l1_info,
            system_config: SystemConfig {
                gas_limit: 150_000_000,
                eip1559_denominator: Some(250),
                eip1559_elasticity: Some(6),
                min_base_fee: Some(1_000),
                ..Default::default()
            },
        };

        let mut rollup = anchored_rollup(parent, 2_002);
        rollup.set_upgrade_activation_timestamp(BaseUpgrade::Holocene, 0);

        let schedule = StandaloneDenimSchedule::new(rollup, &head).unwrap();

        let rollup = &schedule.rollup_config;
        assert_eq!(rollup.upgrade_activation_timestamp(BaseUpgrade::Denim), Some(2_002));
        assert_eq!(rollup.l2_block_timestamp_parts(201), (2_002, 0));
        assert_eq!(rollup.l2_block_timestamp_parts(202), (2_002, 200));
        assert_eq!(rollup.l2_block_timestamp_parts(206), (2_003, 0));
        let expected = SystemConfig {
            gas_limit: 15_000_000,
            eip1559_denominator: Some(2_500),
            ..head.system_config
        };
        assert_eq!(schedule.system_config, expected);
        let mut builder = StandaloneAttributesBuilder::new(
            std::sync::Arc::new(rollup.clone()),
            l1_info,
            schedule.system_config,
            None,
        );
        let first = builder.prepare_payload_attributes(parent, l1_info.id()).await.unwrap();
        assert_eq!(first.gas_limit, Some(15_000_000));
        assert_eq!(first.eip_1559_params, Some(B64::from(0x0000_09c4_0000_0006_u64)));

        let restarted_head = L2BlockMetadata {
            l2_block_info: L2BlockInfo {
                block_info: BlockInfo { number: 206, timestamp: 2_003, ..parent.block_info },
                ..parent
            },
            l1_info,
            system_config: schedule.system_config,
        };
        assert_eq!(
            StandaloneDenimSchedule::new(schedule.rollup_config.clone(), &restarted_head),
            Ok(schedule)
        );
    }

    #[test]
    fn denim_schedule_rejects_off_schedule_head_and_pending_upgrade() {
        let (l1_info, parent) = snapshot_boundary();
        let mut head = L2BlockMetadata {
            l2_block_info: parent,
            l1_info,
            system_config: SystemConfig::default(),
        };
        let mut rollup = anchored_rollup(parent, 2_002);
        rollup.set_upgrade_activation_timestamp(BaseUpgrade::Cobalt, 2_100);

        assert_eq!(
            StandaloneDenimSchedule::new(rollup.clone(), &head),
            Err(StandaloneScheduleError::PendingUpgrade(BaseUpgrade::Cobalt))
        );

        head.l2_block_info.block_info.timestamp += 1;
        assert_eq!(
            StandaloneDenimSchedule::new(rollup, &head),
            Err(StandaloneScheduleError::HeadTimestamp { expected: 2_000, actual: 2_001 })
        );
    }
}
