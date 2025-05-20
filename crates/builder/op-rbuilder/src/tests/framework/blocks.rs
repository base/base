use crate::tx_signer::Signer;
use alloy_eips::{eip2718::Encodable2718, BlockNumberOrTag};
use alloy_primitives::{address, hex, Address, Bytes, TxKind, B256, U256};
use alloy_rpc_types_engine::{
    ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3, PayloadAttributes,
    PayloadStatusEnum,
};
use alloy_rpc_types_eth::Block;
use op_alloy_consensus::{OpTypedTransaction, TxDeposit};
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use rollup_boost::{Flashblocks, FlashblocksService};

use super::apis::EngineApi;

// L1 block info for OP mainnet block 124665056 (stored in input of tx at index 0)
//
// https://optimistic.etherscan.io/tx/0x312e290cf36df704a2217b015d6455396830b0ce678b860ebfcc30f41403d7b1
const FJORD_DATA: &[u8] = &hex!("440a5e200000146b000f79c500000000000000040000000066d052e700000000013ad8a3000000000000000000000000000000000000000000000000000000003ef1278700000000000000000000000000000000000000000000000000000000000000012fdf87b89884a61e74b322bbcf60386f543bfae7827725efaaf0ab1de2294a590000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f32985");

/// A system that continuously generates blocks using the engine API
pub struct BlockGenerator {
    engine_api: EngineApi,
    validation_api: Option<EngineApi>,
    latest_hash: B256,
    no_tx_pool: bool,
    block_time_secs: u64,
    timestamp: u64,
    // flashblocks service
    flashblocks_endpoint: Option<String>,
    flashblocks_service: Option<FlashblocksService>,
}

impl BlockGenerator {
    pub fn new(
        engine_api: EngineApi,
        validation_api: Option<EngineApi>,
        no_tx_pool: bool,
        block_time_secs: u64,
        flashblocks_endpoint: Option<String>,
    ) -> Self {
        Self {
            engine_api,
            validation_api,
            latest_hash: B256::ZERO, // temporary value
            no_tx_pool,
            timestamp: 0,
            block_time_secs,
            flashblocks_endpoint,
            flashblocks_service: None,
        }
    }

    /// Initialize the block generator by fetching the latest block
    pub async fn init(&mut self) -> eyre::Result<Block> {
        let latest_block = self.engine_api.latest().await?.expect("block not found");
        self.latest_hash = latest_block.header.hash;
        self.timestamp = latest_block.header.timestamp;

        // Sync validation node if it exists
        if let Some(validation_api) = &self.validation_api {
            self.sync_validation_node(validation_api).await?;
        }

        // Initialize flashblocks service
        if let Some(flashblocks_endpoint) = &self.flashblocks_endpoint {
            println!("Initializing flashblocks service at {flashblocks_endpoint}");

            self.flashblocks_service = Some(Flashblocks::run(
                flashblocks_endpoint.to_string(),
                "127.0.0.1:1112".to_string(), // output address for the preconfirmations from rb
            )?);
        }

        Ok(latest_block)
    }

