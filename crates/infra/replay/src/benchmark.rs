use std::{collections::BTreeMap, hint::black_box, time::Instant};

use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_rlp::Decodable;
use alloy_trie::{Nibbles, TrieAccount};
use base_common_consensus::OpTxType;
use base_common_flz::data_gas_fjord;
use base_proof_executor::compute_receipts_root;
use base_proof_mpt::{TrieNode, TrieProvider};
use eyre::{Result, ensure};
use serde::Serialize;

use crate::{EvmWorkload, Fixture};

/// Lazily opened proof tries, reusable with any storage implementation of [`TrieProvider`].
#[derive(Clone, Debug)]
pub struct OpenedState {
    /// Parent state trie with captured account paths opened.
    pub accounts: TrieNode,
    /// Storage tries for the captured accounts.
    pub storage: BTreeMap<Address, TrieNode>,
}

impl OpenedState {
    /// Opens a fixture and verifies account/storage values and reconstructed roots.
    pub fn new(fixture: &Fixture) -> Result<Self> {
        let mut state = Self::blinded(fixture);
        state.read(fixture, &fixture.provider())?;
        ensure!(state.accounts.blind() == fixture.parent.state_root, "state root mismatch");
        for proof in &fixture.proofs {
            ensure!(
                state.storage[&proof.address].blind() == proof.storage_hash,
                "storage root mismatch"
            );
        }
        Ok(state)
    }

    /// Starts a cold traversal; witness indexing is separate from this operation.
    pub fn blinded(fixture: &Fixture) -> Self {
        Self {
            accounts: TrieNode::new_blinded(fixture.parent.state_root),
            storage: fixture
                .proofs
                .iter()
                .map(|p| (p.address, TrieNode::new_blinded(p.storage_hash)))
                .collect(),
        }
    }

    /// Reads and checks all recorded values, keeping opened nodes warm for subsequent reads.
    pub fn read<P: TrieProvider>(&mut self, fixture: &Fixture, provider: &P) -> Result<()> {
        for proof in &fixture.proofs {
            let value = self.accounts.open(&Nibbles::unpack(keccak256(proof.address)), provider)?;
            let account = match value {
                Some(bytes) => {
                    let mut input = bytes.as_ref();
                    let account = TrieAccount::decode(&mut input)?;
                    ensure!(input.is_empty(), "trailing account bytes");
                    account
                }
                None => TrieAccount::default(),
            };
            ensure!(
                account
                    == TrieAccount {
                        nonce: proof.nonce,
                        balance: proof.balance,
                        storage_root: proof.storage_hash,
                        code_hash: proof.code_hash
                    },
                "account value mismatch for {}",
                proof.address
            );
            let storage =
                self.storage.get_mut(&proof.address).expect("initialized from same fixture");
            for slot in &proof.storage_proof {
                let value =
                    storage.open(&Nibbles::unpack(keccak256(slot.key.as_b256())), provider)?;
                let value = match value {
                    Some(bytes) => {
                        let mut input = bytes.as_ref();
                        let value = U256::decode(&mut input)?;
                        ensure!(input.is_empty(), "trailing storage bytes");
                        value
                    }
                    None => U256::ZERO,
                };
                ensure!(value == slot.value, "storage value mismatch for {}", proof.address);
            }
        }
        Ok(())
    }

    /// Reconstructs roots from opened paths, including untouched blinded siblings.
    pub fn roots(&self) -> Vec<B256> {
        std::iter::once(self.accounts.blind())
            .chain(self.storage.values().map(TrieNode::blind))
            .collect()
    }
}

/// Batch-normalized wall-clock samples; nanoseconds per entire selected workload.
#[derive(Debug, Serialize)]
pub struct Measurement {
    /// Production component being measured.
    pub stage: String,
    /// Median batch time divided by iterations per batch.
    pub median_ns: f64,
    /// Tenth percentile of batch-normalized times.
    pub p10_ns: f64,
    /// Ninetieth percentile of batch-normalized times.
    pub p90_ns: f64,
    /// Raw samples for independent analysis.
    pub samples_ns: Vec<f64>,
}

impl Measurement {
    /// Warms a component, then measures repeated batches. Input setup belongs outside `run`.
    pub fn measure<T>(
        stage: &str,
        samples: u32,
        iterations: u32,
        mut run: impl FnMut() -> Result<T>,
    ) -> Result<Self> {
        ensure!(samples >= 3 && iterations > 0, "need at least 3 samples and positive iterations");
        for _ in 0..iterations {
            black_box(run()?);
        }
        let mut times = Vec::with_capacity(samples as usize);
        for _ in 0..samples {
            let start = Instant::now();
            for _ in 0..iterations {
                black_box(run()?);
            }
            times.push(start.elapsed().as_nanos() as f64 / f64::from(iterations));
        }
        let mut sorted = times.clone();
        sorted.sort_by(f64::total_cmp);
        let last = sorted.len() - 1;
        Ok(Self {
            stage: stage.into(),
            median_ns: (sorted[last / 2] + sorted[sorted.len() / 2]) / 2.0,
            p10_ns: sorted[last / 10],
            p90_ns: sorted[last * 9 / 10],
            samples_ns: times,
        })
    }
}

