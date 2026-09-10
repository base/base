#![allow(unreachable_pub)]
//! Standalone http tests

use std::collections::HashSet;

use alloy_eips::{BlockId, BlockNumberOrTag, Encodable2718};
use alloy_primitives::{Address, B64, B256, Bytes, TxHash, U64, U256};
use base_common_types_rpc::{
    BaseTransactionRequest, Block, FeeHistory, Filter, Index, Log, PendingTransactionFilterKind,
    SyncStatus, TraceFilter, Transaction, TransactionReceipt,
};
use base_execution_network_wire::NodeRecord;
use base_execution_rpc::{
    AdminApiClient, DebugApiClient, EthApiClient, EthCallBundleApiClient, EthFilterApiClient,
    NetApiClient, TraceApiClient, Web3ApiClient,
};
use jsonrpsee::{
    core::{
        client::{ClientT, SubscriptionClientT},
        params::ArrayParams,
    },
    http_client::HttpClient,
    rpc_params,
    types::error::ErrorCode,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::utils::{launch_http, launch_http_ws, launch_ws};

fn is_unimplemented(err: jsonrpsee::core::client::Error) -> bool {
    match err {
        jsonrpsee::core::client::Error::Call(error_obj) => {
            error_obj.code() == ErrorCode::InternalError.code()
                && error_obj.message() == "unimplemented"
        }
        _ => false,
    }
}

fn is_invalid_params(err: &jsonrpsee::core::client::Error) -> bool {
    match err {
        jsonrpsee::core::client::Error::Call(error_obj) => {
            error_obj.code() == ErrorCode::InvalidParams.code()
        }
        _ => false,
    }
}

async fn test_rpc_call_ok<R>(client: &HttpClient, method_name: &str, params: ArrayParams)
where
    R: DeserializeOwned,
{
    // Make the RPC request
    match client.request::<R, _>(method_name, params).await {
        Ok(_) => {} // If the request is successful, do nothing
        Err(e) => {
            // If an error occurs, panic with the error message
            panic!("Expected successful response, got error: {e:?}");
        }
    }
}

async fn test_rpc_call_err<R>(client: &HttpClient, method_name: &str, params: ArrayParams)
where
    R: DeserializeOwned + std::fmt::Debug,
{
    // Make the RPC request
    if let Ok(resp) = client.request::<R, _>(method_name, params).await {
        // Panic if an unexpected successful response is received
        panic!("Expected error response, got successful response: {resp:?}");
    };
}

async fn assert_empty_raw_transaction_sync_error(client: &HttpClient, params: ArrayParams) {
    let err = client
        .request::<Value, _>("eth_sendRawTransactionSync", params)
        .await
        .expect_err("empty raw transaction should fail after RPC parameter decoding");
    assert!(err.to_string().contains("empty transaction data"), "{err:?}");
}

/// Represents a builder for creating JSON-RPC requests.
#[derive(Clone, Serialize, Deserialize)]
pub struct RawRpcParamsBuilder {
    method: Option<String>,
    params: Vec<Value>,
    id: i32,
}

impl RawRpcParamsBuilder {
    /// Sets the method name for the JSON-RPC request.
    pub fn method(mut self, method: impl Into<String>) -> Self {
        self.method = Some(method.into());
        self
    }

    /// Adds a parameter to the JSON-RPC request.
    pub fn add_param<S: Serialize>(mut self, param: S) -> Self {
        self.params.push(serde_json::to_value(param).expect("Failed to serialize parameter"));
        self
    }

    /// Sets the ID for the JSON-RPC request.
    pub const fn set_id(mut self, id: i32) -> Self {
        self.id = id;
        self
    }

    /// Constructs the JSON-RPC request string based on the provided configurations.
    pub fn build(self) -> String {
        let Self { method, params, id } = self;
        let method = method.unwrap_or_else(|| panic!("JSON-RPC method not set"));
        let params: Vec<String> = params.into_iter().map(|p| p.to_string()).collect();

        format!(
            r#"{{"jsonrpc":"2.0","id":{},"method":"{}","params":[{}]}}"#,
            id,
            method,
            params.join(",")
        )
    }
}

impl Default for RawRpcParamsBuilder {
    fn default() -> Self {
        Self { method: None, params: Vec::new(), id: 1 }
    }
}

async fn test_filter_calls<C>(client: &C)
where
    C: ClientT + SubscriptionClientT + Sync,
{
    EthFilterApiClient::new_filter(client, Filter::default()).await.unwrap();
    EthFilterApiClient::new_pending_transaction_filter(client, None).await.unwrap();
    EthFilterApiClient::new_pending_transaction_filter(
        client,
        Some(PendingTransactionFilterKind::Full),
    )
    .await
    .unwrap();
    let id = EthFilterApiClient::new_block_filter(client).await.unwrap();
    EthFilterApiClient::filter_changes(client, id.clone()).await.unwrap();
    EthFilterApiClient::logs(client, Filter::default()).await.unwrap();
    let id = EthFilterApiClient::new_filter(client, Filter::default()).await.unwrap();
    EthFilterApiClient::filter_logs(client, id.clone()).await.unwrap();
    EthFilterApiClient::uninstall_filter(client, id).await.unwrap();
}

async fn test_basic_admin_calls<C>(client: &C)
where
    C: ClientT + SubscriptionClientT + Sync,
{
    let url = "enode://6f8a80d14311c39f35f516fa664deaaaa13e85b2f7493f37f6144d86991ec012937307647bd3b9a82abe2974e1407241d54947bbb39763a4cac9f77166ad92a0@127.0.0.1:9?discport=9";
    let node: NodeRecord = url.parse().unwrap();

    AdminApiClient::add_peer(client, node).await.unwrap();
    AdminApiClient::remove_peer(client, node.into()).await.unwrap();
    AdminApiClient::add_trusted_peer(client, node.into()).await.unwrap();
    AdminApiClient::remove_trusted_peer(client, node.into()).await.unwrap();
    AdminApiClient::ban_peer(client, node.into()).await.unwrap();
    AdminApiClient::unban_peer(client, node.into()).await.unwrap();
    AdminApiClient::node_info(client).await.unwrap();
}

async fn test_basic_eth_calls<C>(client: &C)
where
    C: ClientT + SubscriptionClientT + Sync,
{
    let address = Address::default();
    let index = Index::default();
    let hash = B256::default();
    let tx_hash = TxHash::default();
    let block_number = BlockNumberOrTag::default();
    let call_request = BaseTransactionRequest::default();
    let transaction_request = BaseTransactionRequest::default();
    let bytes = Bytes::default();
    let tx = base_execution_txpool::test_utils::TransactionBuilder::default()
        .signer(B256::repeat_byte(1))
        .chain_id(8453)
        .nonce(0)
        .to(Address::repeat_byte(2))
        .gas_limit(21_000)
        .value(0)
        .max_fee_per_gas(2_000_000_000)
        .max_priority_fee_per_gas(1_000_000_000)
        .into_eip1559()
        .encoded_2718()
        .into();
    let typed_data = serde_json::from_str(
        r#"{
        "types": {
            "EIP712Domain": []
        },
        "primaryType": "EIP712Domain",
        "domain": {},
        "message": {}
    }"#,
    )
    .unwrap();

    // Implemented
    EthApiClient::protocol_version(client).await.unwrap();
    EthApiClient::chain_id(client).await.unwrap();
    EthApiClient::accounts(client).await.unwrap();
    EthApiClient::get_account(client, address, block_number.into()).await.unwrap();
    EthApiClient::block_number(client).await.unwrap();
    EthApiClient::get_code(client, address, None).await.unwrap();
    EthApiClient::send_raw_transaction(client, tx).await.unwrap();
    EthApiClient::fee_history(client, U64::from(0), block_number, None).await.unwrap();
    EthApiClient::balance(client, address, None).await.unwrap();
    EthApiClient::transaction_count(client, address, None).await.unwrap();
    EthApiClient::storage_at(client, address, U256::default().into(), None).await.unwrap();
    EthApiClient::block_by_hash(client, hash, false).await.unwrap();
    EthApiClient::block_by_number(client, block_number, false).await.unwrap();
    EthApiClient::block_transaction_count_by_number(client, block_number).await.unwrap();
    EthApiClient::block_transaction_count_by_hash(client, hash).await.unwrap();
    EthApiClient::block_uncles_count_by_hash(client, hash).await.unwrap();
    EthApiClient::block_uncles_count_by_number(client, block_number).await.unwrap();
    EthApiClient::uncle_by_block_hash_and_index(client, hash, index).await.unwrap();
    EthApiClient::uncle_by_block_number_and_index(client, block_number, index).await.unwrap();
    EthApiClient::sign(client, address, bytes.clone()).await.unwrap_err();
    EthApiClient::sign_typed_data(client, address, typed_data).await.unwrap_err();
    EthApiClient::transaction_by_hash(client, tx_hash).await.unwrap();
    EthApiClient::transaction_by_block_hash_and_index(client, hash, index).await.unwrap();
    EthApiClient::transaction_by_block_number_and_index(client, block_number, index).await.unwrap();
    EthApiClient::create_access_list(client, call_request.clone(), Some(block_number.into()), None)
        .await
        .unwrap();
    EthApiClient::estimate_gas(client, call_request.clone(), Some(block_number.into()), None, None)
        .await
        .unwrap();
    EthApiClient::call(client, call_request.clone(), Some(block_number.into()), None, None)
        .await
        .unwrap();
    EthApiClient::syncing(client).await.unwrap();
    EthApiClient::send_transaction(client, transaction_request.clone()).await.unwrap_err();
    EthApiClient::sign_transaction(client, transaction_request).await.unwrap_err();
    EthApiClient::hashrate(client).await.unwrap();
    EthApiClient::submit_hashrate(client, U256::default(), B256::default()).await.unwrap();
    EthApiClient::gas_price(client).await.unwrap();
    EthApiClient::max_priority_fee_per_gas(client).await.unwrap();
    EthApiClient::get_proof(client, address, vec![], None).await.unwrap();
    let proofs = EthApiClient::get_multi_proof(client, vec![(address, vec![B256::ZERO])], None)
        .await
        .unwrap();
    assert_eq!(proofs.len(), 1);
    assert_eq!(proofs[0].address, address);

    // Unimplemented
    assert!(is_unimplemented(EthApiClient::author(client).await.err().unwrap()));
    assert!(is_unimplemented(EthApiClient::is_mining(client).await.err().unwrap()));
    assert!(is_unimplemented(EthApiClient::get_work(client).await.err().unwrap()));
    assert!(is_unimplemented(
        EthApiClient::submit_work(client, B64::default(), B256::default(), B256::default())
            .await
            .err()
            .unwrap()
    ));
    EthCallBundleApiClient::call_bundle(client, Default::default()).await.unwrap_err();
}