    /// Sync the validation node to the current state
    async fn sync_validation_node(&self, validation_api: &EngineApi) -> eyre::Result<()> {
        let latest_validation_block = validation_api.latest().await?.expect("block not found");
        let latest_block = self.engine_api.latest().await?.expect("block not found");

        if latest_validation_block.header.number > latest_block.header.number {
            return Err(eyre::eyre!("validation node is ahead of the builder"));
        }

        if latest_validation_block.header.number < latest_block.header.number {
            println!(
                "validation node {} is behind the builder {}, syncing up",
                latest_validation_block.header.number, latest_block.header.number
            );

            let mut latest_hash = latest_validation_block.header.hash;

            for i in (latest_validation_block.header.number + 1)..=latest_block.header.number {
                println!("syncing block {i}");

                let block = self
                    .engine_api
                    .get_block_by_number(BlockNumberOrTag::Number(i), true)
                    .await?
                    .expect("block not found");

                if block.header.parent_hash != latest_hash {
                    return Err(eyre::eyre!("unexpected parent hash during sync"));
                }

                let payload_request = ExecutionPayloadV3 {
                    payload_inner: ExecutionPayloadV2 {
                        payload_inner: ExecutionPayloadV1 {
                            parent_hash: block.header.parent_hash,
                            fee_recipient: block.header.beneficiary,
                            state_root: block.header.state_root,
                            receipts_root: block.header.receipts_root,
                            logs_bloom: block.header.logs_bloom,
                            prev_randao: B256::ZERO,
                            block_number: block.header.number,
                            gas_limit: block.header.gas_limit,
                            gas_used: block.header.gas_used,
                            timestamp: block.header.timestamp,
                            extra_data: block.header.extra_data.clone(),
                            base_fee_per_gas: U256::from(block.header.base_fee_per_gas.unwrap()),
                            block_hash: block.header.hash,
                            transactions: vec![], // there are no txns yet
                        },
                        withdrawals: block.withdrawals.unwrap().to_vec(),
                    },
                    blob_gas_used: block.header.inner.blob_gas_used.unwrap(),
                    excess_blob_gas: block.header.inner.excess_blob_gas.unwrap(),
                };

                let validation_status = validation_api
                    .new_payload(payload_request, vec![], B256::ZERO)
                    .await?;

                if validation_status.status != PayloadStatusEnum::Valid {
                    return Err(eyre::eyre!("invalid payload status during sync"));
                }

                let new_chain_hash = validation_status
                    .latest_valid_hash
                    .ok_or_else(|| eyre::eyre!("missing latest valid hash"))?;

                if new_chain_hash != block.header.hash {
                    return Err(eyre::eyre!("hash mismatch during sync"));
                }

                validation_api
                    .update_forkchoice(latest_hash, new_chain_hash, None)
                    .await?;

                latest_hash = new_chain_hash;
            }
        }

        Ok(())
    }

