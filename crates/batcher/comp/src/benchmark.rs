//! Deterministic signed-transaction channel-compression measurements.

use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;

use crate::{BrotliLevel, CompressionError, CompressionStream};

/// A synthetic transaction profile measured by the benchmark.
///
/// `encoded_bytes` is retained as the original scenario size reference. Benchmark inputs are
/// instead complete, deterministically signed EIP-1559 transactions with ABI-shaped calldata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransactionProfile {
    /// Stable identifier printed in benchmark reports.
    pub name: &'static str,
    /// Complete encoded transaction length represented by one synthetic transaction.
    pub encoded_bytes: usize,
}

/// Starting profiles from the transaction-size model.
///
/// These are inputs to a channel-compression experiment, not assertions about the encoding of a
/// particular signed transaction. Add a new [`TransactionProfile`] to measure another type.
pub const DEFAULT_TRANSACTION_PROFILES: [TransactionProfile; 8] = [
    TransactionProfile { name: "Native ETH transfer", encoded_bytes: 100 },
    TransactionProfile { name: "ERC-20 transfer (USDC)", encoded_bytes: 126 },
    TransactionProfile { name: "B20 transfer", encoded_bytes: 126 },
    TransactionProfile { name: "Uniswap V3 / aggregator swap", encoded_bytes: 261 },
    TransactionProfile { name: "Uniswap V2 swap", encoded_bytes: 288 },
    TransactionProfile { name: "x402 agentic payment", encoded_bytes: 315 },
    TransactionProfile { name: "ERC-4337 smart-wallet UserOp", encoded_bytes: 448 },
    TransactionProfile { name: "Contract deployment", encoded_bytes: 3261 },
];

/// Byte distribution used for synthetic transaction inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputPattern {
    /// Consecutive byte values (`0x00` through `0xff`) that are intentionally compressible.
    Incrementing,
    /// Deterministic pseudorandom bytes, the conservative incompressible-data case.
    Pseudorandom,
}

impl InputPattern {
    /// Stable label used in benchmark reports.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Incrementing => "incrementing",
            Self::Pseudorandom => "pseudorandom",
        }
    }
}

/// Input shape for one compressed derivation channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressionScenario {
    /// Synthetic transaction shape repeated in the channel.
    pub profile: TransactionProfile,
    /// Byte distribution for the synthetic transaction bytes.
    pub pattern: InputPattern,
    /// Number of transactions appended in each compressor write.
    pub transactions_per_batch: usize,
    /// Number of batches appended to the channel before it is finished.
    pub batches_per_channel: usize,
}

impl CompressionScenario {
    /// Returns the number of transactions represented by the scenario.
    pub const fn transaction_count(self) -> usize {
        self.transactions_per_batch.saturating_mul(self.batches_per_channel)
    }
}

/// Bytes observed after writing and finishing a synthetic derivation channel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompressionMeasurement {
    /// The scenario that was measured.
    pub scenario: CompressionScenario,
    /// Total uncompressed bytes submitted to the compressor.
    pub uncompressed_bytes: usize,
    /// Total channel bytes emitted by the compressor, including the channel-version byte.
    pub compressed_bytes: usize,
}

/// Compressed bytes emitted while appending one synthetic transaction to a channel.
///
/// Brotli may buffer input, so a transaction can emit zero bytes and a later transaction can
/// release bytes for an earlier one. The final entry includes the stream trailer emitted by
/// [`CompressionStream::finish`]. The entries therefore sum exactly to total channel bytes, but
/// are a streaming allocation rather than independently finalized-prefix deltas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncrementalCompressionMeasurement {
    /// Zero-based position of the transaction within the channel.
    pub transaction_index: usize,
    /// Raw bytes submitted for this transaction.
    pub uncompressed_bytes: usize,
    /// New stable compressed bytes emitted after appending this transaction.
    pub compressed_bytes: usize,
    /// Whether `compressed_bytes` includes the channel's final Brotli trailer.
    pub includes_final_trailer: bool,
}

