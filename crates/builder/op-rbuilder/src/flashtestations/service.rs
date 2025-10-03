use std::sync::Arc;

use alloy_primitives::U256;
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
    tx_signer::{Signer, generate_ethereum_keypair},
};

use super::{
    args::FlashtestationsArgs,
    attestation::{AttestationConfig, AttestationProvider, get_attestation_provider},
    tx_manager::TxManager,
};

#[derive(Clone)]
pub struct FlashtestationsService {
    // Attestation provider generating attestations
    attestation_provider: Arc<Box<dyn AttestationProvider + Send + Sync + 'static>>,
    // Handles the onchain attestation and TEE block building proofs
    tx_manager: TxManager,
    // TEE service generated key
    tee_service_signer: Signer,
    // Funding amount for the TEE signer
    funding_amount: U256,
}

// TODO: FlashtestationsService error types
impl FlashtestationsService {
    pub fn new(args: FlashtestationsArgs) -> Self {
        let (private_key, public_key, address) = generate_ethereum_keypair();
        let tee_service_signer = Signer {
            address,
            pubkey: public_key,
            secret: private_key,
        };

        let attestation_provider = Arc::new(get_attestation_provider(AttestationConfig {
            debug: args.debug,
            quote_provider: args.quote_provider,
        }));

        let tx_manager = TxManager::new(
            tee_service_signer,
            args.funding_key
                .expect("funding key required when flashtestations enabled"),
            args.rpc_url
                .expect("external rpc url required when flashtestations enabled"),
            args.registry_address
                .expect("registry address required when flashtestations enabled"),
            args.builder_policy_address
                .expect("builder policy address required when flashtestations enabled"),
            args.builder_proof_version,
        );

        Self {
            attestation_provider,
            tx_manager,
            tee_service_signer,
            funding_amount: args.funding_amount,
        }
    }

    pub async fn bootstrap(&self) -> eyre::Result<()> {
        // Prepare report data with public key (64 bytes, no 0x04 prefix)
        let mut report_data = [0u8; 64];
        let pubkey_uncompressed = self.tee_service_signer.pubkey.serialize_uncompressed();
        report_data.copy_from_slice(&pubkey_uncompressed[1..65]); // Skip 0x04 prefix

        // Request TDX attestation
        info!(target: "flashtestations", "requesting TDX attestation");
        let attestation = self.attestation_provider.get_attestation(report_data)?;

        // Submit report onchain by registering the key of the tee service
        self.tx_manager
            .fund_and_register_tee_service(attestation, self.funding_amount)
            .await
    }

    pub async fn clean_up(&self) -> eyre::Result<()> {
        self.tx_manager.clean_up().await
    }
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

pub async fn bootstrap_flashtestations<Node>(
    args: FlashtestationsArgs,
    ctx: &BuilderContext<Node>,
) -> eyre::Result<FlashtestationsBuilderTx>
where
    Node: NodeBounds,
{
    info!("Flashtestations enabled");

    let flashtestations_service = FlashtestationsService::new(args.clone());
    // Generates new key and registers the attestation onchain
    flashtestations_service.bootstrap().await?;

    let flashtestations_clone = flashtestations_service.clone();
    ctx.task_executor()
        .spawn_critical_with_graceful_shutdown_signal(
            "flashtestations clean up task",
            |shutdown| {
                Box::pin(async move {
                    let graceful_guard = shutdown.await;
                    if let Err(e) = flashtestations_clone.clean_up().await {
                        warn!(
                            error = %e,
                            "Failed to complete clean up for flashtestations service",
                        )
                    };
                    drop(graceful_guard)
                })
            },
        );

    Ok(FlashtestationsBuilderTx {})
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use secp256k1::{PublicKey, Secp256k1, SecretKey};
    use sha3::{Digest, Keccak256};

    use crate::tx_signer::public_key_to_address;

    /// Derives Ethereum address from report data using the same logic as the Solidity contract
    fn derive_ethereum_address_from_report_data(pubkey_64_bytes: &[u8]) -> Address {
        // This exactly matches the Solidity implementation:
        // address(uint160(uint256(keccak256(reportData))))

        // Step 1: keccak256(reportData)
        let hash = Keccak256::digest(pubkey_64_bytes);

        // Step 2: Take last 20 bytes (same as uint256 -> uint160 conversion)
        let mut address_bytes = [0u8; 20];
        address_bytes.copy_from_slice(&hash[12..32]);

        Address::from(address_bytes)
    }

    #[test]
    fn test_address_derivation_matches() {
        // Test that our manual derivation is correct
        let secp = Secp256k1::new();
        let private_key = SecretKey::from_slice(&[0x01; 32]).unwrap();
        let public_key = PublicKey::from_secret_key(&secp, &private_key);

        // Get address using our implementation
        let our_address = public_key_to_address(&public_key);

        // Get address using our manual derivation (matching Solidity)
        let pubkey_bytes = public_key.serialize_uncompressed();
        let report_data = &pubkey_bytes[1..65]; // Skip 0x04 prefix
        let manual_address = derive_ethereum_address_from_report_data(report_data);

        assert_eq!(
            our_address, manual_address,
            "Address derivation should match"
        );
    }
}
