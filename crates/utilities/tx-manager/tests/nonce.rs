//! Integration tests for [`NonceManager`] with an Anvil backend.

use std::time::Duration;

use alloy_network::EthereumWallet;
use alloy_node_bindings::Anvil;
use alloy_primitives::{Address, U256};
use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_signer_local::PrivateKeySigner;
use base_tx_manager::{NonceGuard, NonceManager, TxManagerError};
use rayon::prelude::*;

/// Helper: spawns an Anvil instance and returns a [`NonceManager`] wired to
/// the first default account.
fn setup() -> (NonceManager, alloy_node_bindings::AnvilInstance) {
    let anvil = Anvil::new().spawn();
    let url = anvil.endpoint_url();
    let provider = RootProvider::new_http(url);
    let address = anvil.addresses()[0];
    let manager = NonceManager::new(provider, address, Duration::from_secs(10));
    (manager, anvil)
}

#[tokio::test]
async fn first_call_fetches_nonce_from_provider() {
    let (manager, _anvil) = setup();

    let guard = manager.next_nonce().await.expect("should fetch nonce");
    // Fresh Anvil account has zero transactions.
    assert_eq!(guard.nonce(), 0);
}

#[tokio::test]
async fn subsequent_calls_increment_locally() {
    let (manager, _anvil) = setup();

    let g0 = manager.next_nonce().await.expect("first nonce");
    assert_eq!(g0.nonce(), 0);
    drop(g0);

    let g1 = manager.next_nonce().await.expect("second nonce");
    assert_eq!(g1.nonce(), 1);
    drop(g1);

    let g2 = manager.next_nonce().await.expect("third nonce");
    assert_eq!(g2.nonce(), 2);
}

#[tokio::test]
async fn rollback_restores_nonce() {
    let (manager, _anvil) = setup();

    // Reserve nonces 0 and 1, drop them to advance the cache.
    let g0 = manager.next_nonce().await.unwrap();
    assert_eq!(g0.nonce(), 0);
    drop(g0);

    let g1 = manager.next_nonce().await.unwrap();
    assert_eq!(g1.nonce(), 1);
    drop(g1);

    // Reserve nonce 2, then roll it back.
    let g2 = manager.next_nonce().await.unwrap();
    assert_eq!(g2.nonce(), 2);
    g2.rollback();

    // Next call should reuse nonce 2.
    let g2_again = manager.next_nonce().await.unwrap();
    assert_eq!(g2_again.nonce(), 2);
}

