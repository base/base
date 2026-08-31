//! Bidirectional streaming transaction submission service.

use std::pin::Pin;

use alloy_primitives::Bytes;
use base_execution_rpc::BaseEthApiError;
use reth_rpc_eth_api::helpers::EthTransactions;
use tokio::sync::mpsc;
use tokio_stream::{Stream, wrappers::ReceiverStream};
use tonic::{Request, Response, Status, Streaming};

use crate::{SubmitError, SubmitRequest, SubmitResponse, submit_response};

/// Number of completed responses that may wait for the transport to consume them.
const RESPONSE_QUEUE_CAPACITY: usize = 1024;

/// Streaming transaction ingress backed by an Ethereum RPC admission API.
#[derive(Debug, Clone)]
pub struct TransactionIngressService<EthApi> {
    eth_api: EthApi,
}

impl<EthApi> TransactionIngressService<EthApi> {
    /// Creates a transaction ingress service using `eth_api` for admission.
    pub const fn new(eth_api: EthApi) -> Self {
        Self { eth_api }
    }
}

#[tonic::async_trait]
impl<EthApi> crate::protocol::transaction_ingress_service_server::TransactionIngressService
    for TransactionIngressService<EthApi>
where
    EthApi: EthTransactions<Error = BaseEthApiError> + Clone + Send + Sync + 'static,
{
    type SubmitStream =
        Pin<Box<dyn Stream<Item = Result<SubmitResponse, Status>> + Send + 'static>>;

    async fn submit(
        &self,
        request: Request<Streaming<SubmitRequest>>,
    ) -> Result<Response<Self::SubmitStream>, Status> {
        let mut submissions = request.into_inner();
        let (responses, response_stream) = mpsc::channel(RESPONSE_QUEUE_CAPACITY);
        let eth_api = self.eth_api.clone();

        tokio::spawn(async move {
            loop {
                if responses.is_closed() {
                    break;
                }
                let submission = match submissions.message().await {
                    Ok(Some(submission)) => submission,
                    Ok(None) => break,
                    Err(error) => {
                        let _ = responses.send(Err(error)).await;
                        break;
                    }
                };

                let eth_api = eth_api.clone();
                let responses = responses.clone();
                // Admission is intentionally unconstrained, matching concurrent JSON-RPC
                // submissions from the trusted proxy ingress.
                tokio::spawn(async move {
                    let request_id = submission.request_id;
                    let outcome = match EthTransactions::send_raw_transaction(
                        &eth_api,
                        Bytes::from(submission.raw_transaction),
                    )
                    .await
                    {
                        Ok(hash) => {
                            submit_response::Outcome::TransactionHash(hash.as_slice().to_vec())
                        }
                        Err(error) => {
                            let error: jsonrpsee_types::ErrorObjectOwned = error.into();
                            submit_response::Outcome::Error(SubmitError {
                                code: error.code(),
                                message: error.message().to_owned(),
                                json_data: error.data().map(|data| data.get().as_bytes().to_vec()),
                            })
                        }
                    };
                    let result = SubmitResponse { request_id, outcome: Some(outcome) };
                    let _ = responses.send(Ok(result)).await;
                });
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(response_stream))))
    }
}
