//! Shared test utilities for exercising the upgrade signal reader against a mock L1 endpoint.

use alloy_primitives::{B256, Bytes, hex};
use alloy_rpc_types_eth::Block;
use alloy_sol_types::SolCall;
use httpmock::{Mock, prelude::*};

use crate::contract::IProtocolVersions;

/// Mock L1 JSON-RPC endpoint builders for upgrade signal reads.
#[derive(Debug)]
pub struct MockL1;

impl MockL1 {
    /// Serves a finalized block plus a `getSchedule` / `minimumProtocolVersion` pair over a mock
    /// L1 endpoint.
    ///
    /// The two contract calls are matched by ABI selector so a read observes the exact
    /// `getSchedule` return supplied here; `minimumProtocolVersion` returns zero. This is the mock
    /// triple shared across the reader's schedule-read tests.
    pub async fn schedule_server(getschedule_return: Vec<u8>) -> MockServer {
        let server = Self::block_and_min_protocol_server().await;
        Self::mock_get_schedule(&server, getschedule_return).await;
        server
    }

    /// Serves a finalized block and a zero `minimumProtocolVersion` over a mock L1 endpoint,
    /// without a `getSchedule` mock.
    ///
    /// Register the `getSchedule` response separately with [`Self::mock_get_schedule`] when the
    /// test needs the mock handle (to assert call counts or swap the response mid-run).
    pub async fn block_and_min_protocol_server() -> MockServer {
        let server = MockServer::start_async().await;
        let block_hash = B256::repeat_byte(1);
        let mut block: Block = Block::default();
        block.header.hash = block_hash;
        block.header.inner.number = 42;
        server
            .mock_async(|when, then| {
                when.method(POST).path("/").body_includes("eth_getBlockByNumber");
                then.json_body(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 0,
                    "result": block,
                }));
            })
            .await;
        let minimum_protocol_version = Bytes::from(vec![0_u8; 32]);
        server
            .mock_async(|when, then| {
                when.method(POST).path("/").body_includes(hex::encode(
                    IProtocolVersions::minimumProtocolVersionCall::SELECTOR,
                ));
                then.json_body(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 0,
                    "result": minimum_protocol_version,
                }));
            })
            .await;
        server
    }

    /// Registers a `getSchedule` mock returning `getschedule_return`, matched by ABI selector, and
    /// returns its handle so tests can assert call counts or swap the response mid-run.
    pub async fn mock_get_schedule(server: &MockServer, getschedule_return: Vec<u8>) -> Mock<'_> {
        let getschedule_return = Bytes::from(getschedule_return);
        server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/")
                    .body_includes(hex::encode(IProtocolVersions::getScheduleCall::SELECTOR));
                then.json_body(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 0,
                    "result": getschedule_return,
                }));
            })
            .await
    }
}
