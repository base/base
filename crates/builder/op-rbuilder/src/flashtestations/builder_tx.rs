use alloy_consensus::TxEip1559;
use alloy_eips::Encodable2718;
use alloy_evm::Database;
use alloy_op_evm::OpEvm;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, keccak256, map::foldhash::HashMap};
use alloy_sol_types::{Error, SolCall, SolEvent, SolInterface, SolValue};
use core::fmt::Debug;
use op_alloy_consensus::OpTypedTransaction;
use reth_evm::{ConfigureEvm, Evm, EvmError, precompiles::PrecompilesMap};
use reth_optimism_primitives::OpTransactionSigned;
use reth_primitives::{Log, Recovered};
use reth_provider::StateProvider;
use reth_revm::{State, database::StateProviderDatabase};
use revm::{
    DatabaseCommit,
    context::result::{ExecutionResult, ResultAndState},
    inspector::NoOpInspector,
    state::Account,
};
use std::sync::OnceLock;
use tracing::{debug, info};

use crate::{
    builders::{
        BuilderTransactionCtx, BuilderTransactionError, BuilderTransactions, OpPayloadBuilderCtx,
        get_balance, get_nonce,
    },
    flashtestations::{
        BlockData, FlashtestationRevertReason,
        IBlockBuilderPolicy::{self, BlockBuilderProofVerified},
        IFlashtestationRegistry::{self, TEEServiceRegistered},
    },
    primitives::reth::ExecutionInfo,
    tx_signer::Signer,
};

pub struct FlashtestationsBuilderTxArgs {
    pub attestation: Vec<u8>,
    pub extra_registration_data: Bytes,
    pub tee_service_signer: Signer,
    pub funding_key: Signer,
    pub funding_amount: U256,
    pub registry_address: Address,
    pub builder_policy_address: Address,
    pub builder_proof_version: u8,
    pub enable_block_proofs: bool,
    pub registered: bool,
}

#[derive(Debug, Clone)]
pub struct FlashtestationsBuilderTx {
    // Attestation for the builder
    attestation: Vec<u8>,
    // Extra registration data for the builder
    extra_registration_data: Bytes,
    // TEE service generated key
    tee_service_signer: Signer,
    // Funding key for the TEE signer
    funding_key: Signer,
    // Funding amount for the TEE signer
    funding_amount: U256,
    // Registry address for the attestation
    registry_address: Address,
    // Builder policy address for the block builder proof
    builder_policy_address: Address,
    // Builder proof version
    builder_proof_version: u8,
    // Whether the workload and address has been registered
    registered: OnceLock<bool>,
    // Whether block proofs are enabled
    enable_block_proofs: bool,
}

#[derive(Debug, Default)]
pub struct TxSimulateResult {
    pub gas_used: u64,
    pub success: bool,
    pub state_changes: HashMap<Address, Account>,
    pub revert_reason: Option<FlashtestationRevertReason>,
    pub logs: Vec<Log>,
}

impl FlashtestationsBuilderTx {
    pub fn new(args: FlashtestationsBuilderTxArgs) -> Self {
        Self {
            attestation: args.attestation,
            extra_registration_data: args.extra_registration_data,
            tee_service_signer: args.tee_service_signer,
            funding_key: args.funding_key,
            funding_amount: args.funding_amount,
            registry_address: args.registry_address,
            builder_policy_address: args.builder_policy_address,
            builder_proof_version: args.builder_proof_version,
            registered: OnceLock::new(),
            enable_block_proofs: args.enable_block_proofs,
        }
    }

    fn signed_funding_tx(
        &self,
        to: Address,
        from: Signer,
        amount: U256,
        base_fee: u64,
        chain_id: u64,
        nonce: u64,
    ) -> Result<Recovered<OpTransactionSigned>, secp256k1::Error> {
        // Create the EIP-1559 transaction
        let tx = OpTypedTransaction::Eip1559(TxEip1559 {
            chain_id,
            nonce,
            gas_limit: 21000,
            max_fee_per_gas: base_fee.into(),
            max_priority_fee_per_gas: 0,
            to: TxKind::Call(to),
            value: amount,
            ..Default::default()
        });
        from.sign_tx(tx)
    }

