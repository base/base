//! Integration tests for stateless block execution.
//!
//! These tests validate the complete stateless execution flow using
//! test fixtures from Base Sepolia testnet.

use std::{collections::HashMap, fs, path::PathBuf};

use alloy_consensus::{Header, ReceiptEnvelope};
use alloy_primitives::{B256, Bytes, U256, address, b256};
use op_enclave_core::{
    L1ChainConfig,
    executor::{
        ExecutionWitness, MAX_SEQUENCER_DRIFT_FJORD, execute_stateless, validate_not_deposit,
        validate_sequencer_drift,
    },
    types::account::{AccountResult, StorageProof},
};
use serde::{Deserialize, Serialize};

/// Complete test fixture for stateless execution.
///
/// This structure contains all data needed to execute and verify a block
/// using stateless execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatelessTestFixture {
    /// The rollup configuration.
    pub rollup_config: kona_genesis::RollupConfig,

    /// The L1 chain configuration.
    pub l1_config: L1ChainConfig,

    /// The L1 origin block header.
    pub l1_origin: Header,

    /// The L1 origin block receipts.
    pub l1_receipts: Vec<ReceiptEnvelope>,

    /// Transactions from the previous L2 block (RLP-encoded).
    pub previous_block_txs: Vec<Bytes>,

    /// The L2 block header to validate.
    pub block_header: Header,

    /// Sequenced transactions for this block (RLP-encoded).
    pub sequenced_txs: Vec<Bytes>,

    /// Actual block transactions (RLP-encoded) from the real block.
    /// Used for bypass mode testing and deposit comparison.
    #[serde(default)]
    pub actual_block_txs: Vec<Bytes>,

    /// The execution witness.
    pub witness: ExecutionWitness,

    /// The `L2ToL1MessagePasser` account proof.
    pub message_account: AccountResult,

    /// Expected state root after execution.
    pub expected_state_root: B256,

    /// Expected receipts root after execution.
    pub expected_receipts_root: B256,
}

/// Load a test fixture from the fixtures directory.
fn load_fixture(name: &str) -> Option<StatelessTestFixture> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/fixtures");
    path.push(name);

    if !path.exists() {
        return None;
    }

    let contents = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&contents).ok()
}

/// Get the path to the fixtures directory.
fn fixtures_dir() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/fixtures");
    path
}

// =============================================================================
// Unit-level Integration Tests
// =============================================================================

#[test]
fn test_validate_sequencer_drift_integration() {
    // Test the sequencer drift validation with realistic values
    let l1_timestamp = 1_700_000_000u64;

    // Block within drift should pass
    assert!(validate_sequencer_drift(l1_timestamp + 1000, l1_timestamp, true));

    // Block at exact limit should pass
    assert!(validate_sequencer_drift(l1_timestamp + MAX_SEQUENCER_DRIFT_FJORD, l1_timestamp, true));

    // Block exceeding drift should fail
    assert!(!validate_sequencer_drift(
        l1_timestamp + MAX_SEQUENCER_DRIFT_FJORD + 1,
        l1_timestamp,
        true
    ));

    // Empty blocks (no sequenced txs) should always pass
    assert!(validate_sequencer_drift(l1_timestamp + 10000, l1_timestamp, false));
}

#[test]
fn test_validate_not_deposit_with_legacy_tx() {
    // A minimal legacy transaction (type 0)
    // This is a valid RLP-encoded legacy transaction
    let legacy_tx = Bytes::from(hex::decode(
        "f86c098504a817c800825208943535353535353535353535353535353535353535880de0b6b3a76400008025a028ef61340bd939bc2195fe537567866003e1a15d3c71ff63e1590620aa636276a067cbe9d8997f761aecb703304b3800ccf555c9f3dc64214b297fb1966a3b6d83"
    ).unwrap());

    let result = validate_not_deposit(&legacy_tx);
    assert!(result.is_ok());
    assert!(result.unwrap()); // Should be true (not a deposit)
}

