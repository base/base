use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, U256};
use alloy_signer::SignerSync;

use super::Account;

/// Build and sign an EIP-1559 transaction
pub fn build_eip1559_tx(
    chain_id: u64,
    nonce: u64,
    to: Address,
    value: U256,
    input: Bytes,
    account: Account,
) -> Bytes {
    let tx = TxEip1559 {
        chain_id,
        nonce,
        gas_limit: 200_000,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
        to: alloy_primitives::TxKind::Call(to),
        value,
        access_list: Default::default(),
        input,
    };

    let signature = account.signer().sign_hash_sync(&tx.signature_hash()).expect("signing works");
    let signed = tx.into_signed(signature);

    signed.encoded_2718().into()
}
