//! Shared helpers for Beryl precompile action tests.

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_sol_types::SolValue;
use base_common_consensus::{BaseBlock, BaseTxEnvelope};

use crate::env::BerylTestEnv;

/// Expected output for a staticcall probe invocation.
pub(crate) struct StaticcallCase {
    label: &'static str,
    input: Vec<u8>,
    expected_word: U256,
    expected_returndata: Vec<u8>,
}

impl StaticcallCase {
    /// Creates a staticcall case from full expected ABI returndata.
    pub(crate) fn returndata(
        label: &'static str,
        input: Vec<u8>,
        expected_returndata: Vec<u8>,
    ) -> Self {
        let expected_word = first_word(&expected_returndata);
        Self { label, input, expected_word, expected_returndata }
    }

    /// Creates a staticcall case for a single-word ABI value.
    pub(crate) fn word(label: &'static str, input: Vec<u8>, expected_word: U256) -> Self {
        Self::returndata(label, input, expected_word.abi_encode())
    }

    /// Creates a staticcall case for a `bytes32` ABI value.
    pub(crate) fn bytes32(label: &'static str, input: Vec<u8>, expected: B256) -> Self {
        Self::returndata(label, input, expected.abi_encode())
    }

    /// Creates a staticcall case for a dynamic string ABI value.
    pub(crate) fn string(label: &'static str, input: Vec<u8>, expected: &str) -> Self {
        Self::returndata(label, input, expected.to_string().abi_encode())
    }
}

/// Builds an L2 block and records it for derivation replay.
pub(crate) async fn build_block_with_transactions(
    env: &mut BerylTestEnv,
    blocks: &mut Vec<(BaseBlock, u64)>,
    transactions: Vec<BaseTxEnvelope>,
) -> BaseBlock {
    let block = env.sequencer.build_next_block_with_transactions(transactions).await;
    let block_number = blocks.len() as u64 + 1;
    blocks.push((block.clone(), block_number));
    block
}

/// Deploys staticcall probes, executes all cases, and asserts the full returndata payload.
pub(crate) async fn assert_staticcall_cases(
    env: &mut BerylTestEnv,
    blocks: &mut Vec<(BaseBlock, u64)>,
    target: Address,
    cases: Vec<StaticcallCase>,
    probe_label: &str,
) {
    let mut probes = Vec::with_capacity(cases.len());
    let mut deployments = Vec::with_capacity(cases.len());
    for _ in &cases {
        let (probe, deploy) = env.deploy_staticcall_probe_tx(target);
        probes.push(probe);
        deployments.push(deploy);
    }

    let deploy_block = build_block_with_transactions(env, blocks, deployments).await;
    for index in 0..cases.len() {
        assert!(
            env.user_tx_succeeded(&deploy_block, index),
            "{probe_label} staticcall probe deployment {index} must succeed"
        );
    }

    let calls = probes
        .iter()
        .zip(cases.iter())
        .map(|(probe, case)| {
            env.call_staticcall_probe_tx(
                *probe,
                Bytes::from(case.input.clone()),
                BerylTestEnv::B20_PROBE_GAS_LIMIT,
            )
        })
        .collect();
    let call_block = build_block_with_transactions(env, blocks, calls).await;

    for (index, (probe, case)) in probes.iter().zip(cases.iter()).enumerate() {
        assert!(
            env.user_tx_succeeded(&call_block, index),
            "{} probe transaction must succeed",
            case.label
        );
        assert!(env.probe_call_succeeded(*probe), "{} staticcall must succeed", case.label);
        assert_eq!(
            env.probe_return_word(*probe),
            case.expected_word,
            "{} staticcall must return the expected first word",
            case.label
        );
        assert_eq!(
            env.probe_return_size(*probe),
            U256::from(case.expected_returndata.len()),
            "{} staticcall must return the expected byte length",
            case.label
        );
        assert_eq!(
            env.probe_return_hash(*probe),
            returndata_hash(&case.expected_returndata),
            "{} staticcall must return the expected ABI payload",
            case.label
        );
    }
}

/// Asserts a token's total supply from storage.
pub(crate) fn assert_total_supply(
    env: &BerylTestEnv,
    token: Address,
    token_label: &str,
    total_supply: u64,
) {
    assert_eq!(
        env.b20_total_supply(token),
        U256::from(total_supply),
        "{token_label} total supply must match expected value"
    );
}

/// Asserts the Alice, Bob, and Carol token balances from storage.
pub(crate) fn assert_balances(
    env: &BerylTestEnv,
    token: Address,
    token_label: &str,
    alice: u64,
    bob: u64,
    carol: u64,
) {
    assert_eq!(
        env.b20_balance(token, BerylTestEnv::alice()),
        U256::from(alice),
        "Alice {token_label} balance must match expected value"
    );
    assert_eq!(
        env.b20_balance(token, BerylTestEnv::bob()),
        U256::from(bob),
        "Bob {token_label} balance must match expected value"
    );
    assert_eq!(
        env.b20_balance(token, BerylTestEnv::carol()),
        U256::from(carol),
        "Carol {token_label} balance must match expected value"
    );
}

/// Asserts a probe returned the ABI encoding of `expected`.
pub(crate) fn assert_probe_string(env: &BerylTestEnv, probe: Address, label: &str, expected: &str) {
    assert_probe_returndata(env, probe, label, &expected.to_string().abi_encode());
}

/// ABI-encodes an address as a returned word.
pub(crate) fn word_from_address(address: Address) -> U256 {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(address.as_slice());
    U256::from_be_slice(&word)
}

fn first_word(returndata: &[u8]) -> U256 {
    let mut word = [0u8; 32];
    let copied = returndata.len().min(word.len());
    word[..copied].copy_from_slice(&returndata[..copied]);
    U256::from_be_bytes(word)
}

fn assert_probe_returndata(
    env: &BerylTestEnv,
    probe: Address,
    label: &str,
    expected_returndata: &[u8],
) {
    assert_eq!(
        env.probe_return_size(probe),
        U256::from(expected_returndata.len()),
        "{label} staticcall must return the expected byte length"
    );
    assert_eq!(
        env.probe_return_hash(probe),
        returndata_hash(expected_returndata),
        "{label} staticcall must return the expected ABI payload"
    );
}

fn returndata_hash(returndata: &[u8]) -> B256 {
    keccak256(returndata)
}