/// Offline component measurements. Execution and trie stages use the proof executor,
/// not the sequencer's stateful payload builder. Host timings are not TEE prover latency.
#[derive(Debug, Serialize)]
pub struct BenchmarkReport {
    /// Hash of canonical serialized fixture data, including the response tape.
    pub fixture_hash: B256,
    /// Canonical source block.
    pub block_hash: B256,
    /// Number of selected transactions per iteration.
    pub transactions: usize,
    /// Number of captured account proofs.
    pub accounts: usize,
    /// Number of captured storage accesses.
    pub storage_slots: usize,
    /// Number of deduplicated nodes in the full execution witness, if present.
    pub execution_nodes: usize,
    /// Canonical block gas, if full execution was verified.
    pub execution_gas: Option<u64>,
    /// Inner loop count for each sample.
    pub iterations: u32,
    /// Debug binaries must not be mistaken for optimized benchmark results.
    pub debug_assertions: bool,
    /// Machine architecture.
    pub arch: String,
    /// Independently measured stages.
    pub measurements: Vec<Measurement>,
}

impl BenchmarkReport {
    /// Validates the fixture, then benchmarks real production functions without RPC or disk I/O.
    pub fn run(fixture: &Fixture, samples: u32, iterations: u32) -> Result<Self> {
        fixture.validate()?;
        let payload = fixture.payload();
        let mut measurements = vec![
            Measurement::measure("payload-decode", samples, iterations, || {
                Ok(payload.decoded_transactions().collect::<Result<Vec<_>, _>>()?)
            })?,
            Measurement::measure("payload-recover", samples, iterations, || {
                Ok(payload.recovered_transactions_with_encoded().collect::<Result<Vec<_>, _>>()?)
            })?,
            // Deposits do not pay L1 fees. This explicitly measures the Fjord algorithm, not
            // historical fee totals (which also depend on fork and L1 scalar state).
            Measurement::measure("fjord-data-gas", samples, iterations, || {
                Ok(fixture
                    .transactions
                    .iter()
                    .filter(|tx| tx.encoded.first().copied() != Some(OpTxType::Deposit as u8))
                    .map(|tx| black_box(data_gas_fjord(black_box(&tx.encoded))))
                    .sum::<u64>())
            })?,
        ];
        if !fixture.proofs.is_empty() {
            let provider = fixture.provider();
            measurements.push(Measurement::measure(
                "proof-open-cold",
                samples,
                iterations,
                || {
                    let mut state = OpenedState::blinded(fixture);
                    state.read(fixture, &provider)?;
                    Ok(state)
                },
            )?);
            let mut state = OpenedState::new(fixture)?;
            measurements.push(Measurement::measure(
                "proof-read-warm",
                samples,
                iterations,
                || state.read(fixture, &provider),
            )?);
            measurements.push(Measurement::measure("proof-root", samples, iterations, || {
                Ok(state.roots())
            })?);
        }
        if let Some(witness) = &fixture.execution {
            let evm = EvmWorkload::new(fixture)?;
            let state = evm.execute()?;
            ensure!(
                state.db.clone().state_root(&state.bundle)? == fixture.header.state_root,
                "isolated EVM/state update root mismatch"
            );
            ensure!(
                compute_receipts_root(
                    &state.result.receipts,
                    &evm.config,
                    fixture.header.timestamp
                ) == fixture.header.receipts_root,
                "isolated EVM receipts mismatch"
            );
            ensure!(state.result.gas_used == fixture.header.gas_used, "isolated EVM gas mismatch");
            measurements.push(Measurement::measure(
                "proof-evm-execute",
                samples,
                iterations,
                || evm.execute(),
            )?);
            measurements.push(Measurement::measure(
                "proof-receipts-root",
                samples,
                iterations,
                || {
                    Ok(compute_receipts_root(
                        &state.result.receipts,
                        &evm.config,
                        fixture.header.timestamp,
                    ))
                },
            )?);
            measurements.push(Measurement::measure(
                "proof-storage-update-root",
                samples,
                iterations,
                || {
                    // Reset to the same opened parent trie every time, including clone/drop costs.
                    Ok(state.db.clone().state_root(&state.bundle)?)
                },
            )?);
            measurements.push(Measurement::measure(
                "proof-stateless-block",
                samples,
                iterations,
                || witness.execute(fixture),
            )?);
        }
        Ok(Self {
            fixture_hash: keccak256(serde_json::to_vec(fixture)?),
            block_hash: fixture.block_hash,
            transactions: fixture.transactions.len(),
            accounts: fixture.proofs.len(),
            storage_slots: fixture.proofs.iter().map(|p| p.storage_proof.len()).sum(),
            execution_nodes: fixture.execution.as_ref().map_or(0, |w| w.nodes.len()),
            execution_gas: fixture.execution.as_ref().map(|_| fixture.header.gas_used),
            iterations,
            debug_assertions: cfg!(debug_assertions),
            arch: std::env::consts::ARCH.into(),
            measurements,
        })
    }
}
