use alloy_eips::BlockNumberOrTag;
use alloy_network::Ethereum;
use alloy_primitives::B256;
use alloy_primitives::U256;
use alloy_provider::Provider;
use alloy_provider::RootProvider;
use alloy_rpc_types_engine::ExecutionPayloadV1;
use alloy_rpc_types_engine::ExecutionPayloadV2;
use alloy_rpc_types_engine::PayloadAttributes;
use alloy_rpc_types_engine::PayloadStatusEnum;
use alloy_rpc_types_engine::{ExecutionPayloadV3, ForkchoiceUpdated, PayloadStatus};
use alloy_transport::BoxTransport;
use clap::Parser;
use jsonrpsee::http_client::{transport::HttpBackend, HttpClient};
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use reth::{
    api::EngineTypes,
    rpc::{
        api::EngineApiClient,
        types::{engine::ForkchoiceState, BlockTransactionsKind},
    },
};
use reth_optimism_node::OpEngineTypes;
use reth_payload_builder::PayloadId;
use reth_rpc_layer::{AuthClientLayer, AuthClientService, JwtSecret};
use std::{marker::PhantomData, str::FromStr};

/// Helper for engine api operations
pub struct EngineApi<E> {
    url: String,
    pub engine_api_client: HttpClient<AuthClientService<HttpBackend>>,
    pub _marker: PhantomData<E>,
}

pub type BoxedProvider = RootProvider<BoxTransport, Ethereum>;

impl EngineApi<OpEngineTypes> {
    pub fn new(url: &str, non_auth_url: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let secret_layer = AuthClientLayer::new(JwtSecret::from_str(
            "688f5d737bad920bdfb2fc2f488d6b6209eebda1dae949a8de91398d932c517a",
        )?);
        let middleware = tower::ServiceBuilder::default().layer(secret_layer);
        let client = jsonrpsee::http_client::HttpClientBuilder::default()
            .set_http_middleware(middleware)
            .build(url)
            .expect("Failed to create http client");

        Ok(Self {
            url: non_auth_url.to_string(),
            engine_api_client: client,
            _marker: PhantomData,
        })
    }
}

impl<E: EngineTypes + 'static> EngineApi<E> {
    /// Retrieves a v3 payload from the engine api
    pub async fn get_payload_v3(
        &self,
        payload_id: PayloadId,
    ) -> eyre::Result<E::ExecutionPayloadEnvelopeV3> {
        Ok(EngineApiClient::<E>::get_payload_v3(&self.engine_api_client, payload_id).await?)
    }

    pub async fn new_payload(
        &self,
        payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
    ) -> eyre::Result<PayloadStatus> {
        Ok(EngineApiClient::<E>::new_payload_v3(
            &self.engine_api_client,
            payload,
            versioned_hashes,
            parent_beacon_block_root,
        )
        .await?)
    }

    /// Sends forkchoice update to the engine api
    pub async fn update_forkchoice(
        &self,
        current_head: B256,
        new_head: B256,
        payload_attributes: Option<E::PayloadAttributes>,
    ) -> eyre::Result<ForkchoiceUpdated> {
        Ok(EngineApiClient::<E>::fork_choice_updated_v3(
            &self.engine_api_client,
            ForkchoiceState {
                head_block_hash: new_head,
                safe_block_hash: current_head,
                finalized_block_hash: current_head,
            },
            payload_attributes,
        )
        .await?)
    }

    pub async fn latest(&self) -> alloy_rpc_types_eth::Block {
        self.get_block_by_number(BlockNumberOrTag::Latest, BlockTransactionsKind::Hashes)
            .await
    }

    pub async fn get_block_by_number(
        &self,
        number: BlockNumberOrTag,
        kind: BlockTransactionsKind,
    ) -> alloy_rpc_types_eth::Block {
        // TODO: Do not know how to use the other auth provider for this rpc call
        let provider: BoxedProvider = RootProvider::new_http(self.url.parse().unwrap()).boxed();
        provider
            .get_block_by_number(number, kind)
            .await
            .unwrap()
            .unwrap()
    }
}

