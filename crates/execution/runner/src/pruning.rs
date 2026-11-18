//! Startup validation for historical-state pruning before node components are built.

use eyre::{Result, bail};
use reth_config::PruneConfig;
use reth_node_builder::{BuilderContext, FullNodeTypes, components::NodeComponentsBuilder};
use reth_provider::{BlockNumReader, DatabaseProviderFactory, PruneCheckpointReader};
use reth_prune_types::{MINIMUM_UNWIND_SAFE_DISTANCE, PrunePurpose, PruneSegment};

use crate::BaseNode;

/// Validates historical-state prune backlog before building the wrapped Base components.
#[derive(Debug)]
pub struct PruneGuardedComponents<CB>(pub CB);

impl<N, CB> NodeComponentsBuilder<N> for PruneGuardedComponents<CB>
where
    N: FullNodeTypes<Types = BaseNode>,
    CB: NodeComponentsBuilder<N>,
{
    type Components = CB::Components;

    async fn build_components(self, ctx: &BuilderContext<N>) -> Result<Self::Components> {
        let mut prune = ctx.reth_config().prune.clone();
        if let Some(mut cli) = ctx.config().prune_config() {
            cli.merge(prune);
            prune = cli;
        }
        {
            let provider = ctx.provider().database_provider_ro()?;
            Self::validate(
                &provider,
                &prune,
                ctx.config().engine.persistence_backpressure_threshold(),
            )?;
        }
        self.0.build_components(ctx).await
    }
}