impl CompressionMeasurement {
    /// Returns compressed channel bytes attributed to each synthetic transaction.
    pub fn compressed_bytes_per_transaction(self) -> f64 {
        self.compressed_bytes as f64 / self.scenario.transaction_count() as f64
    }

    /// Returns the whole-byte channel estimate attributed to each synthetic transaction.
    ///
    /// This intentionally rounds the amortized channel overhead to the nearest byte. The
    /// benchmark's persisted results use this presentation value rather than fractional header
    /// bytes such as `0.006` bytes per transaction.
    pub fn estimated_compressed_bytes_per_transaction(self) -> usize {
        self.compressed_bytes_per_transaction().round() as usize
    }

    /// Returns compressed bytes divided by uncompressed bytes.
    pub fn compression_ratio(self) -> f64 {
        self.compressed_bytes as f64 / self.uncompressed_bytes as f64
    }
}

/// Measures the production Brotli channel compressor with deterministic synthetic inputs.
#[derive(Debug, Default, Clone, Copy)]
pub struct CompressionBenchmark;

impl CompressionBenchmark {
    /// Generates and signs one complete EIP-1559 transaction for `profile`.
    ///
    /// The fixtures need not execute against a live chain: compression receives signed EIP-2718
    /// bytes. Nonces cycle over `0..100_000`, native transfers carry a deterministic random
    /// value below 10 ETH, and contract calls carry zero value, matching typical transaction
    /// shapes.
    pub fn synthetic_transaction(
        self,
        profile: TransactionProfile,
        pattern: InputPattern,
        transaction_index: usize,
    ) -> Vec<u8> {
        let (to, value, input) = self.fixture_call(profile, pattern, transaction_index);
        let transaction = TxEip1559 {
            chain_id: 8453,
            nonce: (transaction_index % 100_000) as u64,
            gas_limit: 300_000,
            max_fee_per_gas: 1_000_000,
            max_priority_fee_per_gas: 1_000,
            to,
            value,
            access_list: Default::default(),
            input: Bytes::from(input),
        };
        let signer =
            PrivateKeySigner::from_bytes(&B256::repeat_byte(1)).expect("valid fixture key");
        let signature =
            signer.sign_hash_sync(&transaction.signature_hash()).expect("fixture signing succeeds");
        transaction.into_signed(signature).encoded_2718()
    }

    /// Builds ABI-shaped calldata for a scenario without requiring a deployed contract.
    fn fixture_call(
        self,
        profile: TransactionProfile,
        pattern: InputPattern,
        transaction_index: usize,
    ) -> (TxKind, U256, Vec<u8>) {
        if profile.name == "Native ETH transfer" {
            let mut state = transaction_index as u64 ^ 0xa076_1d64_78bd_642f;
            let mut recipient = [0u8; 20];
            for byte in &mut recipient {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = state as u8;
            }
            let mut value_state = transaction_index as u64 ^ 0xe703_7ed1_a0b4_28db;
            value_state ^= value_state << 13;
            value_state ^= value_state >> 7;
            value_state ^= value_state << 17;
            return (
                TxKind::Call(Address::from(recipient)),
                U256::from((value_state % 10_000_000_000_000_000_000u64).max(1)),
                Vec::new(),
            );
        }

        let (selector, calldata_len, target) = match profile.name {
            "ERC-20 transfer (USDC)" | "B20 transfer" => ([0xa9, 0x05, 0x9c, 0xbb], 68, 0x11),
            "Uniswap V3 / aggregator swap" => ([0x04, 0xe4, 0x5a, 0xaf], 228, 0x22),
            "Uniswap V2 swap" => ([0x38, 0xed, 0x17, 0x39], 260, 0x33),
            "x402 agentic payment" => ([0xe3, 0xee, 0x16, 0x0e], 292, 0x44),
            "ERC-4337 smart-wallet UserOp" => ([0x1f, 0xad, 0x94, 0xe3], 450, 0x55),
            "Contract deployment" => ([0x60, 0x00, 0x60, 0x00], 3800, 0),
            _ => ([0, 0, 0, 0], profile.encoded_bytes, 0x66),
        };
        let mut input = Vec::with_capacity(calldata_len);
        input.extend_from_slice(&selector);
        let mut state = transaction_index as u64 ^ 0x9e37_79b9_7f4a_7c15;
        while input.len() < calldata_len {
            let byte = match pattern {
                InputPattern::Incrementing => input.len() as u8,
                InputPattern::Pseudorandom => {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                }
            };
            input.push(byte);
        }
        let to = if profile.name == "Contract deployment" {
            TxKind::Create
        } else {
            TxKind::Call(Address::repeat_byte(target))
        };
        (to, U256::ZERO, input)
    }