async fn test_basic_debug_calls<C>(client: &C)
where
    C: ClientT + SubscriptionClientT + Sync,
{
    let block_id = BlockId::number(1);

    DebugApiClient::raw_header(client, block_id).await.unwrap_err();
    DebugApiClient::raw_block(client, block_id).await.unwrap_err();
    DebugApiClient::raw_transaction(client, B256::default()).await.unwrap();
    DebugApiClient::raw_receipts(client, block_id).await.unwrap_err();
    DebugApiClient::bad_blocks(client).await.unwrap();
    DebugApiClient::debug_clear_txpool(client).await.unwrap();
    DebugApiClient::debug_account_at(client, block_id, Index::default(), Address::default())
        .await
        .unwrap_err();
    DebugApiClient::debug_account_info_at(client, block_id, Index::default(), Address::default())
        .await
        .unwrap_err();

    for block_id in [
        BlockId::number(0),
        BlockId::latest(),
        BlockId::hash(B256::ZERO),
        BlockId::hash_canonical(B256::ZERO),
    ] {
        let err =
            DebugApiClient::debug_execution_witness(client, block_id, None).await.unwrap_err();
        assert!(!is_invalid_params(&err));
    }

    let err = DebugApiClient::debug_execution_witness_by_block_hash(
        client,
        B256::ZERO,
        Some(Default::default()),
    )
    .await
    .unwrap_err();
    assert!(!is_invalid_params(&err));
}

