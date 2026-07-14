//! Stateless bond claiming for resolving, unlocking, and withdrawing dispute-game credits.

use std::{collections::HashSet, sync::Arc, time::Duration};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::Address;
use base_proof_contracts::{
    AggregateVerifierClient, DelayedWETHClient, DelayedWETHContractClient,
    DisputeGameFactoryClient, encode_claim_credit_calldata, encode_resolve_calldata,
};
use base_proof_rpc::L2Provider;
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

const FINALITY_METRIC_TIMEOUT: Duration = Duration::from_secs(5);

/// Configuration for bond discovery and claiming.
#[derive(Debug)]
pub struct BondManagerConfig {
    /// Addresses whose bonds can be claimed.
    pub claim_addresses: Vec<Address>,
    /// L1 RPC endpoint used to read the delayed WETH contract.
    pub l1_rpc_url: url::Url,
    /// Number of recent factory games to scan.
    pub lookback: u64,
    /// Minimum interval between full discovery scans.
    pub discovery_interval: Duration,
    /// Whether finality metrics are recorded.
    pub metrics_enabled: bool,
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
    l2_provider: Arc<dyn L2Provider>,
    last_scan: Option<Duration>,
    lookback: u64,
    discovery_interval: Duration,
    metrics_enabled: bool,
}

impl<C: Clock> std::fmt::Debug for BondManager<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BondManager").finish_non_exhaustive()
    }
}

impl<C: Clock> BondManager<C> {
    /// Creates a new bond manager from its configuration and contract clients.
    pub fn new(
        config: BondManagerConfig,
        factory_client: Arc<dyn DisputeGameFactoryClient>,
        l2_provider: Arc<dyn L2Provider>,
        clock: C,
    ) -> Self {
        let set: HashSet<Address> = config.claim_addresses.into_iter().collect();
        info!(count = set.len(), "bond manager initialized with claim addresses");
        Self {
            claim_addresses: set,
            weth_delay: None,
            l1_rpc_url: config.l1_rpc_url,
            clock,
            factory_client,
            l2_provider,
            last_scan: None,
            lookback: config.lookback,
            discovery_interval: config.discovery_interval,
            metrics_enabled: config.metrics_enabled,
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
        let mut finality_metric_records = Vec::new();

        for action in actions {
            if let Some(record) = self.process_action(action, verifier_client, submitter).await {
                finality_metric_records.push(record);
            }
        }

        self.record_finality_metrics(finality_metric_records, verifier_client).await;
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
    ) -> Option<(Address, u64)> {
        let game_address = action.game_address();
        let result = match action {
            BondAction::Resolve(game_address) => self
                .try_resolve(game_address, submitter)
                .await
                .map(Some)
                .map_err(|e| eyre::eyre!(e)),
            BondAction::Unlock(game_address) => {
                Self::send_claim_credit(game_address, "unlock", submitter)
                    .await
                    .map(|()| None)
                    .map_err(|e| eyre::eyre!(e))
            }
            BondAction::WithdrawIfReady { game_address, bond_recipient } => self
                .try_withdraw_if_ready(game_address, bond_recipient, verifier_client, submitter)
                .await
                .map(|()| None),
        };

        match result {
            Ok(Some(confirmed_at)) if self.metrics_enabled => Some((game_address, confirmed_at)),
            Ok(_) => None,
            Err(e) => {
                warn!(game = %game_address, error = %e, "failed to advance bond claim");
                None
            }
        }
    }

    async fn try_resolve<T: TxManager>(
        &self,
        game_address: Address,
        submitter: &ChallengeSubmitter<T>,
    ) -> Result<u64, ChallengeSubmitError> {
        let calldata = encode_resolve_calldata();
        info!(game = %game_address, "submitting resolve transaction");
        match submitter.send_bond_tx(game_address, game_address, calldata).await {
            Ok(tx_hash) => {
                let confirmed_at = self.clock.wall_clock_unix_secs();
                info!(game = %game_address, tx_hash = %tx_hash, "resolve transaction confirmed");
                ChallengerMetrics::resolve_tx_outcome_total(ChallengerMetrics::STATUS_SUCCESS)
                    .increment(1);
                Ok(confirmed_at)
            }
            Err(e) => {
                ChallengerMetrics::resolve_tx_outcome_total(ChallengerMetrics::STATUS_ERROR)
                    .increment(1);
                Err(e)
            }
        }
    }

    async fn record_finality_metrics(
        &self,
        records: Vec<(Address, u64)>,
        verifier_client: &dyn AggregateVerifierClient,
    ) {
        let collection = stream::iter(records).for_each_concurrent(
            GameScanner::SCAN_CONCURRENCY,
            |(game_address, confirmed_at)| async move {
                self.record_finality_time(game_address, verifier_client, confirmed_at).await;
            },
        );
        if tokio::time::timeout(FINALITY_METRIC_TIMEOUT, collection).await.is_err() {
            warn!(
                timeout_secs = FINALITY_METRIC_TIMEOUT.as_secs(),
                "timed out collecting finality metrics"
            );
        }
    }

    async fn record_finality_time(
        &self,
        game_address: Address,
        verifier_client: &dyn AggregateVerifierClient,
        confirmed_at: u64,
    ) {
        let l2_block_number = match verifier_client.game_info(game_address).await {
            Ok(game_info) => game_info.l2_block_number,
            Err(error) => {
                warn!(
                    game = %game_address,
                    error = %error,
                    "failed to read game info for finality metric"
                );
                return;
            }
        };
        let header = match self
            .l2_provider
            .header_by_number(BlockNumberOrTag::Number(l2_block_number))
            .await
        {
            Ok(header) => header,
            Err(error) => {
                warn!(
                    game = %game_address,
                    l2_block_number,
                    error = %error,
                    "failed to read L2 block timestamp for finality metric"
                );
                return;
            }
        };

        let finality_time_secs = confirmed_at.saturating_sub(header.timestamp);
        ChallengerMetrics::game_finality_time_seconds().record(finality_time_secs as f64);
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
    #[cfg(feature = "metrics")]
    use metrics_util::{
        MetricKind,
        debugging::{DebugValue, DebuggingRecorder},
    };

    use super::*;
    use crate::test_utils::{
        MockAggregateVerifier, MockDisputeGameFactory, MockL2Provider, SharedMockTxManager,
        TEST_DISCOVERY_INTERVAL, addr, build_test_header_and_account, factory_game, mock_state,
        receipt_with_status,
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
        metrics_enabled: bool,
    ) -> BondManager<FixedClock> {
        let factory = Arc::new(MockDisputeGameFactory::new(vec![factory_game(0, 0)]));
        let mut l2_provider = MockL2Provider::new();
        let (header, account) = build_test_header_and_account(100, B256::ZERO);
        l2_provider.insert_block(100, header, account);
        BondManager::new(
            BondManagerConfig {
                claim_addresses: vec![claim_addr],
                l1_rpc_url: rpc_url,
                lookback: 1000,
                discovery_interval: TEST_DISCOVERY_INTERVAL,
                metrics_enabled,
            },
            factory,
            Arc::new(l2_provider),
            clock,
        )
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
            false,
        );

        mgr.discover_claimable_games(&*verifier, &submitter).await.unwrap();

        assert_eq!(tx_manager.recorded_calls().len(), 1);
        assert_eq!(verifier.game_info_read_count(addr(0)), 0);
    }

    #[cfg(feature = "metrics")]
    #[test]
    fn scan_records_finality_time_when_resolving_game() {
        let claim_addr = claim_addr();
        let state = game_state(Address::repeat_byte(0xDD), claim_addr, 0, false);
        let verifier = verifier(state);
        let (submitter, _) = bond_submitter(vec![Ok(receipt_with_status(true, B256::ZERO))]);
        let mut mgr = manager(
            claim_addr,
            "http://localhost:8545".parse().unwrap(),
            fixed_clock(2_000_000_000),
            true,
        );
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime must build")
                .block_on(async {
                    mgr.discover_claimable_games(&*verifier, &submitter).await.unwrap();
                });
        });

