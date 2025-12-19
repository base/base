use crate::domain::types::{ValidationResult, VersionedUserOperation};
use crate::services::interfaces::user_op_validator::UserOperationValidator;
use alloy_primitives::Address;
use alloy_provider::{Provider, RootProvider};
use async_trait::async_trait;
use op_alloy_network::Optimism;
use std::sync::Arc;
use tokio::time::{Duration, timeout};

#[derive(Debug, Clone)]
pub struct BaseNodeValidator {
    simulation_provider: Arc<RootProvider<Optimism>>,
    validate_user_operation_timeout: u64,
}

impl BaseNodeValidator {
    pub fn new(
        simulation_provider: Arc<RootProvider<Optimism>>,
        validate_user_operation_timeout: u64,
    ) -> Self {
        Self {
            simulation_provider,
            validate_user_operation_timeout,
        }
    }
}

#[async_trait]
impl UserOperationValidator for BaseNodeValidator {
    async fn validate_user_operation(
        &self,
        user_operation: &VersionedUserOperation,
        entry_point: &Address,
    ) -> anyhow::Result<ValidationResult> {
        let result = timeout(
            Duration::from_secs(self.validate_user_operation_timeout),
            self.simulation_provider
                .client()
                .request("base_validateUserOperation", (user_operation, entry_point)),
        )
        .await;

        let validation_result: ValidationResult = match result {
            Err(_) => {
                return Err(anyhow::anyhow!("Timeout on requesting validation"));
            }
            Ok(Err(e)) => {
                return Err(anyhow::anyhow!("RPC error: {e}"));
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

    fn new_test_user_operation_v06() -> VersionedUserOperation {
        VersionedUserOperation::UserOperation(UserOperation {
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

    fn new_validator(mock_server: &MockServer) -> BaseNodeValidator {
        let provider: RootProvider<Optimism> =
            RootProvider::new_http(mock_server.uri().parse().unwrap());
        let simulation_provider = Arc::new(provider);
        BaseNodeValidator::new(simulation_provider, VALIDATION_TIMEOUT_SECS)
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

        let validator = new_validator(&mock_server);
        let user_operation = new_test_user_operation_v06();

        let result = validator
            .validate_user_operation(&user_operation, &Address::ZERO)
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

        let validator = new_validator(&mock_server);
        let user_operation = new_test_user_operation_v06();

        let result = validator
            .validate_user_operation(&user_operation, &Address::ZERO)
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
                 "valid": true,
                 "reason": null,
                 "valid_until": null,
                 "valid_after": null,
                 "context": null
                }
            })))
            .mount(&mock_server)
            .await;

        let validator = new_validator(&mock_server);
        let user_operation = new_test_user_operation_v06();

        let result = validator
            .validate_user_operation(&user_operation, &Address::ZERO)
            .await
            .unwrap();

        assert_eq!(result.valid, true);
    }
}