    /// Helper function to submit a payload and update chain state
    pub async fn submit_payload(
        &mut self,
        transactions: Option<Vec<Bytes>>,
        block_building_delay_secs: u64,
        no_sleep: bool, // TODO: Change this, too many parameters we can tweak here to put as a function arguments
    ) -> eyre::Result<B256> {
        let timestamp = self.timestamp + self.block_time_secs;

        // Add L1 block info as the first transaction in every L2 block
        // This deposit transaction contains L1 block metadata required by the L2 chain
        // Currently using hardcoded data from L1 block 124665056
        // If this info is not provided, Reth cannot decode the receipt for any transaction
        // in the block since it also includes this info as part of the result.
        // It does not matter if the to address (4200000000000000000000000000000000000015) is
        // not deployed on the L2 chain since Reth queries the block to get the info and not the contract.
        let block_info_tx: Bytes = {
            let deposit_tx = TxDeposit {
                source_hash: B256::default(),
                from: address!("DeaDDEaDDeAdDeAdDEAdDEaddeAddEAdDEAd0001"),
                to: TxKind::Call(address!("4200000000000000000000000000000000000015")),
                mint: None,
                value: U256::default(),
                gas_limit: 210000,
                is_system_transaction: false,
                input: FJORD_DATA.into(),
            };

            // Create a temporary signer for the deposit
            let signer = Signer::random();
            let signed_tx = signer.sign_tx(OpTypedTransaction::Deposit(deposit_tx))?;
            signed_tx.encoded_2718().into()
        };

        let transactions = if let Some(transactions) = transactions {
            // prepend the block info transaction
            let mut all_transactions = vec![block_info_tx];
            all_transactions.extend(transactions.into_iter());
            all_transactions
        } else {
            vec![block_info_tx]
        };

        let result = self
            .engine_api
            .update_forkchoice(
                self.latest_hash,
                self.latest_hash,
                Some(OpPayloadAttributes {
                    payload_attributes: PayloadAttributes {
                        withdrawals: Some(vec![]),
                        parent_beacon_block_root: Some(B256::ZERO),
                        timestamp,
                        prev_randao: B256::ZERO,
                        suggested_fee_recipient: Default::default(),
                    },
                    transactions: Some(transactions),
                    no_tx_pool: Some(self.no_tx_pool),
                    gas_limit: Some(10000000),
                    eip_1559_params: None,
                }),
            )
            .await?;

        if result.payload_status.status != PayloadStatusEnum::Valid {
            return Err(eyre::eyre!("Invalid payload status"));
        }

        let payload_id = result.payload_id.unwrap();

        // update the payload id in the flashblocks service if present
        if let Some(flashblocks_service) = &self.flashblocks_service {
            flashblocks_service.set_current_payload_id(payload_id).await;
        }

        if !self.no_tx_pool && !no_sleep {
            let sleep_time = self.block_time_secs + block_building_delay_secs;
            tokio::time::sleep(tokio::time::Duration::from_secs(sleep_time)).await;
        }

        let payload = if let Some(flashblocks_service) = &self.flashblocks_service {
            flashblocks_service.get_best_payload().await?.unwrap()
        } else {
            self.engine_api.get_payload_v3(payload_id).await?
        };

        // Validate with builder node
        let validation_status = self
            .engine_api
            .new_payload(payload.execution_payload.clone(), vec![], B256::ZERO)
            .await?;

        if validation_status.status != PayloadStatusEnum::Valid {
            return Err(eyre::eyre!("Invalid validation status from builder"));
        }

        // Validate with validation node if present
        if let Some(validation_api) = &self.validation_api {
            let validation_status = validation_api
                .new_payload(payload.execution_payload.clone(), vec![], B256::ZERO)
                .await?;

            if validation_status.status != PayloadStatusEnum::Valid {
                return Err(eyre::eyre!("Invalid validation status from validator"));
            }
        }

        let new_block_hash = payload
            .execution_payload
            .payload_inner
            .payload_inner
            .block_hash;

        // Update forkchoice on builder
        self.engine_api
            .update_forkchoice(self.latest_hash, new_block_hash, None)
            .await?;

        // Update forkchoice on validator if present
        if let Some(validation_api) = &self.validation_api {
            validation_api
                .update_forkchoice(self.latest_hash, new_block_hash, None)
                .await?;
        }

        // Update internal state
        self.latest_hash = new_block_hash;
        self.timestamp = payload.execution_payload.timestamp();

        Ok(new_block_hash)
    }

    /// Generate a single new block and return its hash
    pub async fn generate_block(&mut self) -> eyre::Result<B256> {
        self.submit_payload(None, 0, false).await
    }

    pub async fn generate_block_with_delay(&mut self, delay: u64) -> eyre::Result<B256> {
        self.submit_payload(None, delay, false).await
    }

    /// Submit a deposit transaction to seed an account with ETH
    pub async fn deposit(&mut self, address: Address, value: u128) -> eyre::Result<B256> {
        // Create deposit transaction
        let deposit_tx = TxDeposit {
            source_hash: B256::default(),
            from: address, // Set the sender to the address of the account to seed
            to: TxKind::Create,
            mint: Some(value), // Amount to deposit
            value: U256::default(),
            gas_limit: 210000,
            is_system_transaction: false,
            input: Bytes::default(),
        };

        // Create a temporary signer for the deposit
        let signer = Signer::random();
        let signed_tx = signer.sign_tx(OpTypedTransaction::Deposit(deposit_tx))?;
        let signed_tx_rlp = signed_tx.encoded_2718();

        self.submit_payload(Some(vec![signed_tx_rlp.into()]), 0, false)
            .await
    }

    pub async fn create_funded_accounts(
        &mut self,
        count: usize,
        amount: u128,
    ) -> eyre::Result<Vec<Signer>> {
        let mut signers = Vec::with_capacity(count);

        for _ in 0..count {
            // Create a new signer
            let signer = Signer::random();
            let address = signer.address;

            // Deposit funds to the new account
            self.deposit(address, amount).await?;

            signers.push(signer);
        }

        Ok(signers)
    }
}
