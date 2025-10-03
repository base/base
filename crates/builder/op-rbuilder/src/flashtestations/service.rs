use alloy_primitives::{Bytes, keccak256};
use reth_node_builder::BuilderContext;
use reth_provider::StateProvider;
use reth_revm::State;
use revm::Database;
use std::fmt::Debug;
use tracing::{info, warn};

use crate::{
    builders::{
        BuilderTransactionCtx, BuilderTransactionError, BuilderTransactions, OpPayloadBuilderCtx,
    },
    primitives::reth::ExecutionInfo,
    traits::NodeBounds,
    tx_signer::{Signer, generate_ethereum_keypair, generate_key_from_seed},
};

use super::{
    args::FlashtestationsArgs,
    attestation::{AttestationConfig, get_attestation_provider},
    tx_manager::TxManager,
};

pub async fn bootstrap_flashtestations<Node>(
    args: FlashtestationsArgs,
    ctx: &BuilderContext<Node>,
) -> eyre::Result<FlashtestationsBuilderTx>
where
    Node: NodeBounds,
{
    let (private_key, public_key, address) = if args.debug {
        info!("Flashtestations debug mode enabled, generating debug key");
        // Generate deterministic key for debugging purposes
        generate_key_from_seed(&args.debug_tee_key_seed)
    } else {
        generate_ethereum_keypair()
    };

    info!("Flashtestations key generated: {}", address);

    let tee_service_signer = Signer {
        address,
        pubkey: public_key,
        secret: private_key,
    };

    let funding_key = args
        .funding_key
        .expect("funding key required when flashtestations enabled");
    let registry_address = args
        .registry_address
        .expect("registry address required when flashtestations enabled");
    let _builder_policy_address = args
        .builder_policy_address
        .expect("builder policy address required when flashtestations enabled");

    let attestation_provider = get_attestation_provider(AttestationConfig {
        debug: args.debug,
        quote_provider: args.quote_provider,
    });

    // Prepare report data:
    // - TEE address (20 bytes) at reportData[0:20]
    // - Extended registration data hash (32 bytes) at reportData[20:52]
    // - Total: 52 bytes, padded to 64 bytes with zeros

    // Extract TEE address as 20 bytes
    let tee_address_bytes: [u8; 20] = tee_service_signer.address.into();

    // Calculate keccak256 hash of empty bytes (32 bytes)
    let ext_data = Bytes::from(b"");
    let ext_data_hash = keccak256(&ext_data);

    // Create 64-byte report data array
    let mut report_data = [0u8; 64];

    // Copy TEE address (20 bytes) to positions 0-19
    report_data[0..20].copy_from_slice(&tee_address_bytes);

    // Copy extended registration data hash (32 bytes) to positions 20-51
    report_data[20..52].copy_from_slice(ext_data_hash.as_ref());

    // Request TDX attestation
    info!(target: "flashtestations", "requesting TDX attestation");
    let attestation = attestation_provider.get_attestation(report_data).await?;

    #[allow(dead_code)]
    let (tx_manager, _registered) = if let Some(rpc_url) = args.rpc_url {
        let tx_manager = TxManager::new(
            tee_service_signer,
            funding_key,
            rpc_url.clone(),
            registry_address,
        );
        // Submit report onchain by registering the key of the tee service
        match tx_manager
            .fund_and_register_tee_service(
                attestation.clone(),
                ext_data.clone(),
                args.funding_amount,
            )
            .await
        {
            Ok(_) => (Some(tx_manager), true),
            Err(e) => {
                warn!(error = %e, "Failed to register tee service via rpc");
                (Some(tx_manager), false)
            }
        }
    } else {
        (None, false)
    };

    let flashtestations_builder_tx = FlashtestationsBuilderTx {};

    ctx.task_executor()
        .spawn_critical_with_graceful_shutdown_signal(
            "flashtestations clean up task",
            |shutdown| {
                Box::pin(async move {
                    let graceful_guard = shutdown.await;
                    if let Some(tx_manager) = tx_manager {
                        if let Err(e) = tx_manager.clean_up().await {
                            warn!(
                                error = %e,
                                "Failed to complete clean up for flashtestations service",
                            );
                        }
                    }
                    drop(graceful_guard)
                })
            },
        );

    Ok(flashtestations_builder_tx)
}

#[derive(Debug, Clone)]
pub struct FlashtestationsBuilderTx {}

impl<ExtraCtx: Debug + Default> BuilderTransactions<ExtraCtx> for FlashtestationsBuilderTx {
    fn simulate_builder_txs<Extra: Debug + Default>(
        &self,
        _state_provider: impl StateProvider + Clone,
        _info: &mut ExecutionInfo<Extra>,
        _ctx: &OpPayloadBuilderCtx<ExtraCtx>,
        _db: &mut State<impl Database>,
    ) -> Result<Vec<BuilderTransactionCtx>, BuilderTransactionError> {
        Ok(vec![])
    }
}