    fn signed_register_tee_service_tx(
        &self,
        attestation: Vec<u8>,
        gas_limit: u64,
        base_fee: u64,
        chain_id: u64,
        nonce: u64,
    ) -> Result<Recovered<OpTransactionSigned>, secp256k1::Error> {
        let quote_bytes = Bytes::from(attestation);
        let calldata = IFlashtestationRegistry::registerTEEServiceCall {
            rawQuote: quote_bytes,
            extendedRegistrationData: self.extra_registration_data.clone(),
        }
        .abi_encode();

        // Create the EIP-1559 transaction
        let tx = OpTypedTransaction::Eip1559(TxEip1559 {
            chain_id,
            nonce,
            gas_limit,
            max_fee_per_gas: base_fee.into(),
            max_priority_fee_per_gas: 0,
            to: TxKind::Call(self.registry_address),
            input: calldata.into(),
            ..Default::default()
        });
        self.tee_service_signer.sign_tx(tx)
    }

    fn signed_block_builder_proof_tx<ExtraCtx: Debug + Default>(
        &self,
        block_content_hash: B256,
        ctx: &OpPayloadBuilderCtx<ExtraCtx>,
        gas_limit: u64,
        nonce: u64,
    ) -> Result<Recovered<OpTransactionSigned>, secp256k1::Error> {
        let calldata = IBlockBuilderPolicy::verifyBlockBuilderProofCall {
            version: self.builder_proof_version,
            blockContentHash: block_content_hash,
        }
        .abi_encode();
        // Create the EIP-1559 transaction
        let tx = OpTypedTransaction::Eip1559(TxEip1559 {
            chain_id: ctx.chain_id(),
            nonce,
            gas_limit,
            max_fee_per_gas: ctx.base_fee().into(),
            max_priority_fee_per_gas: 0,
            to: TxKind::Call(self.builder_policy_address),
            input: calldata.into(),
            ..Default::default()
        });
        self.tee_service_signer.sign_tx(tx)
    }

    /// Computes the block content hash according to the formula:
    /// keccak256(abi.encode(parentHash, blockNumber, timestamp, transactionHashes))
    /// https://github.com/flashbots/rollup-boost/blob/main/specs/flashtestations.md#block-building-process
    fn compute_block_content_hash(
        transactions: Vec<OpTransactionSigned>,
        parent_hash: B256,
        block_number: u64,
        timestamp: u64,
    ) -> B256 {
        // Create ordered list of transaction hashes
        let transaction_hashes: Vec<B256> = transactions
            .iter()
            .map(|tx| {
                // RLP encode the transaction and hash it
                let mut encoded = Vec::new();
                tx.encode_2718(&mut encoded);
                keccak256(&encoded)
            })
            .collect();

        // Create struct and ABI encode
        let block_data = BlockData {
            parentHash: parent_hash,
            blockNumber: U256::from(block_number),
            timestamp: U256::from(timestamp),
            transactionHashes: transaction_hashes,
        };

        let encoded = block_data.abi_encode();
        keccak256(&encoded)
    }

    fn simulate_register_tee_service_tx<ExtraCtx: Debug + Default>(
        &self,
        ctx: &OpPayloadBuilderCtx<ExtraCtx>,
        evm: &mut OpEvm<
            &mut State<StateProviderDatabase<impl StateProvider>>,
            NoOpInspector,
            PrecompilesMap,
        >,
    ) -> Result<TxSimulateResult, BuilderTransactionError> {
        let nonce = get_nonce(evm.db_mut(), self.tee_service_signer.address)?;

        let register_tx = self.signed_register_tee_service_tx(
            self.attestation.clone(),
            ctx.block_gas_limit(),
            ctx.base_fee(),
            ctx.chain_id(),
            nonce,
        )?;
        let ResultAndState { result, state } = match evm.transact(&register_tx) {
            Ok(res) => res,
            Err(err) => {
                if err.is_invalid_tx_err() {
                    return Err(BuilderTransactionError::InvalidTransactionError(Box::new(
                        err,
                    )));
                } else {
                    return Err(BuilderTransactionError::EvmExecutionError(Box::new(err)));
                }
            }
        };
        match result {
            ExecutionResult::Success { gas_used, logs, .. } => Ok(TxSimulateResult {
                gas_used,
                success: true,
                state_changes: state,
                revert_reason: None,
                logs,
            }),
            ExecutionResult::Revert { output, gas_used } => {
                let revert_reason =
                    IFlashtestationRegistry::IFlashtestationRegistryErrors::abi_decode(&output)
                        .map(FlashtestationRevertReason::FlashtestationRegistry)
                        .unwrap_or_else(|e| {
                            FlashtestationRevertReason::Unknown(hex::encode(output), e)
                        });
                Ok(TxSimulateResult {
                    gas_used,
                    success: false,
                    state_changes: state,
                    revert_reason: Some(revert_reason),
                    logs: vec![],
                })
            }
            ExecutionResult::Halt { reason, .. } => Ok(TxSimulateResult {
                gas_used: 0,
                success: false,
                state_changes: state,
                revert_reason: Some(FlashtestationRevertReason::Halt(reason)),
                logs: vec![],
            }),
        }
    }

