//! Deterministic snapshot-to-measurement integration gates.

use std::{fmt::Debug, str::FromStr, sync::Arc, time::Instant};

use alloy_consensus::{Header, Sealed, Transaction, transaction::SignerRecoverable};
use alloy_eips::{Decodable2718, Typed2718};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_rpc_types_engine::PayloadId;
use base_common_consensus::BaseTxEnvelope;
use base_mev_trader::{
    BundleVisitor, CancellationProbe, CancellationToken, D44CandidateEncoder,
    ExactPrefixCoordinator, ExactPrefixOracle, ExactProtocol, FrameProcessor, GlobalLifecycle,
    IndependentOracle, OracleDigest, OracleOutcome, PairwiseEngine, PayloadVisitor,
    PendingSnapshotView, PortError, PredecessorOracle, PreparedPoolQuote, PreparedPoolState,
    SnapshotCaptureCoordinator, SnapshotHandle, SnapshotHandleFactory, TaskState,
    TraderSnapshotPort, TransactionVisitor, VictimFrame, VisitControl, VisitSummary, WETH,
};
use reth_provider::StateProviderBox;
use revm_bytecode::Bytecode;
use revm_database::BundleAccount;

const PREFIX_TX: &str = "f86c098504a817c800825208943535353535353535353535353535353535353535880de0b6b3a76400008025a028ef61340bd939bc2195fe537567866003e1a15d3c71ff63e1590620aa636276a067cbe9d8997f761aecb703304b3800ccf555c9f3dc64214b297fb1966a3b6d83";
const VICTIM_TX: &str = "02f86c0d010183072335825208940000000000000000000000000000000000000000872386f26fc1000080c001a0cdb9e4f2f1ba53f9429077e7055e078cf599786e29059cd80c5e0e923bb2c114a01c90e29201e031baf1da66296c3a5c15c200bcb5e6c34da2f05f7d1778f8be07";

fn decode(raw: &str) -> (Bytes, BaseTxEnvelope) {
    let bytes = Bytes::from_str(&format!("0x{raw}")).expect("fixture bytes");
    let transaction =
        BaseTxEnvelope::decode_2718_exact(bytes.as_ref()).expect("fixture transaction");
    (bytes, transaction)
}

#[derive(Debug)]
struct FixtureView {
    payload_id: PayloadId,
    prefix: BaseTxEnvelope,
}

impl PendingSnapshotView for FixtureView {
    fn parent_hash(&self) -> B256 {
        B256::with_last_byte(1)
    }

    fn latest_block_number(&self) -> u64 {
        100
    }

    fn canonical_block_number(&self) -> u64 {
        99
    }

    fn latest_flashblock_index(&self) -> u64 {
        1
    }

    fn latest_header(&self) -> Sealed<Header> {
        Sealed::new_unchecked(
            Header {
                parent_hash: self.parent_hash(),
                number: self.latest_block_number(),
                ..Default::default()
            },
            B256::with_last_byte(2),
        )
    }

    fn pending_account_nonce(
        &self,
        _address: Address,
    ) -> Result<Option<base_mev_trader::PendingAccountNonce>, PortError> {
        Ok(None)
    }

    fn latest_block_transaction_count(&self) -> usize {
        1
    }

    fn has_transaction_hash(&self, _transaction_hash: B256) -> bool {
        false
    }

    fn transaction_position(&self, _block_number: u64, _transaction_hash: B256) -> Option<usize> {
        None
    }

    fn visit_latest_block_payloads(
        &self,
        visitor: &mut dyn PayloadVisitor,
    ) -> Result<VisitSummary, PortError> {
        let control = visitor.visit(self.payload_id, self.latest_flashblock_index())?;
        Ok(VisitSummary { visited: 1, complete: control == VisitControl::Continue })
    }