async fn test_basic_net_calls<C>(client: &C)
where
    C: ClientT + SubscriptionClientT + Sync,
{
    NetApiClient::version(client).await.unwrap();
    NetApiClient::peer_count(client).await.unwrap();
    NetApiClient::is_listening(client).await.unwrap();
}

async fn test_basic_trace_calls<C>(client: &C)
where
    C: ClientT + SubscriptionClientT + Sync,
{
    let block_id = BlockId::number(1);
    let trace_filter = TraceFilter {
        from_block: Default::default(),
        to_block: Default::default(),
        from_address: Default::default(),
        to_address: Default::default(),
        mode: Default::default(),
        after: None,
        count: None,
    };

    TraceApiClient::trace_raw_transaction(client, Bytes::default(), HashSet::default(), None)
        .await
        .unwrap_err();
    assert!(
        TraceApiClient::trace_call_many(client, vec![], Some(BlockNumberOrTag::Latest.into()),)
            .await
            .unwrap()
            .is_empty()
    );
    TraceApiClient::replay_transaction(client, B256::default(), HashSet::default())
        .await
        .err()
        .unwrap();
    TraceApiClient::trace_block(client, block_id).await.unwrap_err();
    assert!(
        TraceApiClient::replay_block_transactions(client, block_id, HashSet::default(),)
            .await
            .unwrap()
            .is_none()
    );

    TraceApiClient::trace_filter(client, trace_filter).await.unwrap();
}