    fn check_tee_address_registered_log(&self, logs: &[Log], address: Address) -> bool {
        for log in logs {
            if log.topics().first() == Some(&TEEServiceRegistered::SIGNATURE_HASH) {
                if let Ok(decoded) = TEEServiceRegistered::decode_log(log) {
                    if decoded.teeAddress == address {
                        return true;
                    }
                };
            }
        }
        false
    }

    fn simulate_verify_block_proof_tx<ExtraCtx: Debug + Default>(
        &self,
        block_content_hash: B256,
        ctx: &OpPayloadBuilderCtx<ExtraCtx>,
        evm: &mut OpEvm<
            &mut State<StateProviderDatabase<impl StateProvider>>,
            NoOpInspector,
            PrecompilesMap,
        >,
    ) -> Result<TxSimulateResult, BuilderTransactionError> {
        let nonce = get_nonce(evm.db_mut(), self.tee_service_signer.address)?;

        let verify_block_proof_tx = self.signed_block_builder_proof_tx(
            block_content_hash,
            ctx,
            ctx.block_gas_limit(),
            nonce,
        )?;
        let ResultAndState { result, state } = match evm.transact(&verify_block_proof_tx) {
            Ok(res) => res,
            Err(err) => {
                if err.is_invalid_tx_err() {
                    return Err(BuilderTransactionError::InvalidTransactionError(Box::new(
                        err,
                    )));
                } else {
                    return Err(BuilderTransactionError::EvmExecutionError(Box::new(err)));
                }
            }
        };
        match result {
            ExecutionResult::Success { gas_used, logs, .. } => Ok(TxSimulateResult {
                gas_used,
                success: true,
                state_changes: state,
                revert_reason: None,
                logs,
            }),
            ExecutionResult::Revert { output, gas_used } => {
                let revert_reason =
                    IBlockBuilderPolicy::IBlockBuilderPolicyErrors::abi_decode(&output)
                        .map(FlashtestationRevertReason::BlockBuilderPolicy)
                        .unwrap_or_else(|e| {
                            FlashtestationRevertReason::Unknown(hex::encode(output), e)
                        });
                Ok(TxSimulateResult {
                    gas_used,
                    success: false,
                    state_changes: state,
                    revert_reason: Some(revert_reason),
                    logs: vec![],
                })
            }
            ExecutionResult::Halt { reason, .. } => Ok(TxSimulateResult {
                gas_used: 0,
                success: false,
                state_changes: state,
                revert_reason: Some(FlashtestationRevertReason::Halt(reason)),
                logs: vec![],
            }),
        }
    }

    fn check_verify_block_proof_log(&self, logs: &[Log]) -> bool {
        for log in logs {
            if log.topics().first() == Some(&BlockBuilderProofVerified::SIGNATURE_HASH) {
                return true;
            }
        }
        false
    }

