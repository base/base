use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::{ProviderBuilder, RootProvider};
use alloy_signer_local::PrivateKeySigner;
use anyhow::Result;
use op_alloy_network::{Optimism, TxSignerSync, eip2718::Encodable2718};

/// Create an Optimism RPC provider from a URL string
///
/// This is a convenience function to avoid repeating the provider setup
/// pattern across tests and runner code.
pub fn create_optimism_provider(url: &str) -> Result<RootProvider<Optimism>> {
    Ok(ProviderBuilder::new()
        .disable_recommended_fillers()
        .network::<Optimism>()
        .connect_http(url.parse()?))
}

pub fn create_test_signer() -> PrivateKeySigner {
    "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
        .parse()
        .expect("Valid test private key")
}

pub fn create_funded_signer() -> PrivateKeySigner {
    // This is the same account used in justfile that has funds in builder-playground
    "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"
        .parse()
        .expect("Valid funded private key")
}

pub fn create_signed_transaction(
    signer: &PrivateKeySigner,
    to: Address,
    value: U256,
    nonce: u64,
    gas_limit: u64,
    max_fee_per_gas: u128,
) -> Result<Bytes> {
    let mut tx = TxEip1559 {
        chain_id: 13, // Local builder-playground chain ID
        nonce,
        gas_limit,
        max_fee_per_gas,
        max_priority_fee_per_gas: max_fee_per_gas / 10, // 10% of max fee as priority fee
        to: to.into(),
        value,
        access_list: Default::default(),
        input: Default::default(),
    };

    let signature = signer.sign_transaction_sync(&mut tx)?;

    let envelope = op_alloy_consensus::OpTxEnvelope::Eip1559(tx.into_signed(signature));

    let mut buf = Vec::new();
    envelope.encode_2718(&mut buf);

    Ok(Bytes::from(buf))
}

/// Create a simple load test transaction with standard defaults:
/// - value: 1000 wei (small test amount)
/// - gas_limit: 21000 (standard transfer)
/// - max_fee_per_gas: 1 gwei
pub fn create_load_test_transaction(
    signer: &PrivateKeySigner,
    to: Address,
    nonce: u64,
) -> Result<Bytes> {
    create_signed_transaction(signer, to, U256::from(1000), nonce, 21000, 1_000_000_000)
}
