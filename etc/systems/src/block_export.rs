//! Export of the blocks a snapshot sequencer run produced, for replay into a validator.
//!
//! A run writes every block after the snapshot boundary to `<dir>/blocks.jsonl`, one JSON line
//! per block in order, and then `<dir>/replay.json`. The manifest is written last, so its presence
//! marks a complete export. A replay restores the same snapshot and feeds the blocks back through
//! the engine API on a chain rebuilt from the manifest.

use std::{
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};

use alloy_consensus::{Block, BlockBody};
use alloy_primitives::B256;
use base_common_consensus::BaseTxEnvelope;
use base_common_network::Base;
use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
use eyre::{Result, WrapErr, ensure};
use serde::{Deserialize, Serialize};

/// File holding the exported blocks, one JSON line each.
pub const EXPORT_BLOCKS_FILE: &str = "blocks.jsonl";
/// File holding the [`ReplayManifest`]; written last.
pub const EXPORT_MANIFEST_FILE: &str = "replay.json";

/// What a replay needs besides the blocks: how to rebuild the run's chain and what to verify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayManifest {
    /// `base-bench snapshot --chain` value of the run.
    pub chain: String,
    /// `base-bench snapshot --rollup-config` value of the run, for custom chains.
    pub rollup_config: Option<PathBuf>,
    /// Local block interval of the run in milliseconds (2000 or 200).
    pub block_interval_ms: u64,
    /// Snapshot head the run continued from.
    pub boundary_number: u64,
    /// Hash of the snapshot head.
    pub boundary_hash: B256,
    /// Timestamp in seconds of the first produced block, where the run anchored its schedule and,
    /// at 200 ms, activated Denim.
    pub first_block_timestamp: u64,
    /// Number of the last exported block.
    pub last_number: u64,
    /// Hash of the last exported block, which the replay must reach.
    pub last_hash: B256,
    /// State root of the last exported block, which the replay must reproduce.
    pub last_state_root: B256,
    /// Exported block count: `last_number - boundary_number`.
    pub block_count: u64,
}

/// One produced block as the engine API payload that inserts it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportedBlock {
    /// Block number.
    pub number: u64,
    /// Block hash.
    pub hash: B256,
    /// Block timestamp in milliseconds, used to pace a replay.
    pub timestamp_ms: u64,
    /// Post-state root.
    pub state_root: B256,
    /// Execution payload version (1 to 4), which selects the `newPayload` method.
    pub payload_version: u8,
    /// Payload and parent beacon block root exactly as produced, including every system and
    /// deposit transaction.
    pub envelope: BaseExecutionPayloadEnvelope,
}

impl ExportedBlock {
    /// Converts a full RPC block, as returned by `eth_getBlockByNumber` with transactions, into
    /// its engine payload. Mirrors the follow client's conversion.
    pub fn from_rpc_block(block: <Base as alloy_network::Network>::BlockResponse) -> Self {
        let timestamp_ms = block
            .header
            .timestamp_ms
            .unwrap_or_else(|| block.header.timestamp.saturating_mul(1_000));
        let block = block.map_header(|header| header.into_inner());
        let hash = block.header.hash;
        let number = block.header.inner.number;
        let state_root = block.header.inner.state_root;
        let parent_beacon_block_root = block.header.inner.parent_beacon_block_root;
        let transactions: Vec<BaseTxEnvelope> =
            block.transactions.into_transactions().map(|tx| tx.inner.inner.into_inner()).collect();
        let consensus_block: Block<BaseTxEnvelope> = Block {
            header: block.header.inner,
            body: BlockBody { transactions, ommers: Vec::new(), withdrawals: block.withdrawals },
        };
        let (execution_payload, _sidecar) =
            BaseExecutionPayload::from_block_unchecked(hash, &consensus_block);
        let payload_version = match execution_payload {
            BaseExecutionPayload::V1(_) => 1,
            BaseExecutionPayload::V2(_) => 2,
            BaseExecutionPayload::V3(_) => 3,
            BaseExecutionPayload::V4(_) => 4,
        };
        Self {
            number,
            hash,
            timestamp_ms,
            state_root,
            payload_version,
            envelope: BaseExecutionPayloadEnvelope { parent_beacon_block_root, execution_payload },
        }
    }
}