    fn fund_tee_service_tx<ExtraCtx: Debug + Default>(
        &self,
        ctx: &OpPayloadBuilderCtx<ExtraCtx>,
        evm: &mut OpEvm<
            &mut State<StateProviderDatabase<impl StateProvider>>,
            NoOpInspector,
            PrecompilesMap,
        >,
    ) -> Result<Option<BuilderTransactionCtx>, BuilderTransactionError> {
        let balance = get_balance(evm.db_mut(), self.tee_service_signer.address)?;
        if balance.is_zero() {
            let funding_nonce = get_nonce(evm.db_mut(), self.funding_key.address)?;
            let funding_tx = self.signed_funding_tx(
                self.tee_service_signer.address,
                self.funding_key,
                self.funding_amount,
                ctx.base_fee(),
                ctx.chain_id(),
                funding_nonce,
            )?;
            let da_size =
                op_alloy_flz::tx_estimated_size_fjord_bytes(funding_tx.encoded_2718().as_slice());
            let ResultAndState { state, .. } = match evm.transact(&funding_tx) {
                Ok(res) => res,
                Err(err) => {
                    if err.is_invalid_tx_err() {
                        return Err(BuilderTransactionError::InvalidTransactionError(Box::new(
                            err,
                        )));
                    } else {
                        return Err(BuilderTransactionError::EvmExecutionError(Box::new(err)));
                    }
                }
            };
            info!(target: "flashtestations", block_number = ctx.block_number(), tx_hash = ?funding_tx.tx_hash(), "adding funding tx to builder txs");
            evm.db_mut().commit(state);
            Ok(Some(BuilderTransactionCtx {
                gas_used: 21000,
                da_size,
                signed_tx: funding_tx,
                is_top_of_block: false,
            }))
        } else {
            Ok(None)
        }
    }

    fn register_tee_service_tx<ExtraCtx: Debug + Default>(
        &self,
        ctx: &OpPayloadBuilderCtx<ExtraCtx>,
        evm: &mut OpEvm<
            &mut State<StateProviderDatabase<impl StateProvider>>,
            NoOpInspector,
            PrecompilesMap,
        >,
    ) -> Result<(Option<BuilderTransactionCtx>, bool), BuilderTransactionError> {
        let TxSimulateResult {
            gas_used,
            success,
            state_changes,
            revert_reason,
            logs,
        } = self.simulate_register_tee_service_tx(ctx, evm)?;
        if success {
            if !self.check_tee_address_registered_log(&logs, self.tee_service_signer.address) {
                Err(BuilderTransactionError::other(
                    FlashtestationRevertReason::LogMismatch(
                        self.registry_address,
                        TEEServiceRegistered::SIGNATURE_HASH,
                    ),
                ))
            } else {
                let nonce = get_nonce(evm.db_mut(), self.tee_service_signer.address)?;
                let register_tx = self.signed_register_tee_service_tx(
                    self.attestation.clone(),
                    gas_used * 64 / 63, // Due to EIP-150, 63/64 of available gas is forwarded to external calls so need to add a buffer
                    ctx.base_fee(),
                    ctx.chain_id(),
                    nonce,
                )?;
                let da_size = op_alloy_flz::tx_estimated_size_fjord_bytes(
                    register_tx.encoded_2718().as_slice(),
                );
                info!(target: "flashtestations", block_number = ctx.block_number(), tx_hash = ?register_tx.tx_hash(), "adding register tee tx to builder txs");
                evm.db_mut().commit(state_changes);
                Ok((
                    Some(BuilderTransactionCtx {
                        gas_used,
                        da_size,
                        signed_tx: register_tx,
                        is_top_of_block: false,
                    }),
                    false,
                ))
            }
        } else if let Some(FlashtestationRevertReason::FlashtestationRegistry(
            IFlashtestationRegistry::IFlashtestationRegistryErrors::TEEServiceAlreadyRegistered(_),
        )) = revert_reason
        {
            Ok((None, true))
        } else {
            Err(BuilderTransactionError::other(revert_reason.unwrap_or(
                FlashtestationRevertReason::Unknown(
                    "unknown revert".into(),
                    Error::Other("unknown revert".into()),
                ),
            )))
        }
    }