#[tokio::test]
async fn reset_forces_fresh_chain_fetch() {
    let (manager, _anvil) = setup();

    // Advance the local cache to nonce 2.
    let g0 = manager.next_nonce().await.unwrap();
    drop(g0);
    let g1 = manager.next_nonce().await.unwrap();
    drop(g1);

    // Reset clears the cache.
    manager.reset().await;

    // Next call fetches from chain — still 0 since no tx was sent.
    let guard = manager.next_nonce().await.unwrap();
    assert_eq!(guard.nonce(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_calls_get_unique_sequential_nonces() {
    let (manager, _anvil) = setup();

    let mut handles = Vec::new();
    for _ in 0..10 {
        let mgr = manager.clone();
        handles.push(tokio::spawn(async move {
            let guard = mgr.next_nonce().await.unwrap();
            let n = guard.nonce();
            drop(guard);
            n
        }));
    }

    let mut nonces = Vec::new();
    for h in handles {
        nonces.push(h.await.unwrap());
    }

    nonces.sort();
    let expected: Vec<u64> = (0..10).collect();
    assert_eq!(nonces, expected, "all nonces should be unique and sequential");
}

#[tokio::test]
async fn provider_failure_returns_rpc_error() {
    // Point the provider at a non-listening port so the RPC call fails.
    let url = "http://127.0.0.1:1".parse().expect("valid url");
    let provider = RootProvider::new_http(url);
    let address = Address::ZERO;
    let manager = NonceManager::new(provider, address, Duration::from_secs(10));

    let err = manager.next_nonce().await.expect_err("should fail on unreachable provider");
    assert!(matches!(err, TxManagerError::Rpc(_)), "expected TxManagerError::Rpc, got {err:?}");
}

#[tokio::test]
async fn rpc_timeout_returns_rpc_error() {
    // Start a TCP listener that accepts connections but never responds,
    // simulating a hung RPC endpoint.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        // Accept one connection and hold it open indefinitely.
        let (_socket, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
    });

    let url = format!("http://{addr}").parse().expect("valid url");
    let provider = RootProvider::new_http(url);
    let address = Address::ZERO;
    let manager = NonceManager::new(provider, address, Duration::from_millis(1));

    let err = manager.next_nonce().await.expect_err("should time out");
    match &err {
        TxManagerError::Rpc(msg) => {
            assert!(msg.contains("timed out"), "expected 'timed out' in message, got: {msg}",);
        }
        other => panic!("expected TxManagerError::Rpc, got {other:?}"),
    }
}

#[tokio::test]
async fn drop_without_rollback_advances_nonce() {
    let (manager, _anvil) = setup();

    // Reserve nonce 0 and drop without rollback — cache should stay at 1.
    let g0 = manager.next_nonce().await.unwrap();
    assert_eq!(g0.nonce(), 0);
    drop(g0);

    // The nonce advanced to 1, confirming drop (not rollback) is the
    // success path.
    let g1 = manager.next_nonce().await.unwrap();
    assert_eq!(g1.nonce(), 1);
}

#[tokio::test]
async fn reset_then_rollback_interaction() {
    let (manager, _anvil) = setup();

    // Advance to nonce 2.
    let g0 = manager.next_nonce().await.unwrap();
    drop(g0);
    let g1 = manager.next_nonce().await.unwrap();
    drop(g1);

    // Reset forces a fresh fetch from chain (returns 0 since no txs sent).
    manager.reset().await;
    let g_fresh = manager.next_nonce().await.unwrap();
    assert_eq!(g_fresh.nonce(), 0);

    // Roll back the freshly-fetched nonce — next call should reuse 0.
    g_fresh.rollback();
    let g_reused = manager.next_nonce().await.unwrap();
    assert_eq!(g_reused.nonce(), 0);
    drop(g_reused);

    // After consuming the reused nonce, the next one should be 1.
    let g_next = manager.next_nonce().await.unwrap();
    assert_eq!(g_next.nonce(), 1);
}

#[test]
fn nonce_guard_is_send() {
    // `NonceGuard` must be `Send` so it can be moved into a `tokio::spawn`
    // task after nonce reservation in `send_async()`.
    /// Asserts that `T` implements [`Send`].
    fn assert_send<T: Send>() {}
    assert_send::<NonceGuard>();
}

#[tokio::test(flavor = "multi_thread")]
async fn rayon_parallel_nonce_acquisition_produces_unique_nonces() {
    let (manager, _anvil) = setup();
    let handle = tokio::runtime::Handle::current();

    let nonces: Vec<u64> = (0..100)
        .into_par_iter()
        .map(|_| {
            let mgr = manager.clone();
            handle.block_on(async move {
                let guard = mgr.next_nonce().await.unwrap();
                let n = guard.nonce();
                drop(guard);
                n
            })
        })
        .collect();

    let mut sorted = nonces;
    sorted.sort();
    let expected: Vec<u64> = (0..100).collect();
    assert_eq!(sorted, expected, "all nonces must be unique and contiguous");
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_next_nonce_uniqueness_across_resets() {
    let (manager, _anvil) = setup();

    // Run several rounds of concurrent next_nonce() calls separated by
    // resets. Within each round all assigned nonces must be unique —
    // concurrent callers must never receive the same slot.
    for round in 0u32..5 {
        let batch_size = 20usize;
        let mut handles = Vec::with_capacity(batch_size);
        for _ in 0..batch_size {
            let mgr = manager.clone();
            handles.push(tokio::spawn(async move {
                let guard = mgr.next_nonce().await.unwrap();
                let n = guard.nonce();
                drop(guard);
                n
            }));
        }

        let mut nonces = Vec::with_capacity(batch_size);
        for h in handles {
            nonces.push(h.await.unwrap());
        }

        // Each round re-fetches from chain (0 — no txs sent) so the
        // sorted nonces must form the contiguous range 0..batch_size.
        nonces.sort();
        let expected: Vec<u64> = (0..batch_size as u64).collect();
        assert_eq!(nonces, expected, "round {round}: nonces should be contiguous 0..{batch_size}");

        // Reset clears the cache, forcing a fresh chain fetch next round.
        manager.reset().await;
    }
}

#[tokio::test]
async fn reserve_nonce_consumes_and_increments() {
    let (manager, _anvil) = setup();

    // reserve_nonce returns sequential values starting from 0.
    let n0 = manager.reserve_nonce().await.expect("first reserve");
    assert_eq!(n0, 0);

    let n1 = manager.reserve_nonce().await.expect("second reserve");
    assert_eq!(n1, 1);

    let n2 = manager.reserve_nonce().await.expect("third reserve");
    assert_eq!(n2, 2);

    // The lock is released immediately — next_nonce can acquire it
    // without blocking and picks up from where reserve_nonce left off.
    let guard = manager.next_nonce().await.expect("next_nonce after reserves");
    assert_eq!(guard.nonce(), 3);
}

// ── high-water mark tests ─────────────────────────────────────────

#[tokio::test]
async fn consume_reserved_sets_high_water_mark() {
    let (manager, _anvil) = setup();

    // Reserve nonce 0 — sets high-water mark to 1.
    let n = manager.reserve_nonce().await.expect("should reserve nonce 0");
    assert_eq!(n, 0);

    // Reset clears the cache; chain returns 0 since no tx was sent.
    // But the high-water mark ensures we skip past nonce 0.
    manager.reset().await;
    let guard = manager.next_nonce().await.expect("should get nonce after reset");
    assert_eq!(guard.nonce(), 1, "high-water mark should prevent reuse of reserved nonce 0");
}

#[tokio::test]
async fn high_water_mark_tracks_highest_reservation() {
    let (manager, _anvil) = setup();

    // Reserve nonces 0, 1, 2 — high-water mark advances to 3.
    for expected in 0..3u64 {
        let n = manager.reserve_nonce().await.expect("should reserve nonce");
        assert_eq!(n, expected);
    }

    // Reset and verify next nonce skips past all reserved nonces.
    manager.reset().await;
    let guard = manager.next_nonce().await.expect("should get nonce after reset");
    assert_eq!(guard.nonce(), 3, "next nonce after reset should skip past all reserved nonces");
}

#[tokio::test]
async fn high_water_mark_no_op_when_cache_ahead() {
    let (manager, _anvil) = setup();

    // Reserve nonce 0 — high-water mark is 1.
    let n = manager.reserve_nonce().await.expect("should reserve nonce 0");
    assert_eq!(n, 0);

    // Advance the cache past the high-water mark via next_nonce.
    let g1 = manager.next_nonce().await.expect("nonce 1");
    assert_eq!(g1.nonce(), 1);
    drop(g1);
    let g2 = manager.next_nonce().await.expect("nonce 2");
    assert_eq!(g2.nonce(), 2);
    drop(g2);

    // Cache is at 3, high-water mark is 1. Next nonce should be 3 (no interference).
    let g3 = manager.next_nonce().await.expect("nonce 3");
    assert_eq!(g3.nonce(), 3, "high-water mark should not interfere when cache is ahead");
}

#[tokio::test]
async fn reserve_nonce_protects_against_reset_collision() {
    let (manager, _anvil) = setup();

    // Reserve nonces 0 and 1 (simulating two concurrent send_async calls).
    let n0 = manager.reserve_nonce().await.expect("should reserve nonce 0");
    let n1 = manager.reserve_nonce().await.expect("should reserve nonce 1");
    assert_eq!(n0, 0);
    assert_eq!(n1, 1);

    // Simulate a concurrent send() failure triggering reset().
    manager.reset().await;

    // After reset, chain returns 0. But high-water mark (2) ensures
    // next_nonce() returns >= 2, avoiding collision with reserved 0 and 1.
    let guard = manager.next_nonce().await.expect("should get nonce after reset");
    assert_eq!(
        guard.nonce(),
        2,
        "nonce after reset must be exactly 2 to avoid collision with reserved nonces 0 and 1",
    );
}

// ── returned nonce (gap recovery) tests ───────────────────────────

#[tokio::test]
async fn return_reserved_nonce_enables_reuse() {
    let (manager, _anvil) = setup();

    // Reserve nonces 0, 1, 2.
    let n0 = manager.reserve_nonce().await.unwrap();
    let n1 = manager.reserve_nonce().await.unwrap();
    let n2 = manager.reserve_nonce().await.unwrap();
    assert_eq!((n0, n1, n2), (0, 1, 2));

    // Simulate failure of task with nonce 1.
    manager.return_reserved_nonce(1).await;

    // next_nonce should reissue 1 before allocating 3.
    let guard = manager.next_nonce().await.unwrap();
    assert_eq!(guard.nonce(), 1, "returned nonce should be reissued");
    drop(guard);

    // After reissue, next nonce should be 3 (fresh).
    let guard = manager.next_nonce().await.unwrap();
    assert_eq!(guard.nonce(), 3, "next nonce should be fresh after returned nonce consumed");
}

#[tokio::test]
async fn returned_nonces_survive_reset() {
    let (manager, _anvil) = setup();

    let n = manager.reserve_nonce().await.unwrap();
    assert_eq!(n, 0);

    // Return it then reset — returned nonce must persist.
    manager.return_reserved_nonce(0).await;
    manager.reset().await;

    let guard = manager.next_nonce().await.unwrap();
    assert_eq!(guard.nonce(), 0, "returned nonce should survive reset");
}

#[tokio::test]
async fn rollback_recycled_nonce_re_inserts() {
    let (manager, _anvil) = setup();

    // Reserve 0 and 1, return 0.
    let _ = manager.reserve_nonce().await.unwrap();
    let _ = manager.reserve_nonce().await.unwrap();
    manager.return_reserved_nonce(0).await;

    // Get recycled nonce 0.
    let guard = manager.next_nonce().await.unwrap();
    assert_eq!(guard.nonce(), 0);

    // Roll it back — should re-insert into returned_nonces.
    guard.rollback();

    // Next nonce should be 0 again.
    let guard = manager.next_nonce().await.unwrap();
    assert_eq!(guard.nonce(), 0, "rolled-back recycled nonce should be reissued");
}

#[tokio::test]
async fn multiple_returned_nonces_reissued_in_order() {
    let (manager, _anvil) = setup();

    // Reserve 0, 1, 2, 3.
    for _ in 0..4 {
        manager.reserve_nonce().await.unwrap();
    }

    // Return 3 and 1 (out of order).
    manager.return_reserved_nonce(3).await;
    manager.return_reserved_nonce(1).await;

    // Should reissue smallest first: 1, then 3.
    let g1 = manager.next_nonce().await.unwrap();
    assert_eq!(g1.nonce(), 1);
    drop(g1);

    let g3 = manager.next_nonce().await.unwrap();
    assert_eq!(g3.nonce(), 3);
    drop(g3);

    // Next fresh nonce should be 4.
    let g4 = manager.next_nonce().await.unwrap();
    assert_eq!(g4.nonce(), 4, "next fresh nonce after returned nonces should be 4");
}

#[tokio::test]
async fn reserve_nonce_reuses_returned_nonce() {
    let (manager, _anvil) = setup();

    // Reserve 0 and 1.
    let _ = manager.reserve_nonce().await.unwrap();
    let _ = manager.reserve_nonce().await.unwrap();

    // Return 0.
    manager.return_reserved_nonce(0).await;

    // reserve_nonce should reissue 0 (via next_nonce → advance_nonce).
    let n = manager.reserve_nonce().await.unwrap();
    assert_eq!(n, 0, "reserve_nonce should reuse returned nonce");

    // Next fresh should be 2.
    let n = manager.reserve_nonce().await.unwrap();
    assert_eq!(n, 2, "next reserve after reuse should be fresh nonce 2");
}

// ── returned nonce pruning tests ──────────────────────────────────

#[tokio::test]
async fn returned_nonces_below_chain_count_are_pruned_after_reset() {
    let anvil = Anvil::new().spawn();
    let url = anvil.endpoint_url();
    let address = anvil.addresses()[0];

    // Create a wallet-backed provider to send real transactions.
    let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
    let wallet = EthereumWallet::from(signer);
    let sender = ProviderBuilder::new().wallet(wallet).connect_http(url.clone());

    // Send two real transactions to advance the chain nonce to 2.
    for _ in 0..2 {
        let tx = TransactionRequest::default().to(address).value(U256::from(1));
        let _ = sender.send_transaction(tx).await.unwrap().get_receipt().await.unwrap();
    }

    // Verify chain nonce is now 2.
    let root = RootProvider::new_http(url);
    let chain_nonce = root.get_transaction_count(address).await.unwrap();
    assert_eq!(chain_nonce, 2);

    // Create a NonceManager and populate its cache.
    let manager = NonceManager::new(root, address, Duration::from_secs(10));
    let guard = manager.next_nonce().await.unwrap();
    assert_eq!(guard.nonce(), 2);
    drop(guard);

    // Reserve nonces 3, 4 and return them (simulating failed send_async).
    let _ = manager.reserve_nonce().await.unwrap(); // 3
    let _ = manager.reserve_nonce().await.unwrap(); // 4
    manager.return_reserved_nonce(3).await;
    manager.return_reserved_nonce(4).await;

    // Also return nonces 0 and 1 — these are below the chain nonce and
    // should be pruned after a reset + re-fetch.
    manager.return_reserved_nonce(0).await;
    manager.return_reserved_nonce(1).await;

    // Reset forces a chain fetch; the pruning logic should remove 0 and 1
    // (which are < chain_nonce 2) and keep 3 and 4 (which are >= 2).
    manager.reset().await;

    // The next nonce should be a returned nonce >= chain_nonce.
    // Since returned_nonces has {3, 4} after pruning, the smallest (3)
    // should be reissued first.
    let guard = manager.next_nonce().await.unwrap();
    assert_eq!(guard.nonce(), 3, "stale nonces 0 and 1 should have been pruned");
    drop(guard);

    let guard = manager.next_nonce().await.unwrap();
    assert_eq!(guard.nonce(), 4, "returned nonce 4 should still be available");
    drop(guard);

    // After both returned nonces are consumed, next should be fresh (5).
    // high-water mark from reserve_nonce(4) is 5, chain nonce is 2 → effective is 5.
    let guard = manager.next_nonce().await.unwrap();
    assert_eq!(guard.nonce(), 5, "next fresh nonce should be 5");
}