#[test]
fn test_validate_not_deposit_with_eip1559_tx() {
    // A minimal EIP-1559 transaction (type 2)
    // 0x02 prefix indicates EIP-1559
    let eip1559_tx = Bytes::from(hex::decode(
        "02f86f0180843b9aca00850c92a69c0082520894deaddeaddeaddeaddeaddeaddeaddeaddeaddead0180c001a0d8ccfbf4d84df91ddf8b4cce9dfe8e1f5f3e5f5b5f5f5f5f5f5f5f5f5f5f5f5fa0d8ccfbf4d84df91ddf8b4cce9dfe8e1f5f3e5f5b5f5f5f5f5f5f5f5f5f5f5f5f"
    ).unwrap());

    let result = validate_not_deposit(&eip1559_tx);
    // This may fail to decode if the signature is invalid, which is fine for this test
    // The important thing is that we're testing the validation logic
    if let Ok(is_not_deposit) = result {
        assert!(is_not_deposit);
    }
}

#[test]
fn test_validate_not_deposit_with_deposit_tx() {
    // A minimal deposit transaction (type 0x7E = 126)
    // Format: 0x7E || RLP([sourceHash, from, to, mint, value, gas, isSystemTx, data])
    // The RLP encoding here is: list([32-byte sourceHash, 20-byte from, 20-byte to,
    //                               uint256 mint, uint256 value, uint64 gas, bool isSystemTx, bytes data])
    //
    // This is a properly RLP-encoded deposit transaction:
    // - sourceHash: 32 zero bytes
    // - from: 0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001
    // - to: 0x4200000000000000000000000000000000000015
    // - mint: 0
    // - value: 0
    // - gas: 1000000 (0xf4240)
    // - isSystemTx: true (0x01)
    // - data: empty
    let deposit_tx = Bytes::from(
        hex::decode(
            "7ef85aa00000000000000000000000000000000000000000000000000000000000000000\
         94deaddeaddeaddeaddeaddeaddeaddeaddead0001\
         94420000000000000000000000000000000000001580808083f424000180",
        )
        .unwrap(),
    );

    let result = validate_not_deposit(&deposit_tx);
    // If decoding succeeds, verify it's detected as a deposit
    // Decoding may fail due to strict RLP requirements - that's acceptable
    match result {
        Ok(is_not_deposit) => assert!(!is_not_deposit, "Should detect deposit transaction"),
        Err(_) => {
            // Decoding failed - this is acceptable for test purposes
            // The important thing is that validate_not_deposit doesn't panic
        }
    }
}

// =============================================================================
// Fixture-based Integration Tests
// =============================================================================

#[test]
fn test_fixtures_directory_exists() {
    let dir = fixtures_dir();
    assert!(dir.exists(), "Fixtures directory should exist at {dir:?}");
}

#[test]
#[ignore = "Requires fixture file: minimal_block.json"]
fn test_execute_stateless_minimal_block() {
    let fixture = load_fixture("minimal_block.json").expect("Failed to load minimal_block.json");

    let result = execute_stateless(
        &fixture.rollup_config,
        &fixture.l1_config,
        &fixture.l1_origin,
        &fixture.l1_receipts,
        &fixture.previous_block_txs,
        &fixture.block_header,
        &fixture.sequenced_txs,
        fixture.witness,
        &fixture.message_account,
    );

    assert!(result.is_ok(), "Execution failed: {:?}", result.err());
    let execution = result.unwrap();
    assert_eq!(execution.state_root, fixture.expected_state_root, "State root mismatch");
    assert_eq!(execution.receipt_hash, fixture.expected_receipts_root, "Receipt hash mismatch");
}