        let snapshot = snapshotter.snapshot().into_vec();
        let finality_time = snapshot.iter().find_map(|(key, _, _, value)| {
            (key.kind() == MetricKind::Histogram
                && key.key().name() == "base_challenger.game_finality_time_seconds")
                .then_some(value)
        });
        assert_eq!(finality_time, Some(&DebugValue::Histogram(vec![2_000_000_000.0.into()])),);
        assert_eq!(verifier.game_info_read_count(addr(0)), 1);
    }

    #[cfg(feature = "metrics")]
    #[tokio::test(start_paused = true)]
    async fn scan_advances_all_bonds_before_waiting_for_finality_metrics() {
        let claim_addr = claim_addr();
        let state = game_state(Address::repeat_byte(0xDD), claim_addr, 0, false);
        let verifier = Arc::new(MockAggregateVerifier::new(HashMap::from([
            (addr(0), state.clone()),
            (addr(1), state),
        ])));
        let factory =
            Arc::new(MockDisputeGameFactory::new(vec![factory_game(0, 0), factory_game(1, 0)]));
        let mut l2_provider = MockL2Provider::new();
        let (header, account) = build_test_header_and_account(100, B256::ZERO);
        l2_provider.insert_block(100, header, account);
        l2_provider.header_delay = Some(FINALITY_METRIC_TIMEOUT + Duration::from_secs(1));
        let mut mgr = BondManager::new(
            BondManagerConfig {
                claim_addresses: vec![claim_addr],
                l1_rpc_url: "http://localhost:8545".parse().unwrap(),
                lookback: 1000,
                discovery_interval: TEST_DISCOVERY_INTERVAL,
                metrics_enabled: true,
            },
            factory,
            Arc::new(l2_provider),
            fixed_clock(2_000_000_000),
        );
        let (submitter, tx_manager) = bond_submitter(vec![
            Ok(receipt_with_status(true, B256::ZERO)),
            Ok(receipt_with_status(true, B256::ZERO)),
        ]);

        let scan =
            tokio::spawn(async move { mgr.discover_claimable_games(&*verifier, &submitter).await });
        tokio::task::yield_now().await;

        assert_eq!(tx_manager.recorded_calls().len(), 2);

        tokio::time::advance(FINALITY_METRIC_TIMEOUT).await;
        scan.await.unwrap().unwrap();
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
            false,
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
            false,
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
        let mut mgr = manager(claim_addr, rpc_url, fixed_clock(150), false);

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
        let mut mgr = manager(claim_addr, rpc_url, fixed_clock(160), false);

        mgr.discover_claimable_games(&*verifier, &submitter).await.unwrap();
        handle.join().unwrap();

        assert_eq!(verifier.delayed_weth_reads.lock().unwrap().len(), 1);
        assert_eq!(tx_manager.recorded_calls().len(), 1);
    }
}
