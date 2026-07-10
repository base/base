//! Optional block-arrival latency recorder for the CL gossip layer.
//!
//! This is a lightweight, flag-gated instrument used to measure how long canonical (unsafe) blocks
//! take to reach a node over the consensus-layer gossip network, sliced by the observer node's
//! geographic region. It is intended for a one-off measurement to evaluate whether the current P2P
//! architecture is viable for 200ms blocks; it is **not** wired into the normal metrics pipeline.
//!
//! When enabled (via `--p2p.latency.log` / `--p2p.latency.region`), the [`GossipDriver`] records one
//! CSV row per *first-seen* block at the point it is accepted off gossip. Rows are handed to a
//! dedicated writer thread over a bounded channel so the libp2p swarm loop is never blocked; if the
//! channel is full the row is dropped and counted rather than applying backpressure.
//!
//! [`GossipDriver`]: crate::GossipDriver

use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread,
};

use alloy_primitives::{B256, Bytes};
use base_common_rpc_types_engine::NetworkPayloadEnvelope;
use libp2p::PeerId;

/// The `setTimestampMillisPart(uint16)` selector for the `BaseTime` metadata deposit.
///
/// Mirrors `base_consensus_protocol::BaseTimeUpdateTx::SELECTOR`. Inlined here to avoid a
/// dependency on the protocol crate for this throwaway tool.
const BASE_TIME_SELECTOR: [u8; 4] = [0x86, 0xbd, 0xf3, 0x94];

/// The ABI calldata length of the `BaseTime` metadata deposit (`selector ++ 30 zero bytes ++ u16`).
const BASE_TIME_CALLDATA_LEN: usize = 4 + 32;

/// Bounded capacity of the record channel. Large enough that a brief writer stall never drops rows
/// under normal block rates; overflow is counted, not blocking.
const CHANNEL_CAPACITY: usize = 8192;

/// The CSV header written to a freshly created log file.
const CSV_HEADER: &str =
    "recv_wallclock_ns,block_number,block_hash,produced_sec,produced_millis_part,region,peer_id";

/// A single block-arrival observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatencyRecord {
    /// Wall-clock time (nanoseconds since the Unix epoch) at which the block was received off
    /// gossip. Must be NTP-synced to be comparable across observer nodes.
    pub recv_wallclock_ns: u128,
    /// The block number.
    pub block_number: u64,
    /// The block hash (join key when aggregating across observers).
    pub block_hash: B256,
    /// The block header timestamp in whole seconds (the produced time).
    pub produced_sec: u64,
    /// The sub-second millisecond component decoded from the `BaseTime` deposit, or `0` if absent
    /// (e.g. pre-Holocene).
    pub produced_millis_part: u16,
    /// The peer we received this block from (`propagation_source`).
    pub peer_id: String,
}

impl LatencyRecord {
    /// Formats the record as a single CSV line (without a trailing newline), stamping the observer's
    /// `region`.
    pub fn to_csv_line(&self, region: &str) -> String {
        format!(
            "{},{},{:#x},{},{},{},{}",
            self.recv_wallclock_ns,
            self.block_number,
            self.block_hash,
            self.produced_sec,
            self.produced_millis_part,
            region,
            self.peer_id,
        )
    }
}

/// Records block-arrival observations to a CSV file via a background writer thread.
///
/// Cloning a recorder yields another handle to the same underlying channel and writer.
#[derive(Debug, Clone)]
pub struct LatencyRecorder {
    /// Non-blocking sender to the writer thread.
    tx: SyncSender<LatencyRecord>,
    /// Count of records dropped because the channel was full or closed.
    dropped: Arc<AtomicU64>,
}

impl LatencyRecorder {
    /// Opens (or creates) the CSV file at `path` and spawns the writer thread, stamping every row
    /// with `region`.
    pub fn new(path: impl AsRef<Path>, region: String) -> std::io::Result<Self> {
        let path = path.as_ref();
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let needs_header = file.metadata().map(|m| m.len() == 0).unwrap_or(true);

        let (tx, rx) = mpsc::sync_channel::<LatencyRecord>(CHANNEL_CAPACITY);
        thread::Builder::new()
            .name("cl-latency-writer".to_string())
            .spawn(move || Self::run_writer(file, needs_header, region, rx))?;

        Ok(Self { tx, dropped: Arc::new(AtomicU64::new(0)) })
    }

