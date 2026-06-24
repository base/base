//! Requester for composed ZK proof flows.

use base_prover_service_client::ProofRequesterProvider;
use base_prover_service_protocol::{
    GetProofRequest, GetProofResponse, ProofResult, ProofStatus, ProofType,
    ProveBlockRangeResponse, SnarkGroth16ProofResult,
};

use crate::{Groth16RangeProofRequest, ZkProofRequesterError};

/// Accepted prover-service sessions for a requested Groth16 proof flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Groth16ProofRequestResponse {
    /// Accepted compressed range proof session.
    pub range: ProveBlockRangeResponse,
    /// Accepted Groth16 aggregation proof session.
    pub aggregation: ProveBlockRangeResponse,
}

/// Higher-level requester for ZK proof flows backed by prover-service requests.
#[derive(Debug, Clone)]
pub struct ZkProofRequester<Client> {
    client: Client,
}

impl<Client> ZkProofRequester<Client> {
    /// Create a requester around a raw prover-service requester client.
    pub const fn new(client: Client) -> Self {
        Self { client }
    }

    /// Return the underlying prover-service requester client.
    pub const fn client(&self) -> &Client {
        &self.client
    }
}

impl<Client> ZkProofRequester<Client>
where
    Client: ProofRequesterProvider,
{
    /// Request range proof and Groth16 aggregation stages.
    ///
    /// If aggregation submission fails after range acceptance, the error includes
    /// the accepted range response.
    ///
    /// This method is not cancellation-safe between the range and aggregation
    /// submissions.
    pub async fn request_groth16_proof(
        &self,
        request: &Groth16RangeProofRequest,
    ) -> Result<Groth16ProofRequestResponse, ZkProofRequesterError> {
        let range = self.client.prove_block_range(request.range_prove_block_request()).await?;
        let aggregation =
            match self.client.prove_block_range(request.aggregation_prove_block_request()).await {
                Ok(response) => response,
                Err(source) => {
                    return Err(ZkProofRequesterError::AggregationRequestFailed {
                        range_session_id: range.session_id.clone(),
                        range,
                        source,
                    });
                }
            };
        Ok(Groth16ProofRequestResponse { range, aggregation })
    }

    /// Return the completed Groth16 proof if the aggregation stage has finished.
    pub async fn groth16_result(
        &self,
        request: &Groth16RangeProofRequest,
    ) -> Result<Option<SnarkGroth16ProofResult>, ZkProofRequesterError> {
        let range_session_id = request.range_session_id();
        match self.proof_result(&range_session_id).await? {
            Some(ProofResult::Compressed(_)) => {}
            Some(result) => {
                return Err(Self::unexpected_result(
                    range_session_id,
                    &result,
                    ProofType::Compressed,
                ));
            }
            None => return Ok(None),
        }

        let session_id = request.aggregation_session_id();
        match self.proof_result(&session_id).await? {
            Some(ProofResult::SnarkGroth16(result)) => Ok(Some(result)),
            Some(result) => {
                Err(Self::unexpected_result(session_id, &result, ProofType::SnarkGroth16))
            }
            None => Ok(None),
        }
    }

    async fn proof_result(
        &self,
        session_id: &str,
    ) -> Result<Option<ProofResult>, ZkProofRequesterError> {
        let response =
            self.client.get_proof(GetProofRequest { session_id: session_id.to_owned() }).await?;
        Self::resolve_get_proof_response(session_id, response)
    }

    fn resolve_get_proof_response(
        session_id: &str,
        response: GetProofResponse,
    ) -> Result<Option<ProofResult>, ZkProofRequesterError> {
        match response.status {
            ProofStatus::Succeeded => {
                let result = response.result.ok_or_else(|| {
                    ZkProofRequesterError::MissingResult { session_id: session_id.to_owned() }
                })?;
                Ok(Some(result))
            }
            ProofStatus::Failed => {
                let message =
                    response.error_message.unwrap_or_else(|| "proof request failed".to_owned());
                Err(ZkProofRequesterError::ProofFailed {
                    session_id: session_id.to_owned(),
                    message,
                })
            }
            ProofStatus::Queued | ProofStatus::Running => Ok(None),
        }
    }

    const fn unexpected_result(
        session_id: String,
        result: &ProofResult,
        expected: ProofType,
    ) -> ZkProofRequesterError {
        let actual = match result {
            ProofResult::Compressed(_) => ProofType::Compressed,
            ProofResult::SnarkGroth16(_) => ProofType::SnarkGroth16,
            ProofResult::Tee(_) => ProofType::Tee,
        };

        ZkProofRequesterError::UnexpectedResult { session_id, expected, actual }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use alloy_primitives::{Address, Bytes};
    use async_trait::async_trait;
    use base_prover_service_client::ProverServiceClientError;
    use base_prover_service_protocol::{
        DeleteProofRequest, GetProofRequest, GetProofResponse, ListProofsRequest,
        ListProofsResponse, ProofResult, ProofStatus, ProveBlockRangeRequest,
        ProveBlockRangeResponse, SnarkGroth16ProofResult, ZkProofRequest, ZkProofResult, ZkVm,
    };

    use super::*;

    #[derive(Debug, Default)]
    struct MockProofRequester {
        submitted: Mutex<Vec<String>>,
        responses: Mutex<VecDeque<GetProofResponse>>,
        prove_outcomes: Mutex<VecDeque<ProveOutcome>>,
    }

    #[derive(Debug)]
    enum ProveOutcome {
        Success,
        Error(ProverServiceClientError),
    }

    impl MockProofRequester {
        fn with_responses(responses: impl Into<VecDeque<GetProofResponse>>) -> Self {
            Self {
                submitted: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into()),
                prove_outcomes: Mutex::new(VecDeque::new()),
            }
        }

        fn with_prove_outcomes(outcomes: impl Into<VecDeque<ProveOutcome>>) -> Self {
            Self {
                submitted: Mutex::new(Vec::new()),
                responses: Mutex::new(VecDeque::new()),
                prove_outcomes: Mutex::new(outcomes.into()),
            }
        }

        fn submitted(&self) -> Vec<String> {
            self.submitted.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ProofRequesterProvider for MockProofRequester {
        async fn prove_block_range(
            &self,
            request: ProveBlockRangeRequest,
        ) -> Result<ProveBlockRangeResponse, ProverServiceClientError> {
            self.submitted.lock().unwrap().push(request.proof.session_id.clone());
            match self.prove_outcomes.lock().unwrap().pop_front() {
                Some(ProveOutcome::Error(error)) => Err(error),
                None | Some(ProveOutcome::Success) => {
                    Ok(ProveBlockRangeResponse { session_id: request.proof.session_id })
                }
            }
        }

        async fn get_proof(
            &self,
            _request: GetProofRequest,
        ) -> Result<GetProofResponse, ProverServiceClientError> {
            Ok(self.responses.lock().unwrap().pop_front().unwrap())
        }

        async fn delete_proof_request(
            &self,
            _request: DeleteProofRequest,
        ) -> Result<(), ProverServiceClientError> {
            unreachable!("ZkProofRequester does not delete proofs")
        }

        async fn list_proofs(
            &self,
            _request: ListProofsRequest,
        ) -> Result<ListProofsResponse, ProverServiceClientError> {
            unreachable!("ZkProofRequester does not list proofs")
        }
    }

    fn proof_request() -> ZkProofRequest {
        ZkProofRequest {
            start_block_number: 10,
            number_of_blocks_to_prove: 2,
            sequence_window: None,
            l1_head: None,
            intermediate_root_interval: Some(2),
            zk_vm: ZkVm::Sp1,
        }
    }

    fn snark_result() -> ProofResult {
        ProofResult::SnarkGroth16(SnarkGroth16ProofResult {
            proof: ZkProofResult { zk_vm: ZkVm::Sp1, proof: Bytes::from(vec![2]) },
        })
    }

    fn compressed_result() -> ProofResult {
        ProofResult::Compressed(ZkProofResult { zk_vm: ZkVm::Sp1, proof: Bytes::from(vec![1]) })
    }

    fn succeeded(result: ProofResult) -> GetProofResponse {
        GetProofResponse {
            status: ProofStatus::Succeeded,
            error_message: None,
            result: Some(result),
        }
    }

    #[tokio::test]
    async fn request_groth16_proof_requests_range_then_aggregation() {
        let requester = MockProofRequester::with_responses(VecDeque::from([
            GetProofResponse { status: ProofStatus::Running, error_message: None, result: None },
            succeeded(compressed_result()),
            succeeded(snark_result()),
        ]));
        let requester = ZkProofRequester::new(requester);
        let request =
            Groth16RangeProofRequest::new("parent", proof_request(), Address::repeat_byte(0x11));

        let proof = requester.request_groth16_proof(&request).await.unwrap();
        assert_eq!(proof.range.session_id, "parent:range");
        assert_eq!(proof.aggregation.session_id, "parent:aggregation");
        assert!(requester.groth16_result(&request).await.unwrap().is_none());
        assert_eq!(
            requester.groth16_result(&request).await.unwrap().unwrap().proof.proof,
            Bytes::from(vec![2])
        );

        assert_eq!(requester.client().submitted(), vec!["parent:range", "parent:aggregation"]);
    }

    #[tokio::test]
    async fn request_groth16_proof_reports_accepted_range_on_aggregation_submit_failure() {
        let requester = MockProofRequester::with_prove_outcomes(VecDeque::from([
            ProveOutcome::Success,
            ProveOutcome::Error(ProverServiceClientError::Timeout(
                "aggregation timed out".to_owned(),
            )),
        ]));
        let requester = ZkProofRequester::new(requester);
        let request =
            Groth16RangeProofRequest::new("parent", proof_request(), Address::repeat_byte(0x11));

        let error = requester.request_groth16_proof(&request).await.unwrap_err();

        let ZkProofRequesterError::AggregationRequestFailed { range, range_session_id, source } =
            error
        else {
            panic!("expected aggregation request failure");
        };
        assert_eq!(range.session_id, "parent:range");
        assert_eq!(range_session_id, "parent:range");
        assert!(matches!(source, ProverServiceClientError::Timeout(_)));
        assert_eq!(requester.client().submitted(), vec!["parent:range", "parent:aggregation"]);
    }

    #[tokio::test]
    async fn failed_range_status_returns_error() {
        let requester = MockProofRequester::with_responses(VecDeque::from([GetProofResponse {
            status: ProofStatus::Failed,
            error_message: Some("range failed".to_owned()),
            result: None,
        }]));
        let requester = ZkProofRequester::new(requester);
        let request =
            Groth16RangeProofRequest::new("parent", proof_request(), Address::repeat_byte(0x11));

        let error = requester.groth16_result(&request).await.unwrap_err();

        assert!(matches!(error, ZkProofRequesterError::ProofFailed { .. }));
    }

    #[tokio::test]
    async fn failed_aggregation_status_returns_error_after_range_succeeds() {
        let requester = MockProofRequester::with_responses(VecDeque::from([
            succeeded(compressed_result()),
            GetProofResponse {
                status: ProofStatus::Failed,
                error_message: Some("aggregation failed".to_owned()),
                result: None,
            },
        ]));
        let requester = ZkProofRequester::new(requester);
        let request =
            Groth16RangeProofRequest::new("parent", proof_request(), Address::repeat_byte(0x11));

        let error = requester.groth16_result(&request).await.unwrap_err();

        assert!(matches!(
            error,
            ZkProofRequesterError::ProofFailed { session_id, .. } if session_id == "parent:aggregation"
        ));
    }
}
