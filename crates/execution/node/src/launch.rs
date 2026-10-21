//! Base's node launch policy.

use reth_node_core::args::EngineArgs;

/// Base's launch policy for the execution node.
///
/// The policy is applied at every boundary where a Base node's launch configuration is finalised:
/// the unified CLI's execution-node arguments, the standalone node and builder command, the
/// in-process system-test node configurations, and Base's shared node-builder finalisation.
#[derive(Debug, Clone, Copy, Default)]
pub struct BaseLaunchPolicy;

impl BaseLaunchPolicy {
    /// Forces serial block import on the given engine arguments.
    ///
    /// reth's BAL-parallel ordered commit applies only Ethereum's block gas tracker, so it skips
    /// Base's cumulative post-refund gas ledger, the Jovian DA budget and the EIP-8130 payer-auth
    /// reservation: the parallel path accepts blocks serial import rejects (DA budget) and rejects
    /// blocks serial import accepts (gas refunds). The block access list is still validated on the
    /// serial path. This is forced rather than defaulted so the divergent path cannot be enabled
    /// from configuration; a Base-aware ordered admission hook is the follow-up that would let it
    /// be re-enabled.
    pub const fn force_serial_import(engine: &mut EngineArgs) {
        engine.bal_parallel_execution_disabled = true;
    }
}
