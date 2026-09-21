//! Typed contract workloads against the managed L2 RPC, never an in-process node.

use eyre::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{DevnetProfile, ForkActivation, ScenarioConfig, WorkloadContext};

mod client;
pub use client::{B20CreateConfig, B20PrecompileClient};

mod registry;
pub use registry::RegistryWorkload;

mod token;
pub use token::TokenWorkload;

/// One isolated contract behavior selected by a declarative scenario.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractCase {
    /// Factory creation, exact creation/transfer logs, and balance changes.
    B20FactoryCreateAndTransferViaRpc,
    /// Token metadata and initial supply.
    B20TokenMetadata,
    /// Approval, delegated transfer, and allowance consumption.
    B20ApproveAndTransferFrom,
    /// Zero/nonzero mint and burn with supply and balance accounting.
    B20MintAndBurn,
    /// Stablecoin currency, supply, and transfer behavior.
    B20StablecoinCreateAndCurrencyViaRpc,
    /// Asset extension operations and authorization.
    B20AssetExtensionViaRpc,
    /// Transfer memo event and balances.
    B20TransferWithMemo,
    /// Supply cap updates and over-cap rejection.
    B20SupplyCap,
    /// Name, symbol, and contract URI updates.
    B20MetadataUpdates,
    /// Pausing and restoring token transfers.
    B20PauseAndUnpause,
    /// Deterministic factory address and token recognition.
    B20FactoryPredictAndIsB20,
    /// Stablecoin variant initialization and metadata.
    B20StablecoinVariantCreateViaRpc,
    /// No precompile execution before Beryl, followed by activation.
    BerylPrecompilesDoNotExecuteBeforeActivationBlock,
    /// Duplicate deterministic deployment rejection.
    B20CreateTokenDuplicateReverts,
    /// Features start inactive.
    ActivationRegistryIsActivatedDefault,
    /// Generated activation administrator identity.
    ActivationRegistryAdmin,
    /// Admin rotation is unavailable before Cobalt.
    ActivationRegistrySetAdminRevertsBeforeCobalt,
    /// Cobalt admin rotation updates authority and emits exact events.
    ActivationRegistryCobaltAdminRotation,
    /// Activation/deactivation events, state, and repeated-call rejection.
    ActivationRegistryAdminLifecycle,
    /// Unauthorized activation leaves state unchanged.
    ActivationRegistryUnauthorizedActivateReverts,
    /// Feature-gated calls fail while inactive and succeed while active.
    ActivationRegistryCheckActivatedGate,
    /// Policy creation and administrator events.
    PolicyRegistryCreatePolicyEmitsEvents,
    /// Built-in, new, and nonexistent policy queries.
    PolicyRegistryPolicyExists,
    /// Policy membership, administrator lifecycle, and invalid operations.
    PolicyRegistryLifecycleAndErrorPaths,
    /// Read behavior and write rejection after policy deactivation.
    PolicyRegistryDeactivatedViewsAndWriteGate,
    /// Adding a sender to an allowlist enables their previously rejected transfer.
    AllowlistGatesTransfer,
    /// Adding a sender to a blocklist rejects their previously allowed transfer.
    BlocklistGatesTransfer,
    /// Built-in always-block policy rejects all token senders.
    AlwaysBlockPolicyBlocksTransfer,
}

impl ContractCase {
    /// Requires the fork regime used by the original test, not merely an enabled fork.
    pub fn validate(&self, config: &ScenarioConfig) -> Result<()> {
        ensure!(
            config.devnet.profile == DevnetProfile::Canonical,
            "contract workloads require the canonical profile"
        );
        let beryl = config.devnet.l2.forks.get("beryl").and_then(ForkActivation::block);
        ensure!(beryl.is_some(), "contract workload requires Beryl");
        let cobalt = config.devnet.l2.forks.get("cobalt").and_then(ForkActivation::block);
        match self {
            Self::ActivationRegistryCobaltAdminRotation => {
                ensure!(cobalt.is_some(), "admin rotation requires Cobalt");
            }
            _ => ensure!(cobalt.is_none(), "this contract workload requires Cobalt disabled"),
        }
        if matches!(self, Self::BerylPrecompilesDoNotExecuteBeforeActivationBlock) {
            ensure!(
                beryl.is_some_and(|block| block > 0),
                "pre-Beryl workload requires a future activation"
            );
        }
        Ok(())
    }

    /// Resolves only the managed builder execution endpoint.
    pub const fn required_roles(&self) -> &'static [&'static str] {
        &["builder"]
    }

    /// Runs the complete typed workload, with the caller enforcing its overall deadline.
    pub async fn execute(&self, context: &WorkloadContext<'_>) -> Result<Value> {
        match self {
            Self::B20FactoryCreateAndTransferViaRpc
            | Self::B20TokenMetadata
            | Self::B20ApproveAndTransferFrom
            | Self::B20MintAndBurn
            | Self::B20StablecoinCreateAndCurrencyViaRpc
            | Self::B20AssetExtensionViaRpc
            | Self::B20TransferWithMemo
            | Self::B20SupplyCap
            | Self::B20MetadataUpdates
            | Self::B20PauseAndUnpause
            | Self::B20FactoryPredictAndIsB20
            | Self::B20StablecoinVariantCreateViaRpc
            | Self::BerylPrecompilesDoNotExecuteBeforeActivationBlock
            | Self::B20CreateTokenDuplicateReverts => TokenWorkload::execute(*self, context).await,
            _ => RegistryWorkload::execute(*self, context).await,
        }
    }
}