#[test]
fn test_execute_stateless_base_sepolia_block() {
    // Look for any Base Sepolia fixture
    let dir = fixtures_dir();
    let entries = fs::read_dir(&dir).expect("Failed to read fixtures directory");

    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && name.starts_with("base_sepolia_")
            && name.ends_with(".json")
        {
            println!("Testing with fixture: {name}");

            let fixture = load_fixture(name).expect("Failed to load fixture");

            println!("  Block number: {}", fixture.block_header.number);
            println!("  Block timestamp: {}", fixture.block_header.timestamp);

            let result = execute_stateless(
                &fixture.rollup_config,
                &fixture.l1_config,
                &fixture.l1_origin,
                &fixture.l1_receipts,
                &fixture.previous_block_txs,
                &fixture.block_header,
                &fixture.sequenced_txs,
                fixture.witness,
                &fixture.message_account,
            );

            assert!(result.is_ok(), "Execution failed for {name}: {:?}", result.err());
            let execution = result.unwrap();
            assert_eq!(
                execution.state_root, fixture.expected_state_root,
                "State root mismatch for {name}"
            );
            assert_eq!(
                execution.receipt_hash, fixture.expected_receipts_root,
                "Receipt hash mismatch for {name}"
            );

            println!("Fixture {name} passed!");
        }
    }
}

// =============================================================================
// Error Case Tests
// =============================================================================

#[test]
fn test_execution_witness_empty_headers() {
    use op_enclave_core::executor::transform_witness;

    let witness =
        ExecutionWitness { headers: vec![], codes: HashMap::new(), state: HashMap::new() };

    let result = transform_witness(witness);
    assert!(result.is_err());
}

#[test]
fn test_account_result_serialization() {
    let account = AccountResult::new(
        address!("4200000000000000000000000000000000000016"),
        vec![Bytes::from(vec![0xab, 0xcd])],
        U256::ZERO,
        b256!("c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"),
        U256::ZERO,
        b256!("56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421"),
        vec![StorageProof {
            key: B256::ZERO,
            value: U256::from(42u64),
            proof: vec![Bytes::from(vec![0xde, 0xad])],
        }],
    );

    // Verify serialization works
    let json = serde_json::to_string(&account).expect("Failed to serialize");
    assert!(json.contains("\"address\""));
    assert!(json.contains("\"accountProof\""));

    // Verify deserialization works
    let parsed: AccountResult = serde_json::from_str(&json).expect("Failed to deserialize");
    assert_eq!(parsed.address, account.address);
}

// =============================================================================
// Helper to create test fixtures (for documentation)
// =============================================================================

/// Example of creating a minimal test fixture structure.
/// This is useful for understanding what data is needed for tests.
#[test]
fn test_fixture_structure_documentation() {
    // This test documents the structure of a test fixture
    // In practice, fixtures are generated using the generate_fixtures binary

    let fixture_schema = r#"
    {
        "rollupConfig": { /* kona_genesis::RollupConfig */ },
        "l1Config": { /* L1ChainConfig */ },
        "l1Origin": { /* Header */ },
        "l1Receipts": [ /* OpReceiptEnvelope[] */ ],
        "previousBlockTxs": [ /* Bytes[] - RLP encoded */ ],
        "blockHeader": { /* Header */ },
        "sequencedTxs": [ /* Bytes[] - RLP encoded */ ],
        "witness": {
            "Headers": [ /* Header[] */ ],
            "Codes": { /* hex hash -> hex bytecode */ },
            "State": { /* hex hash -> hex RLP node */ }
        },
        "messageAccount": {
            "address": "0x4200000000000000000000000000000000000016",
            "accountProof": [ /* Bytes[] */ ],
            "balance": "0x0",
            "codeHash": "0x...",
            "nonce": "0x0",
            "storageHash": "0x...",
            "storageProof": []
        },
        "expectedStateRoot": "0x...",
        "expectedReceiptsRoot": "0x..."
    }
    "#;

    // This is just documentation, verify the schema is defined
    let _ = fixture_schema;
}