/// This is a simple program
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    #[clap(long, short, action)]
    validation: bool,

    #[clap(long, short, action, default_value = "false")]
    no_tx_pool: bool,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    println!("Validation: {}", args.validation);

    let engine_api =
        EngineApi::<OpEngineTypes>::new("http://localhost:4444", "http://localhost:1111").unwrap();

    let validation_node_api = if args.validation {
        Some(
            EngineApi::<OpEngineTypes>::new("http://localhost:5555", "http://localhost:2222")
                .unwrap(),
        )
    } else {
        None
    };

    let latest_block = engine_api.latest().await;

    // latest hash and timestamp
    let mut latest = latest_block.header.hash;
    let mut timestamp = latest_block.header.timestamp;

    if let Some(validation_node_api) = &validation_node_api {
        // if the validation node is behind the builder, try to sync it using the engine api
        let latest_validation_block = validation_node_api.latest().await;

        if latest_validation_block.header.number > latest_block.header.number {
            panic!("validation node is ahead of the builder")
        }
        if latest_validation_block.header.number < latest_block.header.number {
            println!(
                "validation node {} is behind the builder {}, syncing up",
                latest_validation_block.header.number, latest_block.header.number
            );

            let mut latest_hash = latest_validation_block.header.hash;

            // sync them up using fcu requests
            for i in (latest_validation_block.header.number + 1)..=latest_block.header.number {
                println!("syncing block {}", i);

                let block = engine_api
                    .get_block_by_number(BlockNumberOrTag::Number(i), BlockTransactionsKind::Full)
                    .await;

                if block.header.parent_hash != latest_hash {
                    panic!("not expected")
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

                let validation_payload = validation_node_api
                    .new_payload(payload_request, vec![], B256::ZERO)
                    .await
                    .unwrap();

                if validation_payload.status != PayloadStatusEnum::Valid {
                    panic!("not expected")
                }

                let new_chain_hash = validation_payload.latest_valid_hash.unwrap();
                if new_chain_hash != block.header.hash {
                    println!("synced block {:?}", new_chain_hash);
                    println!("expected block {:?}", block.header.hash);

                    panic!("stop")
                }

                // send an fcu request to add to the canonical chain
                let _result = validation_node_api
                    .update_forkchoice(latest_hash, new_chain_hash, None)
                    .await
                    .unwrap();

                latest_hash = new_chain_hash;
            }
        }
    }

    loop {
        println!("latest block: {}", latest);

        let result = engine_api
            .update_forkchoice(
                latest,
                latest,
                Some(OpPayloadAttributes {
                    payload_attributes: PayloadAttributes {
                        withdrawals: Some(vec![]),
                        parent_beacon_block_root: Some(B256::ZERO),
                        timestamp: timestamp + 1000,
                        prev_randao: B256::ZERO,
                        suggested_fee_recipient: Default::default(),
                    },
                    transactions: None,
                    no_tx_pool: Some(args.no_tx_pool),
                    gas_limit: Some(10000000000),
                    eip_1559_params: None,
                }),
            )
            .await
            .unwrap();

        if result.payload_status.status != PayloadStatusEnum::Valid {
            panic!("not expected")
        }

        let payload_id = result.payload_id.unwrap();

        // Only wait for the block time to request the payload if the block builder
        // is expected to use the txpool (this is, no_tx_pool is false)
        if !args.no_tx_pool {
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }

        // query the result, do nothing, just checks that we can get it back
        let payload = engine_api.get_payload_v3(payload_id).await.unwrap();

        // Send a new_payload request to the builder node again. THIS MUST BE DONE
        let validation_status = engine_api
            .new_payload(payload.execution_payload.clone(), vec![], B256::ZERO)
            .await
            .unwrap();
        if validation_status.status != PayloadStatusEnum::Valid {
            panic!("not expected")
        }

        if let Some(validation_node_api) = &validation_node_api {
            // Validate the payload with the validation node
            let validation_status = validation_node_api
                .new_payload(payload.execution_payload.clone(), vec![], B256::ZERO)
                .await
                .unwrap();

            if validation_status.status != PayloadStatusEnum::Valid {
                panic!("not expected")
            }
        }

        let new_block_hash = payload
            .execution_payload
            .payload_inner
            .payload_inner
            .block_hash;
        let new_timestamp = payload
            .execution_payload
            .payload_inner
            .payload_inner
            .timestamp;

        // send an FCU wihtout payload attributes to lock in the block for the builder
        let _result = engine_api
            .update_forkchoice(latest, new_block_hash, None)
            .await
            .unwrap();

        if let Some(validation_node_api) = &validation_node_api {
            // do the same for the validator
            let _result = validation_node_api
                .update_forkchoice(latest, new_block_hash, None)
                .await
                .unwrap();
        }

        latest = new_block_hash;
        timestamp = new_timestamp;
    }
}
