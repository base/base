//! C-1: `MevEmitter` ExEx — the non-invasive node hook.
//!
//! Installs a reth Execution Extension that observes `ChainCommitted`
//! notifications on the canonical chain. For C-1 it is a skeleton: it logs
//! committed tips and reports `FinishedHeight` (so the node can prune
//! ExEx-held data), establishing the wiring that the later increments build on:
//! C-2 attaches a revm `Inspector` here to capture per-tx token state-diffs,
//! C-3 folds in Flashblocks, and C-4 streams the encoded events to the TS
//! `ProviderNodeStream` consumer.

use alloy_evm::Evm;
use base_execution_evm::BaseEvmConfig;
use base_node_runner::{BaseNodeAdapter, BaseNodeExtension, FromExtensionConfig, NodeHooks};
use futures::TryStreamExt;
use reth_chainspec::ChainSpecProvider;
use reth_evm::ConfigureEvm;
use reth_exex::{ExExContext, ExExEvent, ExExNotificationsStream};
use reth_provider::StateProviderFactory;
use reth_revm::database::StateProviderDatabase;
use revm::database::State;
use revm::DatabaseCommit;
use tracing::{debug, info};

/// `ExEx` run loop: drain canonical-chain notifications, report `FinishedHeight`.
pub async fn run_mev_emitter_exex(mut ctx: ExExContext<BaseNodeAdapter>) -> eyre::Result<()> {
    // C-2 ①: the EVM configuration used to re-execute committed transactions (the
    // per-tx `EvmState` source for `revm_bridge`). `chain_spec` comes from the
    // ExEx provider; `BaseEvmConfig::base` wires the mainnet receipt builder.
    let evm_config = BaseEvmConfig::base(ctx.provider().chain_spec());
    let registry = crate::state_diff::BalanceSlotRegistry::base_priority();
    info!(target: "base::mev_emitter", "mev-emitter ExEx started");
    ctx.notifications.set_without_head();
    while let Some(notification) = ctx.notifications.try_next().await? {
        if let Some(committed) = notification.committed_chain() {
            for (&block_number, block) in committed.blocks() {
                // C-2 ②: the parent state to re-execute this block's txs against.
                let parent = block_number.saturating_sub(1);
                let state_provider = ctx.provider().history_by_block_number(parent)?;
                let db = StateProviderDatabase::new(state_provider);
                // C-2 ③④: a commit-capable revm State over that DB, and the Base
                // EVM configured for this block's environment.
                let state = State::builder().with_database(db).with_bundle_update().build();
                let evm_env = evm_config.evm_env(block.header())?;
                let mut evm = evm_config.evm_with_env(state, evm_env);
                // Canonical blocks carry no flashblock payloadId (that arrives via
                // C-3); use the block hash as a stable placeholder until C-3 maps
                // the real payloadId.
                let payload_id = format!("{:#x}", block.hash());
                // C-2 ⑤: re-execute each tx, derive per-tx StateDiffEvents from its
                // EvmState + Transfer-log candidates, committing state between txs.
                for tx in block.transactions_recovered() {
                    let tx_env = evm_config.tx_env(tx);
                    let out = evm.transact(tx_env)?;
                    let candidates = crate::candidates::transfer_candidates(
                        out.result.logs().iter().map(|l| l.topics()),
                    );
                    let _events = crate::revm_bridge::state_diffs_from_evm_state(
                        &out.state,
                        &registry,
                        &candidates,
                        tx.tx_hash(),
                        block_number,
                        0,
                        payload_id.clone(),
                    );
                    // TODO(C-4): emit `_events` over the outbound transport.
                    evm.db_mut().commit(out.state);
                }
            }
            let tip = committed.tip().num_hash();
            debug!(
                target: "base::mev_emitter",
                number = tip.number,
                hash = ?tip.hash,
                blocks = committed.blocks().len(),
                "chain committed",
            );
            ctx.events.send(ExExEvent::FinishedHeight(tip))?;
        }
    }
    Ok(())
}

/// Node extension that installs the [`run_mev_emitter_exex`] `ExEx` via
/// [`NodeHooks::install_exex`]. Register with `BaseNodeRunner::install_ext`.
#[derive(Debug)]
pub struct MevEmitterExtension;

impl FromExtensionConfig for MevEmitterExtension {
    type Config = ();

    fn from_config(_config: Self::Config) -> Self {
        Self
    }
}

impl BaseNodeExtension for MevEmitterExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        hooks.install_exex("mev-emitter", move |ctx| async move {
            Ok(run_mev_emitter_exex(ctx))
        })
    }
}
