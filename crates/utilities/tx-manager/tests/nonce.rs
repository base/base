//! Integration tests for [`NonceManager`] with an Anvil backend.

use alloy_node_bindings::Anvil;
use alloy_provider::RootProvider;
use base_tx_manager::{NonceGuard, NonceManager};

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

#[tokio::test]
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
async fn nonce_guard_is_send() {
    /// Asserts that `T` implements [`Send`].
    fn assert_send<T: Send>() {}
    assert_send::<NonceGuard>();
}