    fn visit_transactions_for_block(
        &self,
        block_number: u64,
        start: usize,
        limit: usize,
        visitor: &mut dyn TransactionVisitor,
    ) -> Result<VisitSummary, PortError> {
        if block_number != self.latest_block_number() || start != 0 || limit != 1 {
            return Ok(VisitSummary { visited: 0, complete: false });
        }
        let control = visitor.visit(0, &self.prefix)?;
        Ok(VisitSummary { visited: 1, complete: control == VisitControl::Continue })
    }

    fn visit_bundle(&self, _visitor: &mut dyn BundleVisitor) -> Result<VisitSummary, PortError> {
        Ok(VisitSummary { visited: 0, complete: true })
    }
}

#[derive(Debug)]
struct FixturePort {
    view: Arc<dyn PendingSnapshotView + Send + Sync>,
    received_at: Instant,
}

impl TraderSnapshotPort for FixturePort {
    fn capture_latest(
        &self,
        factory: &SnapshotHandleFactory,
    ) -> Result<Option<SnapshotHandle>, PortError> {
        factory.issue(Arc::clone(&self.view), self.received_at).map(Some)
    }

    fn is_current_authoritative(&self, handle: &SnapshotHandle) -> bool {
        handle.matches_capture(&self.view, self.received_at)
    }

    fn state_at_hash(&self, _block_hash: B256) -> Result<StateProviderBox, PortError> {
        Err(PortError::ProviderUnavailable)
    }

    fn sealed_header_at_hash(&self, _block_hash: B256) -> Result<Sealed<Header>, PortError> {
        Ok(self.view.latest_header())
    }
}

#[derive(Debug)]
struct MatchingPredecessor {
    digest: OracleDigest,
}

impl PredecessorOracle for MatchingPredecessor {
    fn freeze(
        &mut self,
        _snapshot: &SnapshotHandle,
        _victim: &VictimFrame,
    ) -> Result<Option<OracleDigest>, PortError> {
        Ok(Some(self.digest))
    }

    fn exact_target(
        &mut self,
        _snapshot: &SnapshotHandle,
        _victim: &VictimFrame,
        frozen_transaction_count: usize,
    ) -> Result<Option<usize>, PortError> {
        Ok(Some(frozen_transaction_count))
    }
}

#[derive(Debug)]
struct MatchingIndependent {
    digest: OracleDigest,
}

impl IndependentOracle for MatchingIndependent {
    fn victim_only(
        &mut self,
        _snapshot: &SnapshotHandle,
        _victim: &VictimFrame,
    ) -> Result<Option<OracleDigest>, PortError> {
        Ok(Some(self.digest))
    }
}

#[derive(Debug)]
struct MatchingPrefix {
    digest: OracleDigest,
    target: usize,
    visited: usize,
}

impl TransactionVisitor for MatchingPrefix {
    fn visit(
        &mut self,
        _position: usize,
        _transaction: &BaseTxEnvelope,
    ) -> Result<VisitControl, PortError> {
        self.visited += 1;
        Ok(VisitControl::Continue)
    }
}

impl ExactPrefixOracle for MatchingPrefix {
    fn begin(
        &mut self,
        _snapshot: &SnapshotHandle,
        _victim: &VictimFrame,
        target: usize,
    ) -> Result<(), PortError> {
        self.target = target;
        self.visited = 0;
        Ok(())
    }

    fn finish(
        &mut self,
        _snapshot: &SnapshotHandle,
        _victim: &VictimFrame,
    ) -> Result<Option<OracleDigest>, PortError> {
        Ok((self.visited == self.target).then_some(self.digest))
    }
}

fn live_probe() -> CancellationProbe {
    CancellationProbe::new(
        Arc::new(CancellationToken::with_approved_deadline(Instant::now())),
        Arc::new(GlobalLifecycle::default()),
    )
}

