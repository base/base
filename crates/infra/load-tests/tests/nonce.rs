//! Integration tests for [`NonceManager`] with an Anvil backend.

use std::time::Duration;

use alloy_network::EthereumWallet;
use alloy_node_bindings::Anvil;
use alloy_primitives::{Address, U256};
use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use alloy_rpc_types::TransactionRequest;
use alloy_signer_local::PrivateKeySigner;
use base_load_tests::{NonceError, NonceGuard, NonceManager};

/// Helper: spawns an Anvil instance and returns a [`NonceManager`] wired to
/// the first default account.
fn setup() -> (NonceManager<RootProvider>, alloy_node_bindings::AnvilInstance) {
    let anvil = Anvil::new().spawn();
    let url = anvil.endpoint_url();
    let provider = RootProvider::new_http(url);
    let address = anvil.addresses()[0];
    let manager = NonceManager::new(provider, address, Duration::from_secs(10));
    (manager, anvil)
}

/// Reserves one nonce and releases its guard as a successful signer would.
async fn consume_nonce(manager: &NonceManager<RootProvider>) -> u64 {
    let guard = manager.next_nonce().await.expect("nonce should be available");
    let nonce = guard.nonce();
    drop(guard);
    nonce
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
async fn provider_failure_returns_fetch_error() {
    // Point the provider at a non-listening port so the RPC call fails.
    let url = "http://127.0.0.1:1".parse().expect("valid url");
    let provider = RootProvider::new_http(url);
    let address = Address::ZERO;
    let manager = NonceManager::new(provider, address, Duration::from_secs(10));

    let err = manager.next_nonce().await.expect_err("should fail on unreachable provider");
    assert_eq!(err, NonceError::FetchFailed);
}

#[tokio::test]
async fn rpc_timeout_returns_timeout_error() {
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
    assert_eq!(err, NonceError::FetchTimeout);
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
    // `NonceGuard` must be `Send` so it can cross task boundaries after
    // reserving a nonce.
    /// Asserts that `T` implements [`Send`].
    fn assert_send<T: Send>() {}
    assert_send::<NonceGuard>();
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

// ── returned nonce (gap recovery) tests ───────────────────────────

#[tokio::test]
async fn return_reserved_nonce_enables_reuse() {
    let (manager, _anvil) = setup();

    // Consume nonces 0, 1, 2.
    let n0 = consume_nonce(&manager).await;
    let n1 = consume_nonce(&manager).await;
    let n2 = consume_nonce(&manager).await;
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

    let n = consume_nonce(&manager).await;
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

    // Consume 0 and 1, then return 0.
    let _ = consume_nonce(&manager).await;
    let _ = consume_nonce(&manager).await;
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

    // Consume 0, 1, 2, 3.
    for _ in 0..4 {
        consume_nonce(&manager).await;
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

    // Consume nonces 3, 4 and return them (simulating failed sends).
    let _ = consume_nonce(&manager).await; // 3
    let _ = consume_nonce(&manager).await; // 4
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
    let guard = manager.next_nonce().await.unwrap();
    assert_eq!(guard.nonce(), 5, "next fresh nonce should be 5");
}
