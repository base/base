//! Contains the [`AccountAbstractionExtension`] which wires up the account abstraction
//! RPC surfaces on the Base node builder.

use std::sync::Arc;

use base_account_abstraction_indexer::UserOperationStorage;
use base_client_node::{BaseNodeExtension, FromExtensionConfig, OpBuilder};
use tracing::info;

use crate::{
    AccountAbstractionApiImpl, AccountAbstractionApiServer, AccountAbstractionArgs,
    BaseAccountAbstractionApiImpl, BaseAccountAbstractionApiServer,
};

/// Configuration for the Account Abstraction extension.
#[derive(Debug, Clone)]
pub struct AccountAbstractionConfig {
    /// CLI arguments for account abstraction
    pub args: AccountAbstractionArgs,
}

impl From<AccountAbstractionArgs> for AccountAbstractionConfig {
    fn from(args: AccountAbstractionArgs) -> Self {
        Self { args }
    }
}

/// Helper struct that wires the account abstraction RPC into the node builder.
#[derive(Debug, Clone)]
pub struct AccountAbstractionExtension {
    /// Configuration for the extension
    config: AccountAbstractionConfig,
}

impl AccountAbstractionExtension {
    /// Creates a new account abstraction extension.
    pub fn new(config: AccountAbstractionConfig) -> Self {
        Self { config }
    }
}

impl BaseNodeExtension for AccountAbstractionExtension {
    /// Applies the extension to the supplied builder.
    fn apply(self: Box<Self>, builder: OpBuilder) -> OpBuilder {
        let args = self.config.args.clone();

        if !args.enabled {
            return builder;
        }

        // Validate configuration
        if let Err(e) = args.validate() {
            tracing::error!(error = %e, "Account Abstraction configuration validation failed");
            return builder;
        }

        // Create shared storage for the indexer ExEx
        let storage = if args.indexer_enabled {
            Some(Arc::new(UserOperationStorage::new()))
        } else {
            None
        };

        let storage_for_exex = storage.clone();
        let indexer_enabled = args.indexer_enabled;

        // Install the ExEx if indexer is enabled
        let builder = builder.install_exex_if(indexer_enabled, "aa-indexer", move |ctx| async move {
            let storage = storage_for_exex.expect("storage should be set when indexer is enabled");
            info!(target: "aa", "Starting Account Abstraction UserOperation Indexer ExEx");
            Ok(base_account_abstraction_indexer::account_abstraction_indexer_exex(ctx, storage))
        });

        // Install RPC extensions
        let tips_url = args.send_url();
        let args_clone = args.clone();

        builder.extend_rpc_modules(move |ctx| {
            info!(target: "aa", "Starting Account Abstraction RPC");

            // Create the main eth_ and base_ RPC implementations
            let aa_api = AccountAbstractionApiImpl::new(
                ctx.provider().clone(),
                ctx.registry.eth_api().clone(),
                tips_url.clone(),
                storage.clone(),
                &args_clone,
            );

            let base_aa_api = BaseAccountAbstractionApiImpl::new(
                ctx.provider().clone(),
                ctx.registry.eth_api().clone(),
            );

            // Merge RPC modules
            ctx.modules.merge_configured(aa_api.into_rpc())?;
            ctx.modules.merge_configured(base_aa_api.into_rpc())?;

            info!(
                target: "aa",
                indexer_enabled = args_clone.indexer_enabled,
                debug = args_clone.debug,
                "Account Abstraction RPC enabled"
            );

            Ok(())
        })
    }
}

impl FromExtensionConfig for AccountAbstractionExtension {
    type Config = AccountAbstractionConfig;

    fn from_config(config: Self::Config) -> Self {
        Self::new(config)
    }
}