/// Writes an export directory block by block.
#[derive(Debug)]
pub struct BlockExportWriter {
    dir: PathBuf,
    blocks: BufWriter<File>,
    last: Option<(u64, B256, B256)>,
    count: u64,
}

impl BlockExportWriter {
    /// Creates `dir` and starts a new blocks file, replacing a previous export there.
    pub fn create(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)
            .wrap_err_with(|| format!("failed to create export directory {}", dir.display()))?;
        let _ = fs::remove_file(dir.join(EXPORT_MANIFEST_FILE));
        let path = dir.join(EXPORT_BLOCKS_FILE);
        let file =
            File::create(&path).wrap_err_with(|| format!("failed to create {}", path.display()))?;
        Ok(Self { dir: dir.to_path_buf(), blocks: BufWriter::new(file), last: None, count: 0 })
    }

    /// Appends the next block; blocks must be consecutive.
    pub fn write(&mut self, block: &ExportedBlock) -> Result<()> {
        if let Some((number, _, _)) = self.last {
            ensure!(
                block.number == number + 1,
                "export blocks must be consecutive: {} after {number}",
                block.number
            );
        }
        serde_json::to_writer(&mut self.blocks, block)
            .wrap_err("failed to encode exported block")?;
        self.blocks.write_all(b"\n").wrap_err("failed to write exported block")?;
        self.last = Some((block.number, block.hash, block.state_root));
        self.count += 1;
        Ok(())
    }

    /// Flushes the blocks and writes the manifest, completing the export. The manifest's
    /// last-block fields and count are filled from the written blocks.
    pub fn finish(mut self, mut manifest: ReplayManifest) -> Result<ReplayManifest> {
        let (last_number, last_hash, last_state_root) =
            self.last.ok_or_else(|| eyre::eyre!("no blocks were exported"))?;
        ensure!(
            last_number == manifest.boundary_number + self.count,
            "exported blocks {}..={last_number} do not start right after boundary {}",
            last_number + 1 - self.count,
            manifest.boundary_number
        );
        self.blocks.flush().wrap_err("failed to flush exported blocks")?;
        self.blocks.get_ref().sync_all().wrap_err("failed to sync exported blocks")?;
        manifest.last_number = last_number;
        manifest.last_hash = last_hash;
        manifest.last_state_root = last_state_root;
        manifest.block_count = self.count;
        let temp = self.dir.join(format!("{EXPORT_MANIFEST_FILE}.tmp"));
        fs::write(&temp, serde_json::to_vec_pretty(&manifest)?)
            .wrap_err_with(|| format!("failed to write {}", temp.display()))?;
        fs::rename(&temp, self.dir.join(EXPORT_MANIFEST_FILE))
            .wrap_err("failed to publish export manifest")?;
        Ok(manifest)
    }
}

/// Reads the manifest of a complete export.
pub fn read_export_manifest(dir: &Path) -> Result<ReplayManifest> {
    let path = dir.join(EXPORT_MANIFEST_FILE);
    let bytes = fs::read(&path).wrap_err_with(|| {
        format!("missing or unreadable {}: incomplete export?", path.display())
    })?;
    serde_json::from_slice(&bytes).wrap_err_with(|| format!("invalid {}", path.display()))
}

