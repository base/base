//! Stateless bond claiming for resolving, unlocking, and withdrawing dispute-game credits.

use std::{collections::HashSet, sync::Arc, time::Duration};

use alloy_primitives::Address;
use base_proof_contracts::{
    AggregateVerifierClient, DelayedWETHClient, DelayedWETHContractClient,
    DisputeGameFactoryClient, encode_claim_credit_calldata, encode_resolve_calldata,
};
use base_runtime::Clock;
use base_tx_manager::TxManager;
use futures::stream::{self, StreamExt};
use tracing::{debug, info, warn};

use crate::{ChallengeSubmitError, ChallengeSubmitter, ChallengerMetrics, GameScanner};

#[derive(Debug, Clone, Copy)]
enum BondAction {
    Resolve(Address),
    Unlock(Address),
    WithdrawIfReady { game_address: Address, bond_recipient: Address },
}

impl BondAction {
    const fn game_address(self) -> Address {
        match self {
            Self::Resolve(game_address)
            | Self::Unlock(game_address)
            | Self::WithdrawIfReady { game_address, .. } => game_address,
        }
    }
}

/// Scans recent dispute games and claims any bonds that are ready onchain.
pub struct BondManager<C: Clock> {
    claim_addresses: HashSet<Address>,
    weth_delay: Option<Duration>,
    l1_rpc_url: url::Url,
    clock: C,
    factory_client: Arc<dyn DisputeGameFactoryClient>,
    last_scan: Option<Duration>,
    lookback: u64,
    discovery_interval: Duration,
}

impl<C: Clock> std::fmt::Debug for BondManager<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BondManager").finish_non_exhaustive()
    }
}

impl<C: Clock> BondManager<C> {
    /// Creates a new bond manager for the given set of claim addresses.
    pub fn new(
        claim_addresses: Vec<Address>,
        l1_rpc_url: url::Url,
        factory_client: Arc<dyn DisputeGameFactoryClient>,
        lookback: u64,
        discovery_interval: Duration,
        clock: C,
    ) -> Self {
        let set: HashSet<Address> = claim_addresses.into_iter().collect();
        info!(count = set.len(), "bond manager initialized with claim addresses");
        Self {
            claim_addresses: set,
            weth_delay: None,
            l1_rpc_url,
            clock,
            factory_client,
            last_scan: None,
            lookback,
            discovery_interval,
        }
    }

    /// Scans the recent lookback window from scratch and advances ready bond claims.
    pub async fn discover_claimable_games<T: TxManager>(
        &mut self,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &ChallengeSubmitter<T>,
    ) -> eyre::Result<()> {
        if self.claim_addresses.is_empty() {
            debug!("bond manager is disabled, skipping discovery scan");
            return Ok(());
        }

        let now = self.clock.now();
        if !self.scan_due(now) {
            return Ok(());
        }

        let game_count = self.factory_client.game_count().await?;
        if game_count == 0 {
            self.last_scan = Some(now);
            debug!("no games found, skipping bond discovery scan");
            return Ok(());
        }

        let start_index = game_count.saturating_sub(self.lookback);
        info!(
            start = start_index,
            end = game_count,
            lookback = self.lookback,
            "scanning recent games for claimable bonds"
        );

        ChallengerMetrics::bond_discovery_scans_total("full").increment(1);
        let actions = self.bond_actions(start_index..game_count, verifier_client).await;
        let action_count = actions.len();

        for action in actions {
            self.process_action(action, verifier_client, submitter).await;
        }

        self.last_scan = Some(now);

        if action_count > 0 {
            ChallengerMetrics::bond_discovery_games_found_total().increment(action_count as u64);
            info!(actions = action_count, "bond discovery complete");
        }

        Ok(())
    }

    fn scan_due(&self, now: Duration) -> bool {
        self.last_scan
            .is_none_or(|last_scan| now.saturating_sub(last_scan) >= self.discovery_interval)
    }

