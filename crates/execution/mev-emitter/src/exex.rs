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
use tracing::{debug, info, warn};

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
            let (mut total_diffs, mut total_cands, mut total_trusted) = (0usize, 0usize, 0usize);
            for (&block_number, block) in committed.blocks() {
                // Re-execution is isolated per block: any failure is logged and
                // skipped, NEVER propagated — an ExEx error would otherwise crash
                // the whole node (ExEx is a critical task). The emitter must never
                // be able to take the node down.
                let block_result: eyre::Result<(usize, usize, usize)> = (|| {
                    // C-2 ②: the parent state to re-execute this block's txs against.
                    let parent = block_number.saturating_sub(1);
                    let db = StateProviderDatabase::new(
                        ctx.provider().history_by_block_number(parent)?,
                    );
                    // C-2 ③④: a commit-capable revm State over that DB + the Base
                    // EVM configured for this block's environment.
                    let state =
                        State::builder().with_database(db).with_bundle_update().build();
                    let evm_env = evm_config.evm_env(block.header())?;
                    let mut evm = evm_config.evm_with_env(state, evm_env);
                    // Canonical blocks carry no flashblock payloadId (that arrives
                    // via C-3); use the block hash as a stable placeholder.
                    let payload_id = format!("{:#x}", block.hash());
                    // C-2 ⑤: re-execute each tx; derive per-tx StateDiffEvents from
                    // its EvmState + Transfer-log candidates, committing between txs.
                    let (mut diffs, mut cands, mut trusted) = (0usize, 0usize, 0usize);
                    for tx in block.transactions_recovered() {
                        let out = evm.transact(evm_config.tx_env(tx))?;
                        // Diagnostics: did this tx's EvmState touch a trusted token
                        // contract, and how many Transfer-log candidates did it yield?
                        trusted += out.state.keys().filter(|&a| registry.is_trusted(a)).count();
                        let candidates = crate::candidates::transfer_candidates(
                            out.result.logs().iter().map(|l| l.topics()),
                        );
                        cands += candidates.len();
                        let events = crate::revm_bridge::state_diffs_from_evm_state(
                            &out.state,
                            &registry,
                            &candidates,
                            tx.tx_hash(),
                            block_number,
                            0,
                            payload_id.clone(),
                        );
                        diffs += events.len();
                        // TODO(C-4): emit `events` over the outbound transport.
                        evm.db_mut().commit(out.state);
                    }
                    Ok((diffs, cands, trusted))
                })();
                match block_result {
                    Ok((d, c, t)) => {
                        total_diffs += d;
                        total_cands += c;
                        total_trusted += t;
                    }
                    Err(e) => warn!(
                        target: "base::mev_emitter",
                        block = block_number,
                        error = %e,
                        "block re-execution failed; skipped (node unaffected)",
                    ),
                }
            }
            let tip = committed.tip().num_hash();
            debug!(
                target: "base::mev_emitter",
                number = tip.number,
                blocks = committed.blocks().len(),
                state_diffs = total_diffs,
                candidates = total_cands,
                trusted_touched = total_trusted,
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
