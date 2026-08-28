//! Public-boundary integration tests for streamed transaction admission.

use std::collections::HashMap;

use alloy_consensus::SignableTransaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_network::TransactionBuilder;
use alloy_primitives::Signature;
use alloy_signer::SignerSync;
use base_common_rpc_types::BaseTransactionRequest;
use base_node_runner::test_utils::TestHarness;
use base_test_utils::{Account, DEVNET_CHAIN_ID};
use base_tx_ingress::{
    SubmitRequest, TransactionIngressConfig, TransactionIngressExtension, submit_response,
    transaction_ingress_service_client::TransactionIngressServiceClient,
};
use eyre::Result;
use tonic::transport::Channel;

struct TestSetup {
    _harness: TestHarness,
    client: TransactionIngressServiceClient<Channel>,
    valid_transaction: Vec<u8>,
}

impl TestSetup {
    async fn new() -> Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        drop(listener);

        let harness = TestHarness::builder()
            .with_ext::<TransactionIngressExtension>(TransactionIngressConfig::new(address))
            .build()
            .await?;
        let client = TransactionIngressServiceClient::connect(format!("http://{address}")).await?;

        let account = Account::Alice;
        let request = BaseTransactionRequest::default()
            .from(account.address())
            .transaction_type(2_u8)
            .with_gas_limit(21_000)
            .with_max_fee_per_gas(1_000_000_000)
            .with_max_priority_fee_per_gas(0)
            .with_chain_id(DEVNET_CHAIN_ID)
            .to(Account::Bob.address())
            .with_nonce(0);
        let transaction = request.build_typed_tx().expect("valid transaction request");
        let signature: Signature = account
            .signer()
            .sign_hash_sync(&transaction.signature_hash())
            .expect("test account should sign transaction");
        let valid_transaction = transaction.into_signed(signature).encoded_2718();

        Ok(Self { _harness: harness, client, valid_transaction })
    }
}

#[tokio::test]
async fn stream_correlates_accepted_and_rejected_submissions() -> Result<()> {
    let mut setup = TestSetup::new().await?;
    let submissions = tokio_stream::iter([
        SubmitRequest { request_id: 41, raw_transaction: vec![0xff] },
        SubmitRequest { request_id: 7, raw_transaction: setup.valid_transaction },
    ]);
    let mut responses = setup.client.submit(submissions).await?.into_inner();
    let mut outcomes = HashMap::new();

    while let Some(response) = responses.message().await? {
        outcomes
            .insert(response.request_id, response.outcome.expect("result must have an outcome"));
    }

    assert_eq!(outcomes.len(), 2);
    let submit_response::Outcome::Error(error) = &outcomes[&41] else {
        panic!("malformed transaction should be rejected");
    };
    assert!(!error.message.is_empty());
    let submit_response::Outcome::TransactionHash(hash) = &outcomes[&7] else {
        panic!("valid transaction should be accepted");
    };
    assert_eq!(hash.len(), 32);

    Ok(())
}