/// Iterates the exported blocks in order.
pub fn read_exported_blocks(dir: &Path) -> Result<impl Iterator<Item = Result<ExportedBlock>>> {
    let path = dir.join(EXPORT_BLOCKS_FILE);
    let file = File::open(&path).wrap_err_with(|| format!("failed to open {}", path.display()))?;
    Ok(BufReader::new(file).lines().enumerate().map(move |(index, line)| {
        let line = line.wrap_err("failed to read exported block")?;
        serde_json::from_str(&line)
            .wrap_err_with(|| format!("invalid exported block on line {}", index + 1))
    }))
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{
        EMPTY_OMMER_ROOT_HASH, Header, SignableTransaction, TxEip1559,
        constants::EMPTY_WITHDRAWALS, proofs::calculate_transaction_root, transaction::Recovered,
    };
    use alloy_eips::eip7685::{EMPTY_REQUESTS_HASH, RequestsOrHash};
    use alloy_primitives::{Address, Bytes, Sealable, Signature, TxKind, U256};
    use alloy_rpc_types_engine::{CancunPayloadFields, PraguePayloadFields};
    use alloy_rpc_types_eth::BlockTransactions;
    use base_common_consensus::TxDeposit;
    use base_common_rpc_types::{BaseHeaderResponse, Transaction};
    use base_common_rpc_types_engine::BaseExecutionPayloadSidecar;

    use super::*;

    const TIMESTAMP: u64 = 1_700_000_000;

    fn rpc_transaction(tx: BaseTxEnvelope) -> Transaction {
        Transaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: Recovered::new_unchecked(tx, Address::repeat_byte(0x11)),
                block_hash: None,
                block_number: None,
                block_timestamp: None,
                transaction_index: None,
                effective_gas_price: None,
            },
            block_timestamp_ms: None,
            deposit_nonce: None,
            deposit_receipt_version: None,
        }
    }

    /// A full RPC block holding an L1-info system deposit and a user transaction. Isthmus blocks
    /// carry a requests hash and a message-passer withdrawals root and become V4 payloads.
    fn rpc_block(
        number: u64,
        parent_hash: B256,
        isthmus: bool,
    ) -> <Base as alloy_network::Network>::BlockResponse {
        let deposit = TxDeposit {
            source_hash: B256::repeat_byte(0x01),
            from: Address::repeat_byte(0xde),
            to: TxKind::Call(Address::repeat_byte(0x15)),
            mint: 0,
            value: U256::ZERO,
            gas_limit: 1_000_000,
            is_system_transaction: true,
            input: Bytes::from_static(&[0x09, 0x8e, 0x99, 0x7a, 0x01]),
        };
        let transfer = TxEip1559 {
            chain_id: 8453,
            nonce: number,
            gas_limit: 21_000,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1_000_000,
            to: TxKind::Call(Address::repeat_byte(0x22)),
            value: U256::from(7),
            ..Default::default()
        };
        let transactions = vec![
            BaseTxEnvelope::Deposit(deposit.seal_slow()),
            BaseTxEnvelope::Eip1559(transfer.into_signed(Signature::test_signature())),
        ];
        let header = Header {
            parent_hash,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: Address::repeat_byte(0x42),
            state_root: B256::with_last_byte(number as u8),
            transactions_root: calculate_transaction_root(&transactions),
            receipts_root: B256::repeat_byte(0x03),
            number,
            gas_limit: 1_000_000_000,
            gas_used: 70_000,
            timestamp: TIMESTAMP,
            // Holocene EIP-1559 parameters (denominator 250, elasticity 6).
            extra_data: Bytes::from_static(&[0, 0, 0, 0, 250, 0, 0, 0, 6]),
            mix_hash: B256::repeat_byte(0x04),
            base_fee_per_gas: Some(1_000_000),
            withdrawals_root: Some(if isthmus {
                B256::repeat_byte(0x05)
            } else {
                EMPTY_WITHDRAWALS
            }),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            parent_beacon_block_root: Some(B256::repeat_byte(0x06)),
            requests_hash: isthmus.then_some(EMPTY_REQUESTS_HASH),
            ..Default::default()
        };
        alloy_rpc_types_eth::Block {
            header: BaseHeaderResponse::with_timestamp_ms(
                alloy_rpc_types_eth::Header::new(header),
                Some(TIMESTAMP * 1_000 + 200),
            ),
            uncles: Vec::new(),
            transactions: BlockTransactions::Full(
                transactions.into_iter().map(rpc_transaction).collect(),
            ),
            withdrawals: Some(Default::default()),
        }
    }

    fn assert_round_trip(isthmus: bool, payload_version: u8) {
        let block = rpc_block(101, B256::repeat_byte(0x07), isthmus);
        let hash = block.header.hash;
        let exported = ExportedBlock::from_rpc_block(block);
        assert_eq!(exported.payload_version, payload_version);
        assert_eq!(exported.hash, hash);
        assert_eq!(exported.number, 101);
        assert_eq!(exported.timestamp_ms, TIMESTAMP * 1_000 + 200);
        assert_eq!(exported.state_root, B256::with_last_byte(101));

        let decoded: ExportedBlock =
            serde_json::from_str(&serde_json::to_string(&exported).unwrap()).unwrap();
        assert_eq!(decoded, exported);

        // Rebuild the block the way a node does from newPayload parameters.
        let ecotone = CancunPayloadFields {
            parent_beacon_block_root: decoded.envelope.parent_beacon_block_root.unwrap(),
            versioned_hashes: Vec::new(),
        };
        let sidecar = if isthmus {
            BaseExecutionPayloadSidecar::v4(
                ecotone,
                PraguePayloadFields::new(RequestsOrHash::Hash(EMPTY_REQUESTS_HASH)),
            )
        } else {
            BaseExecutionPayloadSidecar::v3(ecotone)
        };
        let rebuilt: Block<BaseTxEnvelope> =
            decoded.envelope.execution_payload.try_into_block_with_sidecar(&sidecar).unwrap();
        assert_eq!(rebuilt.header.hash_slow(), hash);
        assert!(matches!(rebuilt.body.transactions[0], BaseTxEnvelope::Deposit(_)));
    }

    #[test]
    fn exported_v3_block_round_trips_to_the_same_hash() {
        assert_round_trip(false, 3);
    }

    #[test]
    fn exported_v4_block_round_trips_to_the_same_hash() {
        assert_round_trip(true, 4);
    }

    /// `count` linked blocks after `boundary_number`.
    fn exported_chain(boundary_number: u64, boundary_hash: B256, count: u64) -> Vec<ExportedBlock> {
        let mut parent_hash = boundary_hash;
        (boundary_number + 1..=boundary_number + count)
            .map(|number| {
                let block = ExportedBlock::from_rpc_block(rpc_block(number, parent_hash, true));
                parent_hash = block.hash;
                block
            })
            .collect()
    }

    fn manifest(boundary_number: u64, boundary_hash: B256) -> ReplayManifest {
        ReplayManifest {
            chain: "mainnet".to_string(),
            rollup_config: None,
            block_interval_ms: 200,
            boundary_number,
            boundary_hash,
            first_block_timestamp: TIMESTAMP,
            last_number: 0,
            last_hash: B256::ZERO,
            last_state_root: B256::ZERO,
            block_count: 0,
        }
    }

    #[test]
    fn writer_output_reads_back_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let boundary_hash = B256::repeat_byte(0x08);
        let blocks = exported_chain(100, boundary_hash, 3);
        let mut writer = BlockExportWriter::create(dir.path()).unwrap();
        for block in &blocks {
            writer.write(block).unwrap();
        }
        assert!(read_export_manifest(dir.path()).is_err(), "manifest must be written last");

        let written = writer.finish(manifest(100, boundary_hash)).unwrap();
        assert_eq!(written.last_number, 103);
        assert_eq!(written.last_hash, blocks[2].hash);
        assert_eq!(written.last_state_root, blocks[2].state_root);
        assert_eq!(written.block_count, 3);
        assert_eq!(read_export_manifest(dir.path()).unwrap(), written);
        let read: Vec<ExportedBlock> =
            read_exported_blocks(dir.path()).unwrap().collect::<Result<_>>().unwrap();
        assert_eq!(read, blocks);
    }

    #[test]
    fn writer_rejects_gaps_and_a_detached_start() {
        let boundary_hash = B256::repeat_byte(0x08);
        let blocks = exported_chain(100, boundary_hash, 3);

        let dir = tempfile::tempdir().unwrap();
        let mut writer = BlockExportWriter::create(dir.path()).unwrap();
        writer.write(&blocks[0]).unwrap();
        assert!(writer.write(&blocks[2]).is_err());

        let dir = tempfile::tempdir().unwrap();
        let mut writer = BlockExportWriter::create(dir.path()).unwrap();
        for block in &blocks {
            writer.write(block).unwrap();
        }
        assert!(writer.finish(manifest(99, boundary_hash)).is_err());
        assert!(read_export_manifest(dir.path()).is_err());
    }
}