    async fn bond_actions(
        &self,
        range: std::ops::Range<u64>,
        verifier_client: &dyn AggregateVerifierClient,
    ) -> Vec<BondAction> {
        stream::iter(range)
            .map(|i| self.evaluate_game_for_bonds(i, verifier_client))
            .buffer_unordered(GameScanner::SCAN_CONCURRENCY)
            .filter_map(std::future::ready)
            .collect()
            .await
    }

    async fn evaluate_game_for_bonds(
        &self,
        index: u64,
        verifier_client: &dyn AggregateVerifierClient,
    ) -> Option<BondAction> {
        let game_at = match self.factory_client.game_at_index(index).await {
            Ok(game_at) => game_at,
            Err(e) => {
                warn!(index = index, error = %e, "failed to fetch game at index");
                ChallengerMetrics::bond_evaluation_errors_total(
                    ChallengerMetrics::EVAL_ERROR_GAME_FETCH,
                )
                .increment(1);
                return None;
            }
        };
        let game_address = game_at.proxy;

        let (bond_recipient, zk_prover) = match futures::try_join!(
            verifier_client.bond_recipient(game_address),
            verifier_client.zk_prover(game_address),
        ) {
            Ok(values) => values,
            Err(e) => {
                debug!(game = %game_address, error = %e, "failed to read bond state");
                ChallengerMetrics::bond_evaluation_errors_total(
                    ChallengerMetrics::EVAL_ERROR_BOND_READ,
                )
                .increment(1);
                return None;
            }
        };

        if !self.claim_addresses.contains(&bond_recipient)
            && (zk_prover == Address::ZERO || !self.claim_addresses.contains(&zk_prover))
        {
            return None;
        }

        let resolved_at = match verifier_client.resolved_at(game_address).await {
            Ok(resolved_at) => resolved_at,
            Err(e) => {
                debug!(game = %game_address, error = %e, "failed to read bond state");
                ChallengerMetrics::bond_evaluation_errors_total(
                    ChallengerMetrics::EVAL_ERROR_BOND_READ,
                )
                .increment(1);
                return None;
            }
        };

        if resolved_at == 0 {
            let game_over = match verifier_client.game_over(game_address).await {
                Ok(game_over) => game_over,
                Err(e) => {
                    warn!(game = %game_address, error = %e, "failed to read gameOver");
                    ChallengerMetrics::bond_evaluation_errors_total(
                        ChallengerMetrics::EVAL_ERROR_PHASE_READ,
                    )
                    .increment(1);
                    return None;
                }
            };

            return game_over.then_some(BondAction::Resolve(game_address));
        }

        let bond_recipient = match verifier_client.bond_recipient(game_address).await {
            Ok(bond_recipient) => bond_recipient,
            Err(e) => {
                debug!(game = %game_address, error = %e, "failed to read bond state");
                ChallengerMetrics::bond_evaluation_errors_total(
                    ChallengerMetrics::EVAL_ERROR_BOND_READ,
                )
                .increment(1);
                return None;
            }
        };

        if !self.claim_addresses.contains(&bond_recipient) {
            return None;
        }

        let (bond_unlocked, bond_claimed) = match futures::try_join!(
            verifier_client.bond_unlocked(game_address),
            verifier_client.bond_claimed(game_address),
        ) {
            Ok(values) => values,
            Err(e) => {
                debug!(game = %game_address, error = %e, "failed to read bond state");
                ChallengerMetrics::bond_evaluation_errors_total(
                    ChallengerMetrics::EVAL_ERROR_BOND_READ,
                )
                .increment(1);
                return None;
            }
        };

        if bond_claimed {
            return None;
        }

        if bond_unlocked {
            Some(BondAction::WithdrawIfReady { game_address, bond_recipient })
        } else {
            Some(BondAction::Unlock(game_address))
        }
    }

