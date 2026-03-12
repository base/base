//! Integration tests for [`NonceManager`] with an Anvil backend.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use alloy_node_bindings::Anvil;
use alloy_primitives::Address;
use alloy_provider::RootProvider;
use base_tx_manager::{NonceGuard, NonceManager, TxManagerError};

/// Helper: spawns an Anvil instance and returns a [`NonceManager`] wired to
/// the first default account.
fn setup() -> (NonceManager, alloy_node_bindings::AnvilInstance) {
    let anvil = Anvil::new().spawn();
    let url = anvil.endpoint_url();
    let provider = RootProvider::new_http(url);
    let address = anvil.addresses()[0];
    let manager = NonceManager::new(provider, address);
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
    let manager = NonceManager::new(provider, address);

    let err = manager.next_nonce().await.expect_err("should fail on unreachable provider");
    assert!(matches!(err, TxManagerError::Rpc(_)), "expected TxManagerError::Rpc, got {err:?}");
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

#[tokio::test]
async fn nonce_guard_is_send() {
    // `NonceGuard` must be `Send` so it can be moved into a `tokio::spawn`
    // task after nonce reservation in `send_async()`.
    /// Asserts that `T` implements [`Send`].
    fn assert_send<T: Send>() {}
    assert_send::<NonceGuard>();
}

#[tokio::test(flavor = "multi_thread")]
async fn reset_blocks_while_guard_held() {
    let (manager, _anvil) = setup();

    // Reserve nonce 0 — the guard holds the lock.
    let guard = manager.next_nonce().await.unwrap();
    assert_eq!(guard.nonce(), 0);

    let reset_completed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&reset_completed);
    let mgr = manager.clone();

    // Spawn a task that calls reset(). It should block because the
    // guard holds the same mutex.
    let handle = tokio::spawn(async move {
        mgr.reset().await;
        flag.store(true, Ordering::SeqCst);
    });

    // Give the spawned task time to contend on the lock.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !reset_completed.load(Ordering::SeqCst),
        "reset() must not complete while a NonceGuard is held",
    );

    // Drop the guard — reset() should now complete.
    drop(guard);
    handle.await.unwrap();
    assert!(
        reset_completed.load(Ordering::SeqCst),
        "reset() should have completed after guard was dropped",
    );

    // After the reset, next nonce should re-fetch from chain (0).
    let g = manager.next_nonce().await.unwrap();
    assert_eq!(g.nonce(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_next_nonce_and_reset_stress() {
    let (manager, _anvil) = setup();

    // Interleave next_nonce() and reset() calls concurrently to
    // exercise the retry-loop path where reset() clears the cache
    // between the peek and lock acquisition in next_nonce().
    let mut handles = Vec::new();
    for i in 0u32..100 {
        let mgr = manager.clone();
        if i % 10 == 0 {
            // Sprinkle resets to trigger the retry path.
            handles.push(tokio::spawn(async move {
                mgr.reset().await;
            }));
        } else {
            handles.push(tokio::spawn(async move {
                let guard = mgr.next_nonce().await.unwrap();
                drop(guard);
            }));
        }
    }

    // All operations must complete without deadlock or panic.
    for h in handles {
        h.await.unwrap();
    }
}