    /// Compresses every synthetic batch as one streaming derivation channel.
    ///
    /// The benchmark feeds each batch through [`CompressionStream::append`] so that cross-batch
    /// back-references match the production channel compressor. It measures only compressed
    /// channel bytes, not blob framing or L1 transaction calldata overhead.
    pub fn measure(
        self,
        scenario: CompressionScenario,
    ) -> Result<CompressionMeasurement, CompressionError> {
        assert!(scenario.transactions_per_batch > 0, "transactions per batch must be nonzero");
        assert!(scenario.batches_per_channel > 0, "batches per channel must be nonzero");

        let mut compressor = CompressionStream::new(BrotliLevel::DEFAULT);
        let batch_capacity =
            scenario.profile.encoded_bytes.saturating_mul(scenario.transactions_per_batch);
        let mut uncompressed_bytes = 0;
        let mut compressed_bytes = 0;

        for batch_index in 0..scenario.batches_per_channel {
            let mut batch = Vec::with_capacity(batch_capacity);
            for transaction_index in 0..scenario.transactions_per_batch {
                let transaction_number = batch_index
                    .saturating_mul(scenario.transactions_per_batch)
                    .saturating_add(transaction_index);
                batch.extend(self.synthetic_transaction(
                    scenario.profile,
                    scenario.pattern,
                    transaction_number,
                ));
            }

            uncompressed_bytes += batch.len();
            compressed_bytes += compressor.append(&batch)?.len();
        }
        compressed_bytes += compressor.finish()?.len();

        Ok(CompressionMeasurement { scenario, uncompressed_bytes, compressed_bytes })
    }