    async fn process_action<T: TxManager>(
        &mut self,
        action: BondAction,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &ChallengeSubmitter<T>,
    ) {
        let game_address = action.game_address();
        let result = match action {
            BondAction::Resolve(game_address) => {
                self.try_resolve(game_address, submitter).await.map_err(|e| eyre::eyre!(e))
            }
            BondAction::Unlock(game_address) => {
                Self::send_claim_credit(game_address, "unlock", submitter)
                    .await
                    .map_err(|e| eyre::eyre!(e))
            }
            BondAction::WithdrawIfReady { game_address, bond_recipient } => {
                self.try_withdraw_if_ready(game_address, bond_recipient, verifier_client, submitter)
                    .await
            }
        };

        if let Err(e) = result {
            warn!(game = %game_address, error = %e, "failed to advance bond claim");
        }
    }

    async fn try_resolve<T: TxManager>(
        &self,
        game_address: Address,
        submitter: &ChallengeSubmitter<T>,
    ) -> Result<(), ChallengeSubmitError> {
        let calldata = encode_resolve_calldata();
        info!(game = %game_address, "submitting resolve transaction");
        match submitter.send_bond_tx(game_address, game_address, calldata).await {
            Ok(tx_hash) => {
                info!(game = %game_address, tx_hash = %tx_hash, "resolve transaction confirmed");
                ChallengerMetrics::resolve_tx_outcome_total(ChallengerMetrics::STATUS_SUCCESS)
                    .increment(1);
                Ok(())
            }
            Err(e) => {
                ChallengerMetrics::resolve_tx_outcome_total(ChallengerMetrics::STATUS_ERROR)
                    .increment(1);
                Err(e)
            }
        }
    }

    async fn try_withdraw_if_ready<T: TxManager>(
        &mut self,
        game_address: Address,
        bond_recipient: Address,
        verifier_client: &dyn AggregateVerifierClient,
        submitter: &ChallengeSubmitter<T>,
    ) -> eyre::Result<()> {
        let (weth_client, delay) =
            self.weth_client_and_delay(verifier_client, game_address).await?;

        let delay_started_at =
            weth_client.withdrawal_timestamp(game_address, bond_recipient).await?;
        if delay_started_at == 0 {
            debug!(game = %game_address, recipient = %bond_recipient, "bond marked unlocked without DelayedWETH withdrawal state");
            return Ok(());
        }

        let ready_at = delay_started_at.saturating_add(delay.as_secs());
        let now = self.clock.wall_clock_unix_secs();
        if now < ready_at {
            debug!(
                game = %game_address,
                ready_at = ready_at,
                now = now,
                remaining_secs = ready_at.saturating_sub(now),
                "waiting for DelayedWETH delay"
            );
            return Ok(());
        }

        Self::send_claim_credit(game_address, "withdraw", submitter).await?;
        ChallengerMetrics::bonds_completed_total().increment(1);
        Ok(())
    }

    async fn send_claim_credit<T: TxManager>(
        game_address: Address,
        step: &'static str,
        submitter: &ChallengeSubmitter<T>,
    ) -> Result<(), ChallengeSubmitError> {
        let calldata = encode_claim_credit_calldata();
        ChallengerMetrics::claim_credit_tx_submitted_total().increment(1);
        info!(game = %game_address, step, "submitting claimCredit transaction");

        let result = submitter.send_bond_tx(game_address, game_address, calldata).await;
        match &result {
            Ok(tx_hash) => {
                info!(game = %game_address, tx_hash = %tx_hash, step, "claimCredit transaction confirmed");
                ChallengerMetrics::claim_credit_tx_outcome_total(ChallengerMetrics::STATUS_SUCCESS)
                    .increment(1);
            }
            Err(_) => {
                ChallengerMetrics::claim_credit_tx_outcome_total(ChallengerMetrics::STATUS_ERROR)
                    .increment(1)
            }
        }
        result.map(|_| ())
    }

    async fn weth_client_and_delay(
        &mut self,
        verifier_client: &dyn AggregateVerifierClient,
        game_address: Address,
    ) -> eyre::Result<(DelayedWETHContractClient, Duration)> {
        let weth_address = verifier_client.delayed_weth(game_address).await?;
        let weth_client = DelayedWETHContractClient::new(weth_address, self.l1_rpc_url.clone())?;

        let Some(delay) = self.weth_delay else {
            let delay = weth_client.delay().await?;
            info!(delay_secs = delay.as_secs(), "DelayedWETH delay configured");
            self.weth_delay = Some(delay);
            return Ok((weth_client, delay));
        };

        Ok((weth_client, delay))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        future::Future,
        io::{ErrorKind, Read, Write},
        net::TcpListener,
        pin::Pin,
        sync::Arc,
        thread,
    };

