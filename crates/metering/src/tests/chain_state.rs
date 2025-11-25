use alloy_consensus::Receipt;
use alloy_eips::{BlockNumberOrTag, Encodable2718};
use alloy_primitives::{B256, Bytes, U256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::TransactionInput;
use alloy_sol_types::{SolCall, sol};
use base_reth_flashblocks_rpc::rpc::FlashblocksAPI;
use base_reth_test_utils::{flashblocks_harness::FlashblocksHarness, node::BASE_CHAIN_ID};
use eyre::{Result, eyre};
use hex_literal::hex;
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_rpc_types::OpTransactionRequest;
use reth::providers::HeaderProvider;
use reth_optimism_primitives::{OpReceipt, OpTransactionSigned};
use reth_primitives::TransactionSigned;
use reth_transaction_pool::test_utils::TransactionBuilder;
use tips_core::types::Bundle;

use super::utils::secret_from_hex;
use crate::rpc::{MeteringApiImpl, MeteringApiServer};

#[tokio::test]
async fn meter_bundle_simulation_reflects_pending_state() -> Result<()> {
    reth_tracing::init_test_tracing();
    let harness = FlashblocksHarness::new().await?;

    let provider = harness.provider();
    let alice = &harness.accounts().alice;
    let alice_secret = secret_from_hex(alice.private_key);

    // Deploy the Counter contract (nonce 0)
    let deploy_signed = TransactionBuilder::default()
        .signer(alice_secret)
        .chain_id(BASE_CHAIN_ID)
        .nonce(0)
        .gas_limit(DEPLOY_GAS_LIMIT)
        .max_fee_per_gas(GWEI)
        .max_priority_fee_per_gas(GWEI)
        .input(COUNTER_CREATION_BYTECODE.to_vec())
        .into_eip1559();
    let (deploy_envelope, deploy_bytes) = envelope_from_signed(deploy_signed);
    harness.build_block_from_transactions(vec![deploy_bytes]).await?;

    let deploy_receipt = provider
        .get_transaction_receipt(deploy_envelope.tx_hash())
        .await?
        .ok_or_else(|| eyre!("deployment transaction missing receipt"))?;
    let contract_address = deploy_receipt
        .inner
        .contract_address
        .ok_or_else(|| eyre!("deployment receipt missing contract address"))?;

    // Mutate storage on-chain via setNumber (nonce 1)
    let set_call = Counter::setNumberCall { newNumber: U256::from(42u64) };
    let set_signed = TransactionBuilder::default()
        .signer(alice_secret)
        .chain_id(BASE_CHAIN_ID)
        .nonce(1)
        .gas_limit(CALL_GAS_LIMIT)
        .max_fee_per_gas(GWEI)
        .max_priority_fee_per_gas(GWEI)
        .to(contract_address)
        .input(Bytes::from(set_call.abi_encode()))
        .into_eip1559();
    let (_set_envelope, set_bytes) = envelope_from_signed(set_signed);
    harness.build_block_from_transactions(vec![set_bytes]).await?;

    // Meter an increment call (nonce 2) after the storage change
    let increment_call = Counter::incrementCall {};
    let increment_signed = TransactionBuilder::default()
        .signer(alice_secret)
        .chain_id(BASE_CHAIN_ID)
        .nonce(2)
        .gas_limit(CALL_GAS_LIMIT)
        .max_fee_per_gas(GWEI)
        .max_priority_fee_per_gas(GWEI)
        .to(contract_address)
        .input(Bytes::from(increment_call.abi_encode()))
        .into_eip1559();
    let (_increment_envelope, increment_bytes) = envelope_from_signed(increment_signed.clone());

    let bundle = Bundle {
        txs: vec![increment_bytes.clone()],
        block_number: provider.get_block_number().await?,
        flashblock_number_min: None,
        flashblock_number_max: None,
        min_timestamp: None,
        max_timestamp: None,
        reverting_tx_hashes: vec![],
        replacement_uuid: None,
        dropping_tx_hashes: vec![],
    };

    let metering_api =
        MeteringApiImpl::new(harness.blockchain_provider(), harness.flashblocks_state());
    let response = MeteringApiServer::meter_bundle(&metering_api, bundle)
        .await
        .map_err(|err| eyre!("meter_bundle rpc failed: {}", err))?;

    assert_eq!(response.results.len(), 1);
    let result = &response.results[0];
    assert_eq!(result.to_address, Some(contract_address));
    assert!(result.gas_used > 0);
    assert!(response.state_flashblock_index.is_none());

    // Confirm canonical storage remains at 42 (increment transaction only simulated)
    let number_call = Counter::numberCall {};
    let call_request = OpTransactionRequest::default()
        .from(alice.address)
        .to(contract_address)
        .input(TransactionInput::new(Bytes::from(number_call.abi_encode())));
    let raw_number = provider.call(call_request).block(BlockNumberOrTag::Latest.into()).await?;
    let decoded: U256 = Counter::numberCall::abi_decode_returns(raw_number.as_ref())?;
    assert_eq!(decoded, U256::from(42u64));

    // Execute the increment on-chain to confirm the transaction is valid when mined
    harness.build_block_from_transactions(vec![increment_bytes]).await?;
    let number_after_increment = provider
        .call(
            OpTransactionRequest::default()
                .from(alice.address)
                .to(contract_address)
                .input(TransactionInput::new(Bytes::from(number_call.abi_encode()))),
        )
        .block(BlockNumberOrTag::Latest.into())
        .await?;
    let decoded_after_increment: U256 =
        Counter::numberCall::abi_decode_returns(number_after_increment.as_ref())?;
    assert_eq!(decoded_after_increment, U256::from(43u64));

    Ok(())
}

#[tokio::test]
async fn meter_bundle_errors_when_beacon_root_missing() -> Result<()> {
    reth_tracing::init_test_tracing();
    let harness = FlashblocksHarness::new().await?;

    let provider = harness.provider();
    let alice = &harness.accounts().alice;
    let alice_secret = secret_from_hex(alice.private_key);

    // Deploy the contract so the pending flashblock can interact with storage
    let deploy_signed = TransactionBuilder::default()
        .signer(alice_secret)
        .chain_id(BASE_CHAIN_ID)
        .nonce(0)
        .gas_limit(DEPLOY_GAS_LIMIT)
        .max_fee_per_gas(GWEI)
        .max_priority_fee_per_gas(GWEI)
        .input(COUNTER_CREATION_BYTECODE.to_vec())
        .into_eip1559();
    let (deploy_envelope, deploy_bytes) = envelope_from_signed(deploy_signed);
    harness.build_block_from_transactions(vec![deploy_bytes]).await?;
    let contract_address = provider
        .get_transaction_receipt(deploy_envelope.tx_hash())
        .await?
        .ok_or_else(|| eyre!("deployment transaction missing receipt"))?
        .inner
        .contract_address
        .ok_or_else(|| eyre!("deployment receipt missing contract address"))?;

    let blockchain_provider = harness.blockchain_provider();
    let latest_number = provider.get_block_number().await?;
    let latest_header = blockchain_provider
        .sealed_header(latest_number)?
        .ok_or_else(|| eyre!("missing header for block {}", latest_number))?;

    let pending_block_number = latest_header.number + 1;
    let flash_call = Counter::setNumberCall { newNumber: U256::from(99u64) };
    let flash_signed = TransactionBuilder::default()
        .signer(alice_secret)
        .chain_id(BASE_CHAIN_ID)
        .nonce(1)
        .gas_limit(CALL_GAS_LIMIT)
        .max_fee_per_gas(GWEI)
        .max_priority_fee_per_gas(GWEI)
        .to(contract_address)
        .input(Bytes::from(flash_call.abi_encode()))
        .into_eip1559();
    let (flash_envelope, flash_bytes) = envelope_from_signed(flash_signed);
    let receipt = OpReceipt::Eip1559(Receipt {
        status: true.into(),
        cumulative_gas_used: 80_000,
        logs: vec![],
    });

    // Pending flashblock omits the beacon root, so metering will report the validation error.
    let flashblock = harness.build_flashblock(
        pending_block_number,
        latest_header.hash(),
        B256::ZERO,
        latest_header.timestamp + 2,
        latest_header.gas_limit,
        vec![(flash_bytes.clone(), Some((flash_envelope.tx_hash(), receipt.clone())))],
    );

    harness.send_flashblock(flashblock).await?;

    let bundle = Bundle {
        txs: vec![flash_bytes.clone()],
        block_number: pending_block_number,
        flashblock_number_min: None,
        flashblock_number_max: None,
        min_timestamp: None,
        max_timestamp: None,
        reverting_tx_hashes: vec![],
        replacement_uuid: None,
        dropping_tx_hashes: vec![],
    };

    let metering_api =
        MeteringApiImpl::new(blockchain_provider.clone(), harness.flashblocks_state());
    let result = MeteringApiServer::meter_bundle(&metering_api, bundle).await;
    let err = result.expect_err("pending flashblock metering should surface missing beacon root");
    assert!(
        err.message().contains("parent beacon block root missing"),
        "unexpected error: {err:?}"
    );

    let pending_blocks = harness.flashblocks_state().get_pending_blocks();
    assert!(pending_blocks.is_some(), "expected flashblock to populate pending state");
    assert_eq!(pending_blocks.as_ref().unwrap().latest_flashblock_index(), 0);

    // Pending state should reflect the storage change even though the simulation failed.
    let number_call = Counter::numberCall {};
    let pending_value = provider
        .call(
            OpTransactionRequest::default()
                .from(alice.address)
                .to(contract_address)
                .input(TransactionInput::new(Bytes::from(number_call.abi_encode()))),
        )
        .block(BlockNumberOrTag::Pending.into())
        .await?;
    let decoded_pending: U256 = Counter::numberCall::abi_decode_returns(pending_value.as_ref())?;
    assert_eq!(decoded_pending, U256::from(99u64));

    Ok(())
}

sol! {
    contract Counter {
        function setNumber(uint256 newNumber);
        function increment();
        function number() view returns (uint256);
    }
}

const COUNTER_CREATION_BYTECODE: &[u8] = &hex!(
    "6080604052348015600e575f5ffd5b506101e18061001c5f395ff3fe608060405234801561000f575f5ffd5b506004361061003f575f3560e01c80633fb5c1cb146100435780638381f58a1461005f578063d09de08a1461007d575b5f5ffd5b61005d600480360381019061005891906100e4565b610087565b005b610067610090565b604051610074919061011e565b60405180910390f35b610085610095565b005b805f8190555050565b5f5481565b5f5f8154809291906100a690610164565b9190505550565b5f5ffd5b5f819050919050565b6100c3816100b1565b81146100cd575f5ffd5b50565b5f813590506100de816100ba565b92915050565b5f602082840312156100f9576100f86100ad565b5b5f610106848285016100d0565b91505092915050565b610118816100b1565b82525050565b5f6020820190506101315f83018461010f565b92915050565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52601160045260245ffd5b5f61016e826100b1565b91507fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff82036101a05761019f610137565b5b60018201905091905056fea26469706673582212204b710430bf5e9541dd320fc4eece1bf270f8b7d4835bba28f79ff7bd29904a2964736f6c634300081e0033"
);
const GWEI: u128 = 1_000_000_000;
const DEPLOY_GAS_LIMIT: u64 = 1_000_000;
const CALL_GAS_LIMIT: u64 = 150_000;

fn envelope_from_signed(tx: TransactionSigned) -> (OpTxEnvelope, Bytes) {
    let op_signed = OpTransactionSigned::Eip1559(
        tx.as_eip1559().expect("transaction should be EIP-1559").clone(),
    );
    let envelope = OpTxEnvelope::from(op_signed);
    let bytes = Bytes::from(envelope.encoded_2718());
    (envelope, bytes)
}

#[tokio::test]
async fn meter_bundle_reads_canonical_storage_without_mutation() -> Result<()> {
    reth_tracing::init_test_tracing();
    let harness = FlashblocksHarness::new().await?;
    let alice = &harness.accounts().alice;
    let alice_secret = secret_from_hex(alice.private_key);
    let mut nonce = 0u64;

    let deploy_signed = TransactionBuilder::default()
        .signer(alice_secret)
        .chain_id(BASE_CHAIN_ID)
        .nonce(nonce)
        .gas_limit(DEPLOY_GAS_LIMIT)
        .max_fee_per_gas(GWEI)
        .max_priority_fee_per_gas(GWEI)
        .input(COUNTER_CREATION_BYTECODE.to_vec())
        .into_eip1559();
    let (deploy_envelope, deploy_bytes) = envelope_from_signed(deploy_signed);

    harness.build_block_from_transactions(vec![deploy_bytes]).await?;
    nonce += 1;

    let provider = harness.provider();
    let deploy_receipt = provider
        .get_transaction_receipt(deploy_envelope.tx_hash())
        .await?
        .ok_or_else(|| eyre!("deployment transaction missing receipt"))?;
    let contract_address = deploy_receipt
        .inner
        .contract_address
        .ok_or_else(|| eyre!("deployment receipt missing contract address"))?;

    let set_call = Counter::setNumberCall { newNumber: U256::from(42u64) };
    let set_signed = TransactionBuilder::default()
        .signer(alice_secret)
        .chain_id(BASE_CHAIN_ID)
        .nonce(nonce)
        .gas_limit(CALL_GAS_LIMIT)
        .max_fee_per_gas(GWEI)
        .max_priority_fee_per_gas(GWEI)
        .to(contract_address)
        .input(Bytes::from(set_call.abi_encode()))
        .into_eip1559();
    let (_set_envelope, set_bytes) = envelope_from_signed(set_signed);
    harness.build_block_from_transactions(vec![set_bytes]).await?;
    nonce += 1;

    let increment_call = Counter::incrementCall {};
    let increment_signed = TransactionBuilder::default()
        .signer(alice_secret)
        .chain_id(BASE_CHAIN_ID)
        .nonce(nonce)
        .gas_limit(CALL_GAS_LIMIT)
        .max_fee_per_gas(GWEI)
        .max_priority_fee_per_gas(GWEI)
        .to(contract_address)
        .input(Bytes::from(increment_call.abi_encode()))
        .into_eip1559();
    let (_increment_envelope, increment_bytes) = envelope_from_signed(increment_signed);
    harness.build_block_from_transactions(vec![increment_bytes]).await?;

    nonce += 1;

    let storage_value = provider.get_storage_at(contract_address, U256::ZERO).await?;
    assert_eq!(storage_value, U256::from(43u64));

    let number_call = Counter::numberCall {};
    let call_request = OpTransactionRequest::default()
        .from(alice.address)
        .to(contract_address)
        .input(TransactionInput::new(Bytes::from(number_call.abi_encode())));
    let raw_number = provider.call(call_request).block(BlockNumberOrTag::Latest.into()).await?;
    let decoded: U256 = Counter::numberCall::abi_decode_returns(raw_number.as_ref())?;
    assert_eq!(decoded, U256::from(43u64));

    // Meter another increment (nonce 3) to ensure meter_bundle sees the persisted state.
    let meter_increment_call = Counter::incrementCall {};
    let meter_increment_signed = TransactionBuilder::default()
        .signer(alice_secret)
        .chain_id(BASE_CHAIN_ID)
        .nonce(nonce)
        .gas_limit(CALL_GAS_LIMIT)
        .max_fee_per_gas(GWEI)
        .max_priority_fee_per_gas(GWEI)
        .to(contract_address)
        .input(Bytes::from(meter_increment_call.abi_encode()))
        .into_eip1559();
    let (_meter_increment_envelope, meter_increment_bytes) =
        envelope_from_signed(meter_increment_signed.clone());

    let bundle = Bundle {
        txs: vec![meter_increment_bytes.clone()],
        block_number: provider.get_block_number().await?,
        flashblock_number_min: None,
        flashblock_number_max: None,
        min_timestamp: None,
        max_timestamp: None,
        reverting_tx_hashes: vec![],
        replacement_uuid: None,
        dropping_tx_hashes: vec![],
    };
    let metering_api =
        MeteringApiImpl::new(harness.blockchain_provider(), harness.flashblocks_state());
    let response = MeteringApiServer::meter_bundle(&metering_api, bundle)
        .await
        .map_err(|err| eyre!("meter_bundle rpc failed: {}", err))?;

    assert_eq!(response.results.len(), 1);
    let metering_result = &response.results[0];
    assert_eq!(metering_result.to_address, Some(contract_address));
    assert!(metering_result.gas_used > 0);

    // Canonical state remains unchanged by the simulation.
    let raw_number_after_sim = provider
        .call(
            OpTransactionRequest::default()
                .from(alice.address)
                .to(contract_address)
                .input(TransactionInput::new(Bytes::from(number_call.abi_encode()))),
        )
        .block(BlockNumberOrTag::Latest.into())
        .await?;
    let decoded_after_sim: U256 =
        Counter::numberCall::abi_decode_returns(raw_number_after_sim.as_ref())?;
    assert_eq!(decoded_after_sim, U256::from(43u64));

    Ok(())
}