async fn test_basic_web3_calls<C>(client: &C)
where
    C: ClientT + SubscriptionClientT + Sync,
{
    Web3ApiClient::client_version(client).await.unwrap();
    Web3ApiClient::sha3(client, Bytes::default()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_call_filter_functions_http() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_http().await;
    let client = handle.http_client().unwrap();
    test_filter_calls(&client).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_call_admin_functions_http() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_http().await;
    let client = handle.http_client().unwrap();
    test_basic_admin_calls(&client).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_call_admin_functions_ws() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_ws().await;
    let client = handle.ws_client().await.unwrap();
    test_basic_admin_calls(&client).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_call_admin_functions_http_and_ws() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_http_ws().await;
    let client = handle.http_client().unwrap();
    test_basic_admin_calls(&client).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_call_eth_functions_http() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_http().await;
    let client = handle.http_client().unwrap();
    test_basic_eth_calls(&client).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_send_raw_transaction_sync_accepts_optional_timeout_arg() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    let mut one_arg = ArrayParams::new();
    one_arg.insert(Bytes::default()).unwrap();
    assert_empty_raw_transaction_sync_error(&client, one_arg).await;

    let mut null_timeout = ArrayParams::new();
    null_timeout.insert(Bytes::default()).unwrap();
    null_timeout.insert(Value::Null).unwrap();
    assert_empty_raw_transaction_sync_error(&client, null_timeout).await;

    let mut two_args = ArrayParams::new();
    two_args.insert(Bytes::default()).unwrap();
    two_args.insert(1u64).unwrap();
    assert_empty_raw_transaction_sync_error(&client, two_args).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_call_eth_functions_ws() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_ws().await;
    let client = handle.ws_client().await.unwrap();
    test_basic_eth_calls(&client).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_call_eth_functions_http_and_ws() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_http_ws().await;
    let client = handle.http_client().unwrap();
    test_basic_eth_calls(&client).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_call_debug_functions_http() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_http().await;
    let client = handle.http_client().unwrap();
    test_basic_debug_calls(&client).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_call_debug_functions_ws() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_ws().await;
    let client = handle.ws_client().await.unwrap();
    test_basic_debug_calls(&client).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_call_debug_functions_http_and_ws() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_http_ws().await;
    let client = handle.http_client().unwrap();
    test_basic_debug_calls(&client).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_call_net_functions_http() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_http().await;
    let client = handle.http_client().unwrap();
    test_basic_net_calls(&client).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_call_net_functions_ws() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_ws().await;
    let client = handle.ws_client().await.unwrap();
    test_basic_net_calls(&client).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_call_net_functions_http_and_ws() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_http_ws().await;
    let client = handle.http_client().unwrap();
    test_basic_net_calls(&client).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_call_trace_functions_http() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_http().await;
    let client = handle.http_client().unwrap();
    test_basic_trace_calls(&client).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_call_trace_functions_ws() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_ws().await;
    let client = handle.ws_client().await.unwrap();
    test_basic_trace_calls(&client).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_call_trace_functions_http_and_ws() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_http_ws().await;
    let client = handle.http_client().unwrap();
    test_basic_trace_calls(&client).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_call_web3_functions_http() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_http().await;
    let client = handle.http_client().unwrap();
    test_basic_web3_calls(&client).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_call_web3_functions_ws() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_ws().await;
    let client = handle.ws_client().await.unwrap();
    test_basic_web3_calls(&client).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_call_web3_functions_http_and_ws() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_http_ws().await;
    let client = handle.http_client().unwrap();
    test_basic_web3_calls(&client).await;
}

// <https://github.com/paradigmxyz/reth/issues/5830>
#[tokio::test(flavor = "multi_thread")]
async fn test_eth_logs_args() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_http_ws().await;
    let client = handle.http_client().unwrap();

    let mut params = ArrayParams::default();
    params.insert(serde_json::json!({"blockHash":"0x58dc57ab582b282c143424bd01e8d923cddfdcda9455bad02a29522f6274a948"})).unwrap();

    let resp = client.request::<Vec<Log>, _>("eth_getLogs", params).await;
    // block does not exist
    assert!(resp.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_block_by_number_rpc_call() {
    base_common_observability_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    // Requesting block by number with proper fields
    test_rpc_call_ok::<Option<Block>>(
        &client,
        "eth_getBlockByNumber",
        rpc_params!["0x1b4", true], // Block number and full transaction object flag
    )
    .await;

    // Requesting block by number with wrong fields
    test_rpc_call_err::<Option<Block>>(
        &client,
        "eth_getBlockByNumber",
        rpc_params!["0x1b4", "0x1b4"],
    )
    .await;

    // Requesting block by number with missing fields
    test_rpc_call_err::<Option<Block>>(&client, "eth_getBlockByNumber", rpc_params!["0x1b4"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_block_by_hash_rpc_call() {
    base_common_observability_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    // Requesting block by hash with proper fields
    test_rpc_call_ok::<Option<Block>>(
        &client,
        "eth_getBlockByHash",
        rpc_params!["0xdc0818cf78f21a8e70579cb46a43643f78291264dda342ae31049421c82d21ae", false],
    )
    .await;

    // Requesting block by hash with wrong fields
    test_rpc_call_err::<Option<Block>>(
        &client,
        "eth_getBlockByHash",
        rpc_params!["0xdc0818cf78f21a8e70579cb46a43643f78291264dda342ae31049421c82d21ae", "0x1b4"],
    )
    .await;

    // Requesting block by hash with missing fields
    test_rpc_call_err::<Option<Block>>(
        &client,
        "eth_getBlockByHash",
        rpc_params!["0xdc0818cf78f21a8e70579cb46a43643f78291264dda342ae31049421c82d21ae"],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_code_rpc_call() {
    base_common_observability_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    // Requesting code at a given address with proper fields
    test_rpc_call_ok::<Bytes>(
        &client,
        "eth_getCode",
        rpc_params![
            "0xa94f5374fce5edbc8e2a8697c15331677e6ebf0b",
            "0x0" // genesis
        ],
    )
    .await;

    // Define test cases with different default block parameters
    let default_block_params = vec!["earliest", "latest", "pending"];

    // Iterate over test cases
    for param in default_block_params {
        // Requesting code at a given address with default block parameter
        test_rpc_call_ok::<Bytes>(
            &client,
            "eth_getCode",
            rpc_params!["0xa94f5374fce5edbc8e2a8697c15331677e6ebf0b", param],
        )
        .await;
    }

    // Without block number which is optional
    test_rpc_call_ok::<Bytes>(
        &client,
        "eth_getCode",
        rpc_params!["0xa94f5374fce5edbc8e2a8697c15331677e6ebf0b"],
    )
    .await;

    // Requesting code at a given address with invalid default block parameter
    test_rpc_call_err::<Bytes>(
        &client,
        "eth_getCode",
        rpc_params!["0xa94f5374fce5edbc8e2a8697c15331677e6ebf0b", "finalized"],
    )
    .await;

    // Requesting code at a given address with wrong fields
    test_rpc_call_err::<Bytes>(
        &client,
        "eth_getCode",
        rpc_params!["0xa94f5374fce5edbc8e2a8697c15331677e6ebf0b", false],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_block_number_rpc_call() {
    base_common_observability_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    // Requesting block number without any parameter
    test_rpc_call_ok::<U256>(&client, "eth_blockNumber", rpc_params![]).await;

    // Define test cases with different default block parameters
    let invalid_default_block_params = vec!["finalized", "0x2"];

    // Iterate over test cases
    for param in invalid_default_block_params {
        // Requesting block number with invalid parameter should not throw an error
        test_rpc_call_ok::<U256>(&client, "eth_blockNumber", rpc_params![param]).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_chain_id_rpc_call() {
    base_common_observability_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    // Requesting chain ID without any parameter
    test_rpc_call_ok::<Option<U64>>(&client, "eth_chainId", rpc_params![]).await;

    // Define test cases with different invalid parameters
    let invalid_params = vec!["finalized", "0x2"];

    // Iterate over test cases
    for param in invalid_params {
        // Requesting chain ID with invalid parameter should not throw an error
        test_rpc_call_ok::<Option<U64>>(&client, "eth_chainId", rpc_params![param]).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_syncing_rpc_call() {
    base_common_observability_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    // Requesting syncing status
    test_rpc_call_ok::<Option<SyncStatus>>(&client, "eth_syncing", rpc_params![]).await;

    // Define test cases with invalid parameters
    let invalid_params = vec!["latest", "earliest", "pending", "0x2"];

    // Iterate over test cases
    for param in invalid_params {
        // Requesting syncing status with invalid parameter should not throw an error
        test_rpc_call_ok::<Option<SyncStatus>>(&client, "eth_syncing", rpc_params![param]).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_protocol_version_rpc_call() {
    base_common_observability_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    // Requesting protocol version without any parameter
    test_rpc_call_ok::<U64>(&client, "eth_protocolVersion", rpc_params![]).await;

    // Define test cases with invalid parameters
    let invalid_params = vec!["latest", "earliest", "pending", "0x2"];

    // Iterate over test cases
    for param in invalid_params {
        // Requesting protocol version with invalid parameter should not throw an error
        test_rpc_call_ok::<U64>(&client, "eth_protocolVersion", rpc_params![param]).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_coinbase_rpc_call() {
    base_common_observability_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    // Requesting coinbase address without any parameter should return Unimplemented
    match client.request::<Address, _>("eth_coinbase", rpc_params![]).await {
        Ok(_) => {
            // If there's a response, it's unexpected, panic
            panic!("Expected Unimplemented error, got successful response");
        }
        Err(err) => {
            assert!(is_unimplemented(err));
        }
    };
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_accounts_rpc_call() {
    base_common_observability_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    // Requesting accounts without any parameter
    test_rpc_call_ok::<Option<Vec<Address>>>(&client, "eth_accounts", rpc_params![]).await;

    // Define test cases with invalid parameters
    let invalid_params = vec!["latest", "earliest", "pending", "0x2"];

    // Iterate over test cases
    for param in invalid_params {
        // Requesting accounts with invalid parameter should not throw an error
        test_rpc_call_ok::<Option<Vec<Address>>>(&client, "eth_accounts", rpc_params![param]).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_block_transaction_count_by_hash_rpc_call() {
    base_common_observability_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    // Requesting transaction count by block hash with proper fields
    test_rpc_call_ok::<Option<U256>>(
        &client,
        "eth_getBlockTransactionCountByHash",
        rpc_params!["0xb903239f8543d04b5dc1ba6579132b143087c68db1b2168786408fcbce568238"],
    )
    .await;

    // Requesting transaction count by block hash with additional fields
    test_rpc_call_ok::<Option<U256>>(
        &client,
        "eth_getBlockTransactionCountByHash",
        rpc_params!["0xb903239f8543d04b5dc1ba6579132b143087c68db1b2168786408fcbce568238", true],
    )
    .await;

    // Requesting transaction count by block hash with missing fields
    test_rpc_call_err::<Option<U256>>(&client, "eth_getBlockTransactionCountByHash", rpc_params![])
        .await;

    // Requesting transaction count by block hash with wrong field
    test_rpc_call_err::<Option<U256>>(
        &client,
        "eth_getBlockTransactionCountByHash",
        rpc_params![true],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_block_transaction_count_by_number_rpc_call() {
    base_common_observability_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    // Requesting transaction count by block number with proper fields
    test_rpc_call_ok::<Option<U256>>(
        &client,
        "eth_getBlockTransactionCountByNumber",
        rpc_params!["0xe8"],
    )
    .await;

    // Requesting transaction count by block number with additional fields
    test_rpc_call_ok::<Option<U256>>(
        &client,
        "eth_getBlockTransactionCountByNumber",
        rpc_params!["0xe8", true],
    )
    .await;

    // Requesting transaction count by block number with missing fields
    test_rpc_call_err::<Option<U256>>(
        &client,
        "eth_getBlockTransactionCountByNumber",
        rpc_params![],
    )
    .await;

    // Requesting transaction count by block number with wrong field
    test_rpc_call_err::<Option<U256>>(
        &client,
        "eth_getBlockTransactionCountByNumber",
        rpc_params![true],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_uncle_count_by_block_hash_rpc_call() {
    base_common_observability_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    // Requesting uncle count by block hash with proper fields
    test_rpc_call_ok::<Option<U256>>(
        &client,
        "eth_getUncleCountByBlockHash",
        rpc_params!["0xb903239f8543d04b5dc1ba6579132b143087c68db1b2168786408fcbce568238"],
    )
    .await;

    // Requesting uncle count by block hash with additional fields
    test_rpc_call_ok::<Option<U256>>(
        &client,
        "eth_getUncleCountByBlockHash",
        rpc_params!["0xb903239f8543d04b5dc1ba6579132b143087c68db1b2168786408fcbce568238", true],
    )
    .await;

    // Requesting uncle count by block hash with missing fields
    test_rpc_call_err::<Option<U256>>(&client, "eth_getUncleCountByBlockHash", rpc_params![]).await;

    // Requesting uncle count by block hash with wrong field
    test_rpc_call_err::<Option<U256>>(&client, "eth_getUncleCountByBlockHash", rpc_params![true])
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_uncle_count_by_block_number_rpc_call() {
    base_common_observability_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    // Requesting uncle count by block number with proper fields
    test_rpc_call_ok::<Option<U256>>(
        &client,
        "eth_getUncleCountByBlockNumber",
        rpc_params!["0xe8"],
    )
    .await;

    // Requesting uncle count by block number with additional fields
    test_rpc_call_ok::<Option<U256>>(
        &client,
        "eth_getUncleCountByBlockNumber",
        rpc_params!["0xe8", true],
    )
    .await;

    // Requesting uncle count by block number with missing fields
    test_rpc_call_err::<Option<U256>>(&client, "eth_getUncleCountByBlockNumber", rpc_params![])
        .await;

    // Requesting uncle count by block number with wrong field
    test_rpc_call_err::<Option<U256>>(&client, "eth_getUncleCountByBlockNumber", rpc_params![true])
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_block_receipts_rpc_call() {
    base_common_observability_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    // Requesting block receipts by block hash with proper fields
    test_rpc_call_ok::<Option<Vec<TransactionReceipt>>>(
        &client,
        "eth_getBlockReceipts",
        rpc_params!["0xe8"],
    )
    .await;

    // Requesting block receipts by block hash with additional fields
    test_rpc_call_ok::<Option<Vec<TransactionReceipt>>>(
        &client,
        "eth_getBlockReceipts",
        rpc_params!["0xe8", true],
    )
    .await;

    // Requesting block receipts by block hash with missing fields
    test_rpc_call_err::<Option<Vec<TransactionReceipt>>>(
        &client,
        "eth_getBlockReceipts",
        rpc_params![],
    )
    .await;

    // Requesting block receipts by block hash with wrong field
    test_rpc_call_err::<Option<Vec<TransactionReceipt>>>(
        &client,
        "eth_getBlockReceipts",
        rpc_params![true],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_uncle_by_block_hash_and_index_rpc_call() {
    base_common_observability_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    // Requesting uncle by block hash and index with proper fields
    test_rpc_call_ok::<Option<Block>>(
        &client,
        "eth_getUncleByBlockHashAndIndex",
        rpc_params!["0xc6ef2fc5426d6ad6fd9e2a26abeab0aa2411b7ab17f30a99d3cb96aed1d1055b", "0x0"],
    )
    .await;

    // Requesting uncle by block hash and index with additional fields
    test_rpc_call_ok::<Option<Block>>(
        &client,
        "eth_getUncleByBlockHashAndIndex",
        rpc_params![
            "0xc6ef2fc5426d6ad6fd9e2a26abeab0aa2411b7ab17f30a99d3cb96aed1d1055b",
            "0x0",
            true
        ],
    )
    .await;

    // Requesting uncle by block hash and index with missing fields
    test_rpc_call_err::<Option<Block>>(&client, "eth_getUncleByBlockHashAndIndex", rpc_params![])
        .await;

    // Requesting uncle by block hash and index with wrong fields
    test_rpc_call_err::<Option<Block>>(
        &client,
        "eth_getUncleByBlockHashAndIndex",
        rpc_params![true],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_uncle_by_block_number_and_index_rpc_call() {
    base_common_observability_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    // Requesting uncle by block number and index with proper fields
    test_rpc_call_ok::<Option<Block>>(
        &client,
        "eth_getUncleByBlockNumberAndIndex",
        rpc_params!["0x29c", "0x0"],
    )
    .await;

    // Requesting uncle by block number and index with additional fields
    test_rpc_call_ok::<Option<Block>>(
        &client,
        "eth_getUncleByBlockNumberAndIndex",
        rpc_params!["0x29c", "0x0", true],
    )
    .await;

    // Requesting uncle by block number and index with missing fields
    test_rpc_call_err::<Option<Block>>(&client, "eth_getUncleByBlockNumberAndIndex", rpc_params![])
        .await;

    // Requesting uncle by block number and index with wrong fields
    test_rpc_call_err::<Option<Block>>(
        &client,
        "eth_getUncleByBlockNumberAndIndex",
        rpc_params![true],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_transaction_by_hash_rpc_call() {
    base_common_observability_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    // Requesting transaction by hash with proper fields
    test_rpc_call_ok::<Option<Transaction>>(
        &client,
        "eth_getTransactionByHash",
        rpc_params!["0x88df016429689c079f3b2f6ad39fa052532c56795b733da78a91ebe6a713944b"],
    )
    .await;

    // Requesting transaction by hash with additional fields
    test_rpc_call_ok::<Option<Transaction>>(
        &client,
        "eth_getTransactionByHash",
        rpc_params!["0x88df016429689c079f3b2f6ad39fa052532c56795b733da78a91ebe6a713944b", true],
    )
    .await;

    // Requesting transaction by hash with missing fields
    test_rpc_call_err::<Option<Transaction>>(&client, "eth_getTransactionByHash", rpc_params![])
        .await;

    // Requesting transaction by hash with wrong fields
    test_rpc_call_err::<Option<Transaction>>(
        &client,
        "eth_getTransactionByHash",
        rpc_params![true],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_transaction_by_block_hash_and_index_rpc_call() {
    base_common_observability_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    // Requesting transaction by block hash and index with proper fields
    test_rpc_call_ok::<Option<Transaction>>(
        &client,
        "eth_getTransactionByBlockHashAndIndex",
        rpc_params!["0x1dad0df7c027bcc86d06b3a6709ff78decd732c37b73123453ba7d9463eae60d", "0x0"],
    )
    .await;

    // Requesting transaction by block hash and index with additional fields
    test_rpc_call_ok::<Option<Transaction>>(
        &client,
        "eth_getTransactionByBlockHashAndIndex",
        rpc_params![
            "0x1dad0df7c027bcc86d06b3a6709ff78decd732c37b73123453ba7d9463eae60d",
            "0x0",
            true
        ],
    )
    .await;

    // Requesting transaction by block hash and index with missing fields
    test_rpc_call_err::<Option<Transaction>>(
        &client,
        "eth_getTransactionByBlockHashAndIndex",
        rpc_params![],
    )
    .await;

    // Requesting transaction by block hash and index with wrong fields
    test_rpc_call_err::<Option<Transaction>>(
        &client,
        "eth_getTransactionByBlockHashAndIndex",
        rpc_params![true],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_transaction_by_block_number_and_index_rpc_call() {
    base_common_observability_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    // Requesting transaction by block number and index with proper fields
    test_rpc_call_ok::<Option<Transaction>>(
        &client,
        "eth_getTransactionByBlockNumberAndIndex",
        rpc_params!["0x29c", "0x0"],
    )
    .await;

    // Requesting transaction by block number and index with additional fields
    test_rpc_call_ok::<Option<Transaction>>(
        &client,
        "eth_getTransactionByBlockNumberAndIndex",
        rpc_params!["0x29c", "0x0", true],
    )
    .await;

    // Requesting transaction by block number and index with missing fields
    test_rpc_call_err::<Option<Transaction>>(
        &client,
        "eth_getTransactionByBlockNumberAndIndex",
        rpc_params![],
    )
    .await;

    // Requesting transaction by block number and index with wrong fields
    test_rpc_call_err::<Option<Transaction>>(
        &client,
        "eth_getTransactionByBlockNumberAndIndex",
        rpc_params![true],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_transaction_receipt_rpc_call() {
    base_common_observability_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    // Requesting transaction receipt by transaction hash with proper fields
    test_rpc_call_ok::<Option<TransactionReceipt>>(
        &client,
        "eth_getTransactionReceipt",
        rpc_params!["0x85d995eba9763907fdf35cd2034144dd9d53ce32cbec21349d4b12823c6860c5"],
    )
    .await;

    // Requesting transaction receipt by transaction hash with additional fields
    test_rpc_call_ok::<Option<TransactionReceipt>>(
        &client,
        "eth_getTransactionReceipt",
        rpc_params!["0x85d995eba9763907fdf35cd2034144dd9d53ce32cbec21349d4b12823c6860c5", true],
    )
    .await;

    // Requesting transaction receipt by transaction hash with missing fields
    test_rpc_call_err::<Option<TransactionReceipt>>(
        &client,
        "eth_getTransactionReceipt",
        rpc_params![],
    )
    .await;

    // Requesting transaction receipt by transaction hash with wrong fields
    test_rpc_call_err::<Option<TransactionReceipt>>(
        &client,
        "eth_getTransactionReceipt",
        rpc_params![true],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_balance_rpc_call() {
    base_common_observability_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    // Vec of block number items
    let block_number = vec!["latest", "earliest", "pending", "0x0"];

    // Iterate over test cases
    for param in block_number {
        // Requesting balance by address with proper fields
        test_rpc_call_ok::<U256>(
            &client,
            "eth_getBalance",
            rpc_params!["0x407d73d8a49eeb85d32cf465507dd71d507100c1", param],
        )
        .await;
    }

    // Requesting balance by address with additional fields
    test_rpc_call_ok::<U256>(
        &client,
        "eth_getBalance",
        rpc_params!["0x407d73d8a49eeb85d32cf465507dd71d507100c1", "latest", true],
    )
    .await;

    // Requesting balance by address without block number which is optional
    test_rpc_call_ok::<U256>(
        &client,
        "eth_getBalance",
        rpc_params!["0x407d73d8a49eeb85d32cf465507dd71d507100c1"],
    )
    .await;

    // Requesting balance by address with no field
    test_rpc_call_err::<U256>(&client, "eth_getBalance", rpc_params![]).await;

    // Requesting balance by address with wrong fields
    test_rpc_call_err::<U256>(
        &client,
        "eth_getBalance",
        rpc_params![true], // Incorrect parameters
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_storage_at_rpc_call() {
    base_common_observability_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    // Vec of block number items
    let block_number = vec!["latest", "earliest", "pending", "0x0"];

    // Iterate over test cases
    for param in block_number {
        // Requesting storage at a given address with proper fields
        test_rpc_call_ok::<Bytes>(
            &client,
            "eth_getStorageAt",
            rpc_params![
                "0x295a70b2de5e3953354a6a8344e616ed314d7251", // Address
                "0x0",                                        // Position in the storage
                param                                         // Block number or tag
            ],
        )
        .await;
    }

    // Requesting storage at a given address with additional fields
    test_rpc_call_ok::<Bytes>(
        &client,
        "eth_getStorageAt",
        rpc_params![
            "0x295a70b2de5e3953354a6a8344e616ed314d7251", // Address
            "0x0",                                        // Position in the storage
            "latest",                                     // Block number or tag
            true                                          // Additional field
        ],
    )
    .await;

    // Requesting storage at a given address without block number which is optional
    test_rpc_call_ok::<Bytes>(
        &client,
        "eth_getStorageAt",
        rpc_params![
            "0x295a70b2de5e3953354a6a8344e616ed314d7251", // Address
            "0x0"                                         // Position in the storage
        ],
    )
    .await;

    // Requesting storage at a given address with no field
    test_rpc_call_err::<Bytes>(&client, "eth_getStorageAt", rpc_params![]).await;

    // Requesting storage at a given address with wrong fields
    test_rpc_call_err::<Bytes>(
        &client,
        "eth_getStorageAt",
        rpc_params![
            "0x295a70b2de5e3953354a6a8344e616ed314d7251", // Address
            "0x0",                                        // Position in the storage
            "not_valid_block_number"                      // Block number or tag
        ],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_get_transaction_count_rpc_call() {
    base_common_observability_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    // Vec of block number items
    let block_number = vec!["latest", "earliest", "pending", "0x0"];

    // Iterate over test cases
    for param in block_number {
        // Requesting transaction count by address with proper fields
        test_rpc_call_ok::<U256>(
            &client,
            "eth_getTransactionCount",
            rpc_params![
                "0x407d73d8a49eeb85d32cf465507dd71d507100c1", // Address
                param                                         // Block number or tag
            ],
        )
        .await;
    }

    // Requesting transaction count by address with additional fields
    test_rpc_call_ok::<U256>(
        &client,
        "eth_getTransactionCount",
        rpc_params![
            "0x407d73d8a49eeb85d32cf465507dd71d507100c1", // Address
            "latest",                                     // Block number or tag
            true                                          // Additional field
        ],
    )
    .await;

    // Requesting transaction count by address without block number which is optional
    test_rpc_call_ok::<U256>(
        &client,
        "eth_getTransactionCount",
        rpc_params![
            "0x407d73d8a49eeb85d32cf465507dd71d507100c1" // Address
        ],
    )
    .await;

    // Requesting transaction count by address with no field
    test_rpc_call_err::<U256>(&client, "eth_getTransactionCount", rpc_params![]).await;

    // Requesting transaction count by address with wrong fields
    test_rpc_call_err::<U256>(
        &client,
        "eth_getTransactionCount",
        rpc_params!["0x407d73d8a49eeb85d32cf465507dd71d507100c1", "not_valid_block_number"], // Incorrect parameters
    )
    .await;
}

#[test]
fn test_rpc_registry_basic() {
    let rpc_string = RawRpcParamsBuilder::default()
        .method("eth_getBalance")
        .add_param("0xaa00000000000000000000000000000000000000")
        .add_param("0x898753d8fdd8d92c1907ca21e68c7970abd290c647a202091181deec3f30a0b2")
        .set_id(1)
        .build();

    let expected = r#"{"jsonrpc":"2.0","id":1,"method":"eth_getBalance","params":["0xaa00000000000000000000000000000000000000","0x898753d8fdd8d92c1907ca21e68c7970abd290c647a202091181deec3f30a0b2"]}"#;
    assert_eq!(rpc_string, expected, "RPC string did not match expected format.");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_eth_fee_history_raw() {
    base_common_observability_tracing::init_test_tracing();

    // Launch HTTP server with the specified RPC module
    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    // Requesting block by number with proper fields
    test_rpc_call_ok::<Option<FeeHistory>>(
        &client,
        "eth_feeHistory",
        rpc_params!["0x0", "latest", [0]],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_debug_db_get() {
    base_common_observability_tracing::init_test_tracing();

    let handle = launch_http().await;
    let client = handle.http_client().unwrap();

    let valid_test_cases = [
        "0x630000000000000000000000000000000000000000000000000000000000000000",
        "c00000000000000000000000000000000",
    ];

    for key in valid_test_cases {
        DebugApiClient::debug_db_get(&client, key.into()).await.unwrap();
    }

    // Invalid test cases
    let test_cases = [
        ("0x0000", "Key must be 33 bytes, got 2"),
        ("00", "Key must be 33 bytes, got 2"),
        (
            "0x000000000000000000000000000000000000000000000000000000000000000000",
            "Key prefix must be 0x63",
        ),
        ("000000000000000000000000000000000", "Key prefix must be 0x63"),
        ("0xc0000000000000000000000000000000000000000000000000000000000000000", "Invalid hex key"),
    ];

    let match_error_msg = |err: jsonrpsee::core::client::Error, expected: String| -> bool {
        match err {
            jsonrpsee::core::client::Error::Call(error_obj) => {
                error_obj.code() == ErrorCode::InvalidParams.code()
                    && error_obj.message() == expected
            }
            _ => false,
        }
    };

    for (key, expected) in test_cases {
        let err = DebugApiClient::debug_db_get(&client, key.into()).await.unwrap_err();
        assert!(match_error_msg(err, expected.into()));
    }
}