fn pool(
    identity: u8,
    token: Address,
    weth_reserve: &str,
    token_reserve: &str,
) -> PreparedPoolState {
    PreparedPoolState {
        pool: Address::with_last_byte(identity),
        protocol: ExactProtocol::UniswapV2,
        token0: WETH,
        token1: token,
        decimals0: 18,
        decimals1: 18,
        fee_pips: 3_000,
        quote: PreparedPoolQuote::ConstantProduct {
            reserve0: U256::from_str_radix(weth_reserve, 10).expect("WETH reserve"),
            reserve1: U256::from_str_radix(token_reserve, 10).expect("token reserve"),
        },
    }
}

fn fixture_pipeline() -> Vec<u8> {
    let now = Instant::now();
    let (_, prefix) = decode(PREFIX_TX);
    let payload_id = PayloadId::new([3; 8]);
    let view: Arc<dyn PendingSnapshotView + Send + Sync> =
        Arc::new(FixtureView { payload_id, prefix });
    let port = FixturePort { view, received_at: now };
    let snapshot =
        SnapshotCaptureCoordinator.capture(&port).expect("capture").expect("live snapshot");

    let (raw_tx, transaction) = decode(VICTIM_TX);
    let victim = VictimFrame {
        chain_id: transaction.chain_id().expect("protected chain"),
        transaction_type: transaction.ty(),
        transaction_hash: B256::from(*transaction.tx_hash()),
        from: transaction.recover_signer().expect("fixture sender"),
        raw_tx,
        parent_hash: snapshot.parent_hash(),
        block_number: snapshot.latest_block_number(),
        victim_flashblock_index: 2,
        received_at: now,
    };
    FrameProcessor::decode(&snapshot, &victim, now).expect("coherent protected frame");

    let digest = OracleDigest(B256::with_last_byte(9));
    let mut predecessor = MatchingPredecessor { digest };
    let mut independent = MatchingIndependent { digest };
    let mut exact_prefix = MatchingPrefix { digest, target: 0, visited: 0 };
    let oracle = ExactPrefixCoordinator
        .evaluate(&snapshot, &victim, &mut predecessor, &mut independent, &mut exact_prefix)
        .expect("six-step oracle");
    assert_eq!(oracle.outcome, OracleOutcome::Match);

    let token = Address::with_last_byte(0xaa);
    let pools = [
        pool(1, token, "1000000000000000000000000", "2000000000000000000000000"),
        pool(2, token, "1000000000000000000000000", "1000000000000000000000000"),
    ];
    let cancellation = live_probe();
    let candidates = PairwiseEngine::discover("e2e", &pools, &[pools[0].pool], &cancellation)
        .expect("pairwise discovery");
    let bytes = D44CandidateEncoder::encode(&candidates).expect("canonical d44 candidate bytes");
    assert!(!bytes.is_empty());
    bytes
}

#[test]
fn repeated_snapshot_frame_oracle_pairwise_candidate_is_byte_identical() {
    let first = fixture_pipeline();
    let second = fixture_pipeline();
    assert_eq!(first, second);
}

#[test]
fn cancelled_pairwise_discovery_has_zero_output() {
    let now = Instant::now();
    let token = Arc::new(CancellationToken::new(now));
    let cancellation =
        CancellationProbe::new(Arc::clone(&token), Arc::new(GlobalLifecycle::default()));
    let token_address = Address::with_last_byte(0xaa);
    let pools = [
        pool(1, token_address, "1000000000000000000000000", "2000000000000000000000000"),
        pool(2, token_address, "1000000000000000000000000", "1000000000000000000000000"),
    ];
    let result = PairwiseEngine::discover("cancelled-e2e", &pools, &[pools[0].pool], &cancellation);
    let candidate_count = result.as_ref().map_or(0, Vec::len);

    assert!(result.is_err());
    assert_eq!(candidate_count, 0);
    assert_eq!(token.state(), TaskState::DroppedAcked);
}

#[test]
fn fixture_traits_remain_borrowed_and_provider_free() {
    fn trait_shapes(
        _debug: &dyn Debug,
        _bundle: &dyn BundleVisitor,
        _bytecode: &Bytecode,
        _account: &BundleAccount,
    ) {
    }
    let _ = trait_shapes;
}
