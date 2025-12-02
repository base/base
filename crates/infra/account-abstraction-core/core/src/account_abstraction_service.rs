use crate::types::{UserOperationRequest, UserOperationRequestValidationResult};
use alloy_provider::{Provider, RootProvider};
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use op_alloy_network::Optimism;
use reth_rpc_eth_types::EthApiError;
use std::sync::Arc;
use tokio::time::{Duration, timeout};
#[async_trait]
pub trait AccountAbstractionService: Send + Sync {
    async fn validate_user_operation(
        &self,
        user_operation: UserOperationRequest,
    ) -> RpcResult<UserOperationRequestValidationResult>;
}

#[derive(Debug, Clone)]
pub struct AccountAbstractionServiceImpl {
    simulation_provider: Arc<RootProvider<Optimism>>,
    validate_user_operation_timeout: u64,
}

#[async_trait]
impl AccountAbstractionService for AccountAbstractionServiceImpl {
    async fn validate_user_operation(
        &self,
        user_operation: UserOperationRequest,
    ) -> RpcResult<UserOperationRequestValidationResult> {
        // Steps: Reputation Service Validate
        // Steps: Base Node Validate User Operation
        self.base_node_validate_user_operation(user_operation).await
    }
}

impl AccountAbstractionServiceImpl {
    pub fn new(
        simulation_provider: Arc<RootProvider<Optimism>>,
        validate_user_operation_timeout: u64,
    ) -> Self {
        Self {
            simulation_provider,
            validate_user_operation_timeout,
        }
    }

    pub async fn base_node_validate_user_operation(
        &self,
        user_operation: UserOperationRequest,
    ) -> RpcResult<UserOperationRequestValidationResult> {
        let result = timeout(
            Duration::from_secs(self.validate_user_operation_timeout),
            self.simulation_provider
                .client()
                .request("base_validateUserOperation", (user_operation,)),
        )
        .await;

        let validation_result: UserOperationRequestValidationResult = match result {
            Err(_) => {
                return Err(
                    EthApiError::InvalidParams("Timeout on requesting validation".into())
                        .into_rpc_err(),
                );
            }
            Ok(Err(e)) => {
                return Err(EthApiError::InvalidParams(e.to_string()).into_rpc_err()); // likewise, map RPC error to your error type
            }
            Ok(Ok(v)) => v,
        };

        Ok(validation_result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloy_primitives::{Address, Bytes, U256};
    use alloy_rpc_types::erc4337::UserOperation;
    use tokio::time::Duration;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    const VALIDATION_TIMEOUT_SECS: u64 = 1;
    const LONG_DELAY_SECS: u64 = 3;

    async fn setup_mock_server() -> MockServer {
        MockServer::start().await
    }

    fn new_test_user_operation_v06() -> UserOperationRequest {
        UserOperationRequest::EntryPointV06(UserOperation {
            sender: Address::ZERO,
            nonce: U256::from(0),
            init_code: Bytes::default(),
            call_data: Bytes::default(),
            call_gas_limit: U256::from(21_000),
            verification_gas_limit: U256::from(100_000),
            pre_verification_gas: U256::from(21_000),
            max_fee_per_gas: U256::from(1_000_000_000),
            max_priority_fee_per_gas: U256::from(1_000_000_000),
            paymaster_and_data: Bytes::default(),
            signature: Bytes::default(),
        })
    }

    fn new_service(mock_server: &MockServer) -> AccountAbstractionServiceImpl {
        let provider: RootProvider<Optimism> =
            RootProvider::new_http(mock_server.uri().parse().unwrap());
        let simulation_provider = Arc::new(provider);
        AccountAbstractionServiceImpl::new(simulation_provider, VALIDATION_TIMEOUT_SECS)
    }

    #[tokio::test]
    async fn base_node_validate_user_operation_times_out() {
        let mock_server = setup_mock_server().await;

        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_delay(Duration::from_secs(LONG_DELAY_SECS)),
            )
            .mount(&mock_server)
            .await;

        let service = new_service(&mock_server);
        let user_operation = new_test_user_operation_v06();

        let result = service
            .base_node_validate_user_operation(user_operation)
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Timeout"));
    }

    #[tokio::test]
    async fn should_propagate_error_from_base_node() {
        let mock_server = setup_mock_server().await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "error": {
                    "code": -32000,
                    "message": "Internal error"
                }
            })))
            .mount(&mock_server)
            .await;

        let service = new_service(&mock_server);
        let user_operation = new_test_user_operation_v06();

        let result = service
            .base_node_validate_user_operation(user_operation)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Internal error"));
    }
    #[tokio::test]
    async fn base_node_validate_user_operation_succeeds() {
        let mock_server = setup_mock_server().await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "expirationTimestamp": 1000,
                    "gasUsed": "10000"
                }
            })))
            .mount(&mock_server)
            .await;

        let service = new_service(&mock_server);
        let user_operation = new_test_user_operation_v06();

        let result = service
            .base_node_validate_user_operation(user_operation)
            .await
            .unwrap();

        assert_eq!(result.expiration_timestamp, 1000);
        assert_eq!(result.gas_used, U256::from(10_000));
    }
}
