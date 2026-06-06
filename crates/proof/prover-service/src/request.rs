//! Internal request types used by prover-service backends.

use std::collections::HashMap;

/// Internal block proving request used by ZK proving backends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProveBlockRequest {
    /// The first L2 block number to prove.
    pub start_block_number: u64,
    /// Number of consecutive L2 blocks to prove.
    pub number_of_blocks_to_prove: u64,
    /// Optional L1 sequence window used for L1-head selection.
    pub sequence_window: Option<u64>,
    /// Internal proof type discriminant.
    pub proof_type: i32,
    /// Caller-provided session ID.
    pub session_id: String,
    /// Ethereum address of the on-chain prover for Groth16 proofs.
    pub prover_address: Option<String>,
    /// Optional explicit L1 head hash.
    pub l1_head: Option<String>,
    /// Optional intermediate root interval.
    pub intermediate_root_interval: Option<u64>,
}

/// Local SP1 execution statistics produced by the dry-run backend.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionStats {
    /// Total RISC-V instruction cycles reported by SP1.
    pub total_instruction_cycles: u64,
    /// Total SP1 gas reported by SP1.
    pub total_sp1_gas: u64,
    /// Per-section cycle tracker values reported by the range program.
    pub cycle_tracker: HashMap<String, u64>,
    /// Time spent generating the witness, in milliseconds.
    pub witness_generation_ms: f64,
    /// Time spent executing the SP1 range program, in milliseconds.
    pub execution_ms: f64,
}