    use alloy_primitives::B256;
    use futures::stream::BoxStream;

    use super::*;
    use crate::test_utils::{
        MockAggregateVerifier, MockDisputeGameFactory, SharedMockTxManager,
        TEST_DISCOVERY_INTERVAL, addr, factory_game, mock_state, receipt_with_status,
    };

    struct FixedClock {
        monotonic: Duration,
        wall_unix: u64,
    }

    impl Clock for FixedClock {
        fn now(&self) -> Duration {
            self.monotonic
        }

        fn sleep(&self, _duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
            Box::pin(std::future::pending())
        }

        fn interval(&self, _period: Duration) -> BoxStream<'static, ()> {
            Box::pin(futures::stream::pending())
        }

        fn wall_clock_unix_secs(&self) -> u64 {
            self.wall_unix
        }
    }

    fn claim_addr() -> Address {
        Address::repeat_byte(0xCC)
    }

    fn fixed_clock(wall_unix: u64) -> FixedClock {
        FixedClock { monotonic: Duration::from_secs(wall_unix), wall_unix }
    }

    fn game_state(
        bond_recipient: Address,
        zk_prover: Address,
        resolved_at: u64,
        bond_unlocked: bool,
    ) -> crate::test_utils::MockGameState {
        let mut state = mock_state(base_proof_contracts::GameStatus::InProgress, zk_prover, 100);
        state.bond_recipient = bond_recipient;
        state.resolved_at = resolved_at;
        state.bond_unlocked = bond_unlocked;
        state.game_over = true;
        state.delayed_weth = addr(9);
        state
    }

    fn verifier(state: crate::test_utils::MockGameState) -> Arc<MockAggregateVerifier> {
        Arc::new(MockAggregateVerifier::new(HashMap::from([(addr(0), state)])))
    }

    fn manager(
        claim_addr: Address,
        rpc_url: url::Url,
        clock: FixedClock,
    ) -> BondManager<FixedClock> {
        let factory = Arc::new(MockDisputeGameFactory::new(vec![factory_game(0, 0)]));
        BondManager::new(vec![claim_addr], rpc_url, factory, 1000, TEST_DISCOVERY_INTERVAL, clock)
    }

    fn bond_submitter(
        responses: Vec<base_tx_manager::SendResponse>,
    ) -> (ChallengeSubmitter<SharedMockTxManager>, SharedMockTxManager) {
        let tx_manager = SharedMockTxManager::with_responses(responses);
        (ChallengeSubmitter::new(tx_manager.clone()), tx_manager)
    }

    fn rpc_id(request: &str) -> &str {
        let Some((_, tail)) = request.split_once("\"id\"") else {
            return "0";
        };
        let Some((_, value)) = tail.split_once(':') else {
            return "0";
        };
        value
            .trim_start()
            .split([',', '}'])
            .next()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .unwrap_or("0")
    }

    fn content_length(request: &str) -> Option<usize> {
        request.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().ok())?
        })
    }

    fn u256(value: u64) -> String {
        format!("{value:064x}")
    }

    fn delayed_weth_rpc(results: Vec<String>) -> (url::Url, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap()).parse().unwrap();
        let handle = thread::spawn(move || {
            for result in results {
                let mut stream = (0..100)
                    .find_map(|_| match listener.accept() {
                        Ok((stream, _)) => Some(stream),
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                            None
                        }
                        Err(e) => panic!("accept failed: {e}"),
                    })
                    .expect("timed out waiting for DelayedWETH request");
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                loop {
                    let read = stream.read(&mut buffer).unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&request);
                    let Some((headers, body)) = text.split_once("\r\n\r\n") else {
                        continue;
                    };
                    if body.len() >= content_length(headers).unwrap_or(0) {
                        break;
                    }
                }

                let request = String::from_utf8_lossy(&request);
                let body = format!(
                    r#"{{"jsonrpc":"2.0","id":{},"result":"0x{}"}}"#,
                    rpc_id(&request),
                    result
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (url, handle)
    }

    #[tokio::test]
    async fn scan_resolves_unresolved_challenge_bond() {
        let claim_addr = claim_addr();
        let state = game_state(Address::repeat_byte(0xDD), claim_addr, 0, false);
        let verifier = verifier(state);
        let (submitter, tx_manager) =
            bond_submitter(vec![Ok(receipt_with_status(true, B256::ZERO))]);
        let mut mgr = manager(
            claim_addr,
            "http://localhost:8545".parse().unwrap(),
            fixed_clock(2_000_000_000),
        );

        mgr.discover_claimable_games(&*verifier, &submitter).await.unwrap();

        assert_eq!(tx_manager.recorded_calls().len(), 1);
    }

    #[tokio::test]
    async fn scan_ignores_zero_zk_prover_match() {
        let state = game_state(Address::repeat_byte(0xDD), Address::ZERO, 0, false);
        let verifier = verifier(state);
        let (submitter, tx_manager) = bond_submitter(vec![]);
        let mut mgr = manager(
            Address::ZERO,
            "http://localhost:8545".parse().unwrap(),
            fixed_clock(2_000_000_000),
        );

        mgr.discover_claimable_games(&*verifier, &submitter).await.unwrap();

        assert!(tx_manager.recorded_calls().is_empty());
    }

    #[tokio::test]
    async fn scan_unlocks_resolved_claimable_bond() {
        let claim_addr = claim_addr();
        let state = game_state(claim_addr, Address::ZERO, 2_000_000_000, false);
        let verifier = verifier(state);
        let (submitter, tx_manager) =
            bond_submitter(vec![Ok(receipt_with_status(true, B256::ZERO))]);
        let mut mgr = manager(
            claim_addr,
            "http://localhost:8545".parse().unwrap(),
            fixed_clock(2_000_000_000),
        );

        mgr.discover_claimable_games(&*verifier, &submitter).await.unwrap();

        assert_eq!(tx_manager.recorded_calls().len(), 1);
    }

    #[tokio::test]
    async fn scan_waits_for_delayed_weth_timestamp() {
        let claim_addr = claim_addr();
        let state = game_state(claim_addr, Address::ZERO, 2_000_000_000, true);
        let verifier = verifier(state);
        let (rpc_url, handle) =
            delayed_weth_rpc(vec![u256(60), format!("{}{}", u256(1), u256(100))]);
        let (submitter, tx_manager) = bond_submitter(vec![]);
        let mut mgr = manager(claim_addr, rpc_url, fixed_clock(150));

        mgr.discover_claimable_games(&*verifier, &submitter).await.unwrap();
        handle.join().unwrap();

        assert_eq!(verifier.delayed_weth_reads.lock().unwrap().len(), 1);
        assert!(tx_manager.recorded_calls().is_empty());
    }

    #[tokio::test]
    async fn scan_withdraws_after_delayed_weth_timestamp() {
        let claim_addr = claim_addr();
        let state = game_state(claim_addr, Address::ZERO, 2_000_000_000, true);
        let verifier = verifier(state);
        let (rpc_url, handle) =
            delayed_weth_rpc(vec![u256(60), format!("{}{}", u256(1), u256(100))]);
        let (submitter, tx_manager) =
            bond_submitter(vec![Ok(receipt_with_status(true, B256::ZERO))]);
        let mut mgr = manager(claim_addr, rpc_url, fixed_clock(160));

        mgr.discover_claimable_games(&*verifier, &submitter).await.unwrap();
        handle.join().unwrap();

        assert_eq!(verifier.delayed_weth_reads.lock().unwrap().len(), 1);
        assert_eq!(tx_manager.recorded_calls().len(), 1);
    }
}
