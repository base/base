//! Immutable d44 and issue-76 pairwise parity gates.
use std::{sync::Arc, time::Instant};

use alloy_primitives::{Address, U256};
use base_mev_trader::{
    CancellationProbe, CancellationToken, D44CandidateEncoder, ExactProtocol, GlobalLifecycle,
    ISSUE76_ENGINE_QUOTE, ISSUE76_OBSERVED_QUOTE, ISSUE76_PROVENANCE_BLOB, ISSUE76_QUOTE_BLOB,
    ISSUE76_QUOTE_GAP, PAIRWISE_SOURCE_COMMIT, PAIRWISE_SOURCE_TREE, PairwiseEngine,
    PreparedPoolQuote, PreparedPoolState, WETH,
};

const TOKEN: Address =
    Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xaa]);

fn probe() -> CancellationProbe {
    CancellationProbe::new(
        Arc::new(CancellationToken::with_approved_deadline(Instant::now())),
        Arc::new(GlobalLifecycle::default()),
    )
}

fn pool(last: u8) -> PreparedPoolState {
    let reserve = U256::from_str_radix("1000000000000000000000000", 10).expect("reserve");
    PreparedPoolState {
        pool: Address::with_last_byte(last),
        protocol: ExactProtocol::UniswapV2,
        token0: WETH,
        token1: TOKEN,
        decimals0: 18,
        decimals1: 18,
        fee_pips: 3_000,
        quote: PreparedPoolQuote::ConstantProduct { reserve0: reserve, reserve1: reserve },
    }
}

#[test]
fn immutable_d44_source_and_corrected_issue76_pins_are_exact() {
    assert_eq!(PAIRWISE_SOURCE_COMMIT, "d44b316266c4231e6b82f88b460efdb00d70428a");
    assert_eq!(PAIRWISE_SOURCE_TREE, "bd5b337329d98965abd25af0a823f19eb12c1baa");
    assert_eq!(ISSUE76_QUOTE_BLOB, "d55983dbc8d075c6ba8012d5e0b40501122147ee");
    assert_eq!(ISSUE76_PROVENANCE_BLOB, "a6016953641a5c1bba3fefb172cc155439cd0442");
    assert_eq!(ISSUE76_ENGINE_QUOTE - ISSUE76_OBSERVED_QUOTE, ISSUE76_QUOTE_GAP);
}

#[test]
fn cancel_false_discovery_matches_frozen_d44_candidate_bytes() {
    let pools = [pool(1), pool(2)];
    let candidates =
        PairwiseEngine::discover("ties", &pools, &[pools[1].pool, pools[0].pool], &probe())
            .expect("d44 discovery");
    assert_eq!(candidates.len(), 2);
    let bytes = D44CandidateEncoder::encode(&candidates).expect("canonical candidates");
    let text = String::from_utf8(bytes).expect("canonical UTF-8");
    assert!(text.starts_with("[{\"amountIn\":\"1000000000000\",\"amountOut\":\"994008999998\""));
    assert!(text.contains("\"grossProfit\":\"-5991000002\""));
    assert!(text.ends_with("]\n"));
    assert!(
        text.find("0000000000000000000000000000000000000001").expect("pool1")
            < text.find("0000000000000000000000000000000000000002").expect("pool2")
    );
}