    /// Measures the compressed bytes emitted for every transaction appended to one channel.
    ///
    /// This uses the production streaming interface rather than repeatedly recompressing prefixes.
    /// Consequently it is efficient and the sum of [`IncrementalCompressionMeasurement`] values is
    /// exactly the final channel length. For a counterfactual "finished channel with versus without
    /// this transaction" measurement, callers must separately compress those two complete inputs.
    pub fn measure_incremental(
        self,
        scenario: CompressionScenario,
    ) -> Result<Vec<IncrementalCompressionMeasurement>, CompressionError> {
        assert!(scenario.transactions_per_batch > 0, "transactions per batch must be nonzero");
        assert!(scenario.batches_per_channel > 0, "batches per channel must be nonzero");

        let transaction_count = scenario.transaction_count();
        let mut compressor = CompressionStream::new(BrotliLevel::DEFAULT);
        let mut measurements = Vec::with_capacity(transaction_count);

        for transaction_index in 0..transaction_count {
            let transaction =
                self.synthetic_transaction(scenario.profile, scenario.pattern, transaction_index);
            let compressed_bytes = compressor.append(&transaction)?.len();
            measurements.push(IncrementalCompressionMeasurement {
                transaction_index,
                uncompressed_bytes: transaction.len(),
                compressed_bytes,
                includes_final_trailer: false,
            });
        }

        let trailer_bytes = compressor.finish()?.len();
        let final_measurement =
            measurements.last_mut().expect("scenario has at least one transaction");
        final_measurement.compressed_bytes += trailer_bytes;
        final_measurement.includes_final_trailer = true;

        Ok(measurements)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE: TransactionProfile = TransactionProfile { name: "test", encoded_bytes: 256 };

    #[test]
    fn measure_counts_all_synthetic_transaction_bytes() {
        let scenario = CompressionScenario {
            profile: PROFILE,
            pattern: InputPattern::Pseudorandom,
            transactions_per_batch: 3,
            batches_per_channel: 2,
        };

        let measurement = CompressionBenchmark.measure(scenario).unwrap();

        assert_eq!(measurement.uncompressed_bytes, 2208);
        assert_eq!(measurement.scenario.transaction_count(), 6);
        assert_eq!(
            measurement.compressed_bytes_per_transaction() * 6.0,
            measurement.compressed_bytes as f64
        );
    }

    #[test]
    fn pseudorandom_input_is_less_compressible_than_incrementing_input() {
        let common = CompressionScenario {
            profile: PROFILE,
            pattern: InputPattern::Incrementing,
            transactions_per_batch: 32,
            batches_per_channel: 4,
        };

        let incrementing = CompressionBenchmark.measure(common).unwrap();
        let pseudorandom = CompressionBenchmark
            .measure(CompressionScenario { pattern: InputPattern::Pseudorandom, ..common })
            .unwrap();

        assert!(pseudorandom.compressed_bytes > incrementing.compressed_bytes);
    }

    #[test]
    fn incremental_measurements_sum_to_the_finished_channel_size() {
        let scenario = CompressionScenario {
            profile: PROFILE,
            pattern: InputPattern::Pseudorandom,
            transactions_per_batch: 3,
            batches_per_channel: 2,
        };

        let aggregate = CompressionBenchmark.measure(scenario).unwrap();
        let incremental = CompressionBenchmark.measure_incremental(scenario).unwrap();

        assert_eq!(incremental.len(), scenario.transaction_count());
        assert_eq!(
            incremental.iter().map(|measurement| measurement.compressed_bytes).sum::<usize>(),
            aggregate.compressed_bytes
        );
        assert!(incremental.last().unwrap().includes_final_trailer);
    }

    #[test]
    fn supplied_profiles_cover_the_model_transaction_types() {
        let profiles = DEFAULT_TRANSACTION_PROFILES;

        assert_eq!(profiles.len(), 8);
        assert!(profiles.iter().all(|profile| profile.encoded_bytes > 0));
        assert_eq!(
            profiles.map(|profile| profile.name),
            [
                "Native ETH transfer",
                "ERC-20 transfer (USDC)",
                "B20 transfer",
                "Uniswap V3 / aggregator swap",
                "Uniswap V2 swap",
                "x402 agentic payment",
                "ERC-4337 smart-wallet UserOp",
                "Contract deployment",
            ]
        );
    }

    #[test]
    fn persisted_incompressible_results_match_the_default_measurement() {
        const RESULTS: &str = include_str!("../benchmarks/incompressible-data.csv");
        const TRANSACTIONS_PER_BATCH: usize = 128;
        const BATCHES_PER_CHANNEL: usize = 8;

        let rows = RESULTS.lines().skip(1).filter(|line| !line.is_empty());
        for (profile, row) in DEFAULT_TRANSACTION_PROFILES.into_iter().zip(rows) {
            let mut fields = row.split(',');
            let name = fields.next().unwrap();
            let input_bytes: usize = fields.next().unwrap().parse().unwrap();
            let estimated_bytes: usize = fields.next().unwrap().parse().unwrap();
            assert!(fields.next().is_none(), "unexpected field in `{row}`");
            assert_eq!(name, profile.name);
            assert_eq!(input_bytes, profile.encoded_bytes);

            let measurement = CompressionBenchmark
                .measure(CompressionScenario {
                    profile,
                    pattern: InputPattern::Pseudorandom,
                    transactions_per_batch: TRANSACTIONS_PER_BATCH,
                    batches_per_channel: BATCHES_PER_CHANNEL,
                })
                .unwrap();
            assert_eq!(measurement.estimated_compressed_bytes_per_transaction(), estimated_bytes);
        }
        assert_eq!(
            RESULTS.lines().skip(1).filter(|line| !line.is_empty()).count(),
            DEFAULT_TRANSACTION_PROFILES.len()
        );
    }
}