    fn verify_block_proof_tx<ExtraCtx: Debug + Default>(
        &self,
        transactions: Vec<OpTransactionSigned>,
        ctx: &OpPayloadBuilderCtx<ExtraCtx>,
        evm: &mut OpEvm<
            &mut State<StateProviderDatabase<impl StateProvider>>,
            NoOpInspector,
            PrecompilesMap,
        >,
    ) -> Result<Option<BuilderTransactionCtx>, BuilderTransactionError> {
        let block_content_hash = Self::compute_block_content_hash(
            transactions.clone(),
            ctx.parent_hash(),
            ctx.block_number(),
            ctx.timestamp(),
        );

        let TxSimulateResult {
            gas_used,
            success,
            revert_reason,
            logs,
            ..
        } = self.simulate_verify_block_proof_tx(block_content_hash, ctx, evm)?;
        if success {
            if !self.check_verify_block_proof_log(&logs) {
                Err(BuilderTransactionError::other(
                    FlashtestationRevertReason::LogMismatch(
                        self.builder_policy_address,
                        BlockBuilderProofVerified::SIGNATURE_HASH,
                    ),
                ))
            } else {
                let nonce = get_nonce(evm.db_mut(), self.tee_service_signer.address)?;
                // Due to EIP-150, only 63/64 of available gas is forwarded to external calls so need to add a buffer
                let verify_block_proof_tx = self.signed_block_builder_proof_tx(
                    block_content_hash,
                    ctx,
                    gas_used * 64 / 63,
                    nonce,
                )?;
                let da_size = op_alloy_flz::tx_estimated_size_fjord_bytes(
                    verify_block_proof_tx.encoded_2718().as_slice(),
                );
                debug!(target: "flashtestations", block_number = ctx.block_number(), tx_hash = ?verify_block_proof_tx.tx_hash(), "adding verify block proof tx to builder txs");
                Ok(Some(BuilderTransactionCtx {
                    gas_used,
                    da_size,
                    signed_tx: verify_block_proof_tx,
                    is_top_of_block: false,
                }))
            }
        } else {
            Err(BuilderTransactionError::other(revert_reason.unwrap_or(
                FlashtestationRevertReason::Unknown(
                    "unknown revert".into(),
                    Error::Other("unknown revert".into()),
                ),
            )))
        }
    }

    fn set_registered<ExtraCtx: Debug + Default>(
        &self,
        state_provider: impl StateProvider + Clone,
        ctx: &OpPayloadBuilderCtx<ExtraCtx>,
    ) -> Result<(), BuilderTransactionError> {
        let state = StateProviderDatabase::new(state_provider.clone());
        let mut simulation_state = State::builder()
            .with_database(state)
            .with_bundle_update()
            .build();
        let mut evm = ctx
            .evm_config
            .evm_with_env(&mut simulation_state, ctx.evm_env.clone());
        evm.modify_cfg(|cfg| {
            cfg.disable_balance_check = true;
        });
        match self.register_tee_service_tx(ctx, &mut evm) {
            Ok((_, registered)) => {
                if registered {
                    let _ = self.registered.set(registered);
                }
                Ok(())
            }
            Err(e) => Err(BuilderTransactionError::other(e)),
        }
    }
}

impl<ExtraCtx: Debug + Default> BuilderTransactions<ExtraCtx> for FlashtestationsBuilderTx {
    fn simulate_builder_txs<Extra: Debug + Default>(
        &self,
        state_provider: impl StateProvider + Clone,
        info: &mut ExecutionInfo<Extra>,
        ctx: &OpPayloadBuilderCtx<ExtraCtx>,
        db: &mut State<impl Database>,
    ) -> Result<Vec<BuilderTransactionCtx>, BuilderTransactionError> {
        // set registered simulating against the committed state
        if !self.registered.get().unwrap_or(&false) {
            self.set_registered(state_provider.clone(), ctx)?;
        }

        let state = StateProviderDatabase::new(state_provider.clone());
        let mut simulation_state = State::builder()
            .with_database(state)
            .with_bundle_prestate(db.bundle_state.clone())
            .with_bundle_update()
            .build();

        let mut evm = ctx
            .evm_config
            .evm_with_env(&mut simulation_state, ctx.evm_env.clone());
        evm.modify_cfg(|cfg| {
            cfg.disable_balance_check = true;
        });

        let mut builder_txs = Vec::<BuilderTransactionCtx>::new();

        if !self.registered.get().unwrap_or(&false) {
            info!(target: "flashtestations", "tee service not registered yet, attempting to register");
            builder_txs.extend(self.fund_tee_service_tx(ctx, &mut evm)?);
            let (register_tx, _) = self.register_tee_service_tx(ctx, &mut evm)?;
            builder_txs.extend(register_tx);
        }

        if self.enable_block_proofs {
            // add verify block proof tx
            builder_txs.extend(self.verify_block_proof_tx(
                info.executed_transactions.clone(),
                ctx,
                &mut evm,
            )?);
        }
        Ok(builder_txs)
    }
}