impl<CB> PruneGuardedComponents<CB> {
    /// Checks persisted executed progress and prune checkpoints without changing the database.
    pub fn validate<P: BlockNumReader + PruneCheckpointReader>(
        provider: &P,
        prune: &PruneConfig,
        allowance: u64,
    ) -> Result<()> {
        // Match PrunerBuilder's treatment of the default versus an explicit minimum override.
        let minimum = (prune.minimum_pruning_distance != MINIMUM_UNWIND_SAFE_DISTANCE)
            .then_some(prune.minimum_pruning_distance);
        let tip = provider.best_block_number()?;
        for (segment, mode) in [
            (PruneSegment::AccountHistory, prune.segments.account_history),
            (PruneSegment::StorageHistory, prune.segments.storage_history),
        ] {
            let Some(mode) = mode else { continue };
            let Some((target, _)) =
                mode.prune_target_block_with_min(tip, segment, PrunePurpose::User, minimum)?
            else {
                continue;
            };
            let checkpoint = provider.get_prune_checkpoint(segment)?.and_then(|c| c.block_number);
            let backlog = checkpoint.map_or_else(
                || target.saturating_add(1),
                |checkpoint| target.saturating_sub(checkpoint),
            );
            if backlog > allowance {
                bail!(
                    "historical-state prune backlog exceeds startup allowance: segment={segment} executed_tip={tip} checkpoint={checkpoint:?} target={target} backlog={backlog} allowance={allowance}; stop the node and run `base reth prune` offline with the same --chain <CHAIN_OR_CUSTOM_SPEC>, --datadir <DATADIR>, --config <CONFIG>, and any custom static-files and RocksDB paths, then restart"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use base_execution_chainspec::BaseChainSpec;
    use reth_db::{DatabaseEnv, test_utils::TempDatabase};
    use reth_node_builder::{
        FullNodeTypesAdapter, Node, NodeConfig, NodeTypesWithDBAdapter, common::WithConfigs,
    };
    use reth_primitives_traits::{Header, SealedHeader};
    use reth_provider::{
        PruneCheckpointWriter, StageCheckpointWriter,
        providers::BlockchainProvider,
        test_utils::{
            create_test_provider_factory, create_test_provider_factory_with_node_types,
            insert_headers,
        },
    };
    use reth_prune_types::{PruneCheckpoint, PruneMode, PruneSegment};
    use reth_stages_types::{StageCheckpoint, StageId};
    use reth_tasks::Runtime;

    use super::*;

    /// Base node types backed by the existing temporary database fixture.
    type FixtureTypes = FullNodeTypesAdapter<
        BaseNode,
        Arc<TempDatabase<DatabaseEnv>>,
        BlockchainProvider<NodeTypesWithDBAdapter<BaseNode, Arc<TempDatabase<DatabaseEnv>>>>,
    >;

    /// Records delegation without starting networking or other node services.
    #[derive(Debug)]
    struct SentinelBuilder(Arc<AtomicUsize>);

    impl NodeComponentsBuilder<FixtureTypes> for SentinelBuilder {
        type Components =
            <<BaseNode as Node<FixtureTypes>>::ComponentsBuilder as NodeComponentsBuilder<
                FixtureTypes,
            >>::Components;

        async fn build_components(
            self,
            _ctx: &BuilderContext<FixtureTypes>,
        ) -> Result<Self::Components> {
            self.0.fetch_add(1, Ordering::SeqCst);
            bail!("delegate reached")
        }
    }

    /// Exercises actual component construction admission with resolved CLI and loaded TOML config.
    #[tokio::test]
    async fn component_guard_resolves_policy_and_controls_delegation() {
        let chain = Arc::new(BaseChainSpec::mainnet());
        let factory = create_test_provider_factory_with_node_types::<BaseNode>(Arc::clone(&chain));
        let writer = factory.provider_rw().expect("open fixture writer");
        writer
            .save_stage_checkpoint(StageId::Finish, StageCheckpoint::new(30_000_000))
            .expect("save Finish");
        for segment in [PruneSegment::AccountHistory, PruneSegment::StorageHistory] {
            writer
                .save_prune_checkpoint(segment, PruneCheckpoint::pruned_through(16_608_000))
                .expect("save checkpoint");
        }
        writer.commit().expect("commit fixture");
        let provider = BlockchainProvider::with_latest(
            factory,
            SealedHeader::seal_slow(Header { number: 30_000_000, ..Default::default() }),
        )
        .expect("create blockchain provider");
        let calls = Arc::new(AtomicUsize::new(0));
        for (
            cli_account,
            cli_storage,
            toml_distance,
            allowance,
            expected_calls,
            expected_segment,
        ) in [
            (
                Some(8_640_000),
                Some(8_640_000),
                13_392_000,
                160,
                0,
                Some(PruneSegment::AccountHistory),
            ),
            (None, None, 8_640_000, 160, 0, Some(PruneSegment::AccountHistory)),
            (
                Some(13_392_000),
                Some(8_640_000),
                13_392_000,
                160,
                0,
                Some(PruneSegment::StorageHistory),
            ),
            (None, None, 13_392_000, 160, 1, None),
            (Some(13_392_000), Some(13_392_000), 8_640_000, 160, 2, None),
            (Some(8_640_000), Some(8_640_000), 13_392_000, 4_752_000, 3, None),
        ] {
            let mut config = NodeConfig::new(Arc::clone(&chain));
            config.pruning.account_history_distance = cli_account;
            config.pruning.storage_history_distance = cli_storage;
            config.engine.persistence_backpressure_threshold = Some(allowance);
            let mut toml_config = reth_config::Config::default();
            toml_config.prune.segments.account_history = Some(PruneMode::Distance(toml_distance));
            toml_config.prune.segments.storage_history = Some(PruneMode::Distance(toml_distance));
            let ctx = BuilderContext::<FixtureTypes>::new(
                Default::default(),
                provider.clone(),
                Runtime::test(),
                WithConfigs { config, toml_config },
            );
            let error = PruneGuardedComponents(SentinelBuilder(Arc::clone(&calls)))
                .build_components(&ctx)
                .await
                .err()
                .expect("sentinel or guard must return an error");
            let expected_error = expected_segment.map_or_else(
                || "delegate reached".to_owned(),
                |segment| format!("historical-state prune backlog exceeds startup allowance: segment={segment} executed_tip=30000000 checkpoint=Some(16608000) target=21360000 backlog=4752000 allowance={allowance}"),
            );
            assert_eq!(
                error.to_string().split("; stop").next().expect("diagnostic prefix"),
                expected_error,
                "component boundary must select the expected policy"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                expected_calls,
                "guard must prevent delegation on refusal and delegate exactly once on acceptance"
            );
        }
    }

    /// Covers segment independence, checkpoint edges, retention modes, and minimum-distance rules.
    #[test]
    fn history_backlog_boundaries() {
        // Outer None means no checkpoint row; Some(None) means block zero is unfinished.
        for (name, tip, account, storage, checkpoint, minimum, allowance, expected) in [
            ("archive", 30_000_000, None, None, None, MINIMUM_UNWIND_SAFE_DISTANCE, 160, None),
            (
                "genesis",
                0,
                Some(PruneMode::Distance(8_640_000)),
                None,
                None,
                MINIMUM_UNWIND_SAFE_DISTANCE,
                160,
                None,
            ),
            (
                "matched 31 days",
                30_000_000,
                Some(PruneMode::Distance(13_392_000)),
                None,
                Some(Some(16_608_000)),
                MINIMUM_UNWIND_SAFE_DISTANCE,
                160,
                None,
            ),
            (
                "wider retention",
                30_000_000,
                Some(PruneMode::Distance(14_000_000)),
                None,
                Some(Some(16_608_000)),
                MINIMUM_UNWIND_SAFE_DISTANCE,
                160,
                None,
            ),
            (
                "equal allowance",
                30_000_000,
                Some(PruneMode::Distance(8_640_000)),
                None,
                Some(Some(21_359_840)),
                MINIMUM_UNWIND_SAFE_DISTANCE,
                160,
                None,
            ),
            (
                "one over allowance",
                30_000_000,
                Some(PruneMode::Distance(8_640_000)),
                None,
                Some(Some(21_359_839)),
                MINIMUM_UNWIND_SAFE_DISTANCE,
                160,
                Some((PruneSegment::AccountHistory, 21_359_839, 21_360_000, 161)),
            ),
            (
                "only storage behind",
                30_000_000,
                None,
                Some(PruneMode::Distance(8_640_000)),
                Some(Some(16_608_000)),
                MINIMUM_UNWIND_SAFE_DISTANCE,
                160,
                Some((PruneSegment::StorageHistory, 16_608_000, 21_360_000, 4_752_000)),
            ),
            (
                "checkpoint ahead",
                30_000_000,
                None,
                Some(PruneMode::Distance(8_640_000)),
                Some(Some(22_000_000)),
                MINIMUM_UNWIND_SAFE_DISTANCE,
                160,
                None,
            ),
            (
                "missing checkpoint",
                200,
                Some(PruneMode::Before(161)),
                None,
                None,
                0,
                160,
                Some((PruneSegment::AccountHistory, u64::MAX, 160, 161)),
            ),
            (
                "unfinished block zero",
                200,
                Some(PruneMode::Before(161)),
                None,
                Some(None),
                0,
                160,
                Some((PruneSegment::AccountHistory, u64::MAX, 160, 161)),
            ),
            ("missing at equality", 200, Some(PruneMode::Before(160)), None, None, 0, 160, None),
            (
                "before complete",
                30_000_000,
                Some(PruneMode::Before(16_608_001)),
                None,
                Some(Some(16_608_000)),
                MINIMUM_UNWIND_SAFE_DISTANCE,
                160,
                None,
            ),
            (
                "before within minimum",
                30_000_000,
                Some(PruneMode::Before(30_000_000)),
                None,
                None,
                MINIMUM_UNWIND_SAFE_DISTANCE,
                160,
                None,
            ),
            (
                "full below minimum",
                MINIMUM_UNWIND_SAFE_DISTANCE - 1,
                Some(PruneMode::Full),
                None,
                None,
                MINIMUM_UNWIND_SAFE_DISTANCE,
                160,
                None,
            ),
            (
                "full default minimum",
                MINIMUM_UNWIND_SAFE_DISTANCE + 200,
                Some(PruneMode::Full),
                None,
                Some(Some(40)),
                MINIMUM_UNWIND_SAFE_DISTANCE,
                160,
                None,
            ),
            (
                "full custom minimum",
                200,
                Some(PruneMode::Full),
                None,
                Some(Some(19)),
                20,
                160,
                Some((PruneSegment::AccountHistory, 19, 180, 161)),
            ),
        ] {
            let factory = create_test_provider_factory();
            let writer = factory.provider_rw().expect("open fixture writer");
            writer
                .save_stage_checkpoint(StageId::Finish, StageCheckpoint::new(tip))
                .expect("save executed tip");
            if let Some(block_number) = checkpoint {
                for segment in [PruneSegment::AccountHistory, PruneSegment::StorageHistory] {
                    writer
                        .save_prune_checkpoint(
                            segment,
                            PruneCheckpoint {
                                block_number,
                                tx_number: None,
                                prune_mode: PruneMode::Full,
                            },
                        )
                        .expect("save checkpoint");
                }
            }
            writer.commit().expect("commit fixture");
            let mut prune = PruneConfig { minimum_pruning_distance: minimum, ..Default::default() };
            prune.segments.account_history = account;
            prune.segments.storage_history = storage;
            prune.segments.receipts = Some(PruneMode::Full);
            let provider = factory.provider().expect("open read-only fixture");
            let before = provider.get_prune_checkpoints().expect("snapshot checkpoints");
            let result = PruneGuardedComponents::<()>::validate(&provider, &prune, allowance);
            if let Some((segment, block, target, backlog)) = expected {
                let checkpoint = (block != u64::MAX).then_some(block);
                let error = result.expect_err(name).to_string();
                assert_eq!(
                    error.split("; stop").next().expect("diagnostic prefix"),
                    format!(
                        "historical-state prune backlog exceeds startup allowance: segment={segment} executed_tip={tip} checkpoint={checkpoint:?} target={target} backlog={backlog} allowance={allowance}"
                    ),
                    "{name}: exact backlog diagnostic"
                );
            } else {
                assert!(result.is_ok(), "{name}: startup should be allowed, got {result:?}");
            }
            assert_eq!(
                provider.get_prune_checkpoints().expect("reread checkpoints"),
                before,
                "{name}: validation must not mutate checkpoints"
            );
            assert_eq!(
                provider.best_block_number().expect("reread Finish"),
                tip,
                "{name}: validation must not mutate Finish"
            );
        }
    }

    /// Headers ahead of genesis must not be mistaken for executed progress.
    #[test]
    fn static_headers_ahead_of_finish_do_not_trigger_pruning() {
        let factory = create_test_provider_factory();
        let headers = (0..=10)
            .map(|number| SealedHeader::seal_slow(Header { number, ..Default::default() }))
            .collect::<Vec<_>>();
        insert_headers(&factory, &headers);
        let provider = factory.provider().expect("read fixture");
        assert_eq!(
            provider.last_block_number().expect("read header tip"),
            10,
            "fixture headers must be ahead"
        );
        assert_eq!(
            provider.best_block_number().expect("read Finish"),
            0,
            "fixture executed tip must remain genesis"
        );
        let mut prune = PruneConfig { minimum_pruning_distance: 0, ..Default::default() };
        prune.segments.account_history = Some(PruneMode::Distance(5));
        PruneGuardedComponents::<()>::validate(&provider, &prune, 0)
            .expect("header-only progress must not create history prune backlog");
        assert_eq!(
            provider.get_prune_checkpoints().expect("read checkpoints"),
            vec![],
            "validation must leave missing checkpoints untouched"
        );
    }

    /// Rejects a full snapshot whose historical-state retention is shortened from 31 to 20 days.
    #[test]
    fn shorter_retention_snapshot_refuses_startup() {
        let factory = create_test_provider_factory();
        let provider = factory.provider_rw().expect("open fixture writer");
        provider
            .save_stage_checkpoint(StageId::Finish, StageCheckpoint::new(30_000_000))
            .expect("persist executed tip");
        for segment in [PruneSegment::AccountHistory, PruneSegment::StorageHistory] {
            provider
                .save_prune_checkpoint(segment, PruneCheckpoint::pruned_through(16_608_000))
                .expect("persist 31-day checkpoint");
        }
        provider.commit().expect("commit fixture");
        let mut prune = PruneConfig::default();
        prune.segments.account_history = Some(PruneMode::Distance(8_640_000));
        prune.segments.storage_history = Some(PruneMode::Distance(8_640_000));
        let provider = factory.provider().expect("open read-only fixture");
        let error = PruneGuardedComponents::<()>::validate(&provider, &prune, 160)
            .expect_err("startup must refuse the 4,752,000-block historical-state prune backlog");
        assert_eq!(
            error.to_string(),
            "historical-state prune backlog exceeds startup allowance: segment=AccountHistory executed_tip=30000000 checkpoint=Some(16608000) target=21360000 backlog=4752000 allowance=160; stop the node and run `base reth prune` offline with the same --chain <CHAIN_OR_CUSTOM_SPEC>, --datadir <DATADIR>, --config <CONFIG>, and any custom static-files and RocksDB paths, then restart",
            "diagnostic must identify the backlog and offline recovery"
        );
        for segment in [PruneSegment::AccountHistory, PruneSegment::StorageHistory] {
            assert_eq!(
                provider.get_prune_checkpoint(segment).expect("read checkpoint"),
                Some(PruneCheckpoint::pruned_through(16_608_000)),
                "startup validation must not change either checkpoint"
            );
        }
        assert_eq!(
            provider.best_block_number().expect("read executed tip"),
            30_000_000,
            "startup validation must not change Finish"
        );
    }
}