    /// Records a first-seen block arrival. Never blocks: if the channel is full the row is dropped
    /// and the drop counter is incremented.
    pub fn record(&self, envelope: &NetworkPayloadEnvelope, recv_wallclock_ns: u128, peer: &PeerId) {
        let payload = &envelope.payload;
        let record = LatencyRecord {
            recv_wallclock_ns,
            block_number: payload.block_number(),
            block_hash: payload.block_hash(),
            produced_sec: payload.timestamp(),
            produced_millis_part: Self::base_time_millis(payload.transactions()),
            peer_id: peer.to_string(),
        };
        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) = self.tx.try_send(record) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Returns the number of records dropped so far due to a full or closed channel.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Best-effort extraction of the `BaseTime` sub-second millisecond component.
    ///
    /// Post-Holocene the `BaseTime` metadata deposit is `tx[1]`. Its calldata
    /// (`selector ++ 30 zero bytes ++ u16`) appears verbatim inside the 2718-encoded deposit
    /// transaction, so we scan for the selector rather than fully decoding the deposit. Pre-Holocene
    /// the deposit is absent and this returns `0`.
    fn base_time_millis(transactions: &[Bytes]) -> u16 {
        let Some(tx) = transactions.get(1) else { return 0 };
        tx.windows(BASE_TIME_SELECTOR.len())
            .position(|window| window == BASE_TIME_SELECTOR.as_slice())
            .and_then(|start| {
                let end = start + BASE_TIME_CALLDATA_LEN;
                (end <= tx.len()).then(|| u16::from_be_bytes([tx[start + 34], tx[start + 35]]))
            })
            .unwrap_or(0)
    }

    /// Writer thread entry point: drains records and appends them to the file, batching flushes so
    /// the file is flushed whenever the channel goes momentarily idle. Exits when all senders drop.
    fn run_writer(file: File, needs_header: bool, region: String, rx: Receiver<LatencyRecord>) {
        let mut writer = BufWriter::new(file);
        if needs_header && writeln!(writer, "{CSV_HEADER}").is_err() {
            warn!(target: "gossip", "failed to write latency log header; disabling recorder");
            return;
        }

        while let Ok(record) = rx.recv() {
            Self::write_row(&mut writer, &record, &region);
            while let Ok(record) = rx.try_recv() {
                Self::write_row(&mut writer, &record, &region);
            }
            let _ = writer.flush();
        }
        let _ = writer.flush();
    }

    /// Writes a single row, logging (but not propagating) I/O errors.
    fn write_row(writer: &mut BufWriter<File>, record: &LatencyRecord, region: &str) {
        if let Err(error) = writeln!(writer, "{}", record.to_csv_line(region)) {
            warn!(target: "gossip", %error, "failed to write latency log row");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{thread::sleep, time::Duration};

    use alloy_primitives::b256;

    use super::*;

    fn selector_tx(millis: u16) -> Bytes {
        // selector ++ 30 zero bytes ++ 2-byte big-endian millis part
        let mut data = Vec::with_capacity(BASE_TIME_CALLDATA_LEN);
        data.extend_from_slice(&BASE_TIME_SELECTOR);
        data.extend_from_slice(&[0u8; 30]);
        data.extend_from_slice(&millis.to_be_bytes());
        Bytes::from(data)
    }

    #[test]
    fn base_time_millis_absent_without_second_tx() {
        assert_eq!(LatencyRecorder::base_time_millis(&[]), 0);
        assert_eq!(LatencyRecorder::base_time_millis(&[Bytes::from_static(b"l1info")]), 0);
    }

    #[test]
    fn base_time_millis_decodes_second_tx() {
        // tx[0] is the L1 info tx; tx[1] carries the BaseTime deposit calldata.
        let txs = vec![Bytes::from_static(b"l1info"), selector_tx(600)];
        assert_eq!(LatencyRecorder::base_time_millis(&txs), 600);
    }

    #[test]
    fn base_time_millis_ignores_missing_selector() {
        let txs = vec![Bytes::from_static(b"l1info"), Bytes::from_static(b"not-base-time")];
        assert_eq!(LatencyRecorder::base_time_millis(&txs), 0);
    }

    #[test]
    fn csv_line_is_stable() {
        let record = LatencyRecord {
            recv_wallclock_ns: 1_725_271_882_500_000_000,
            block_number: 42,
            block_hash: b256!("0x00000000000000000000000000000000000000000000000000000000000000ab"),
            produced_sec: 1_725_271_882,
            produced_millis_part: 200,
            peer_id: "16Uiu2HAm".to_string(),
        };
        assert_eq!(
            record.to_csv_line("ap-southeast"),
            "1725271882500000000,42,0x00000000000000000000000000000000000000000000000000000000000000ab,1725271882,200,ap-southeast,16Uiu2HAm"
        );
    }

    #[test]
    fn writer_persists_header_and_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("latency.csv");

        let recorder = LatencyRecorder::new(&path, "us-east".to_string()).unwrap();
        let record = LatencyRecord {
            recv_wallclock_ns: 123,
            block_number: 7,
            block_hash: B256::ZERO,
            produced_sec: 100,
            produced_millis_part: 0,
            peer_id: "peer".to_string(),
        };
        recorder.tx.try_send(record).unwrap();
        // Drop the recorder so the writer thread observes channel closure, flushes and exits.
        drop(recorder);

        // Poll for the file to be flushed by the background writer.
        let mut contents = String::new();
        for _ in 0..50 {
            contents = std::fs::read_to_string(&path).unwrap();
            if contents.lines().count() >= 2 {
                break;
            }
            sleep(Duration::from_millis(20));
        }

        let mut lines = contents.lines();
        assert_eq!(lines.next().unwrap(), CSV_HEADER);
        assert_eq!(
            lines.next().unwrap(),
            "123,7,0x0000000000000000000000000000000000000000000000000000000000000000,100,0,us-east,peer"
        );
    }
}
