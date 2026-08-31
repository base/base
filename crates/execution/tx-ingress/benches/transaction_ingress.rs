//! End-to-end transaction admission benchmark for JSON-RPC and streaming gRPC.
//!
//! Each transport gets a fresh in-process node with identical genesis state and submits the same
//! pre-signed transaction corpus through the node's real transaction admission path. Node startup,
//! transaction signing, and connection warm-up are excluded from measurements.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p base-tx-ingress --bench transaction_ingress
//! ```
//!
//! Configure the workload with `TX_INGRESS_BENCH_TRANSACTIONS`,
//! `TX_INGRESS_BENCH_SENDERS`, `TX_INGRESS_BENCH_IN_FLIGHT`, and
//! `TX_INGRESS_BENCH_REPETITIONS`. `TX_INGRESS_BENCH_REQUEST_TIMEOUT_MS` bounds each
//! submission so an overloaded transport is reported instead of stalling the benchmark.

use std::{
    collections::HashMap,
    env,
    fmt::{self, Display, Formatter},
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::SignableTransaction;
use alloy_eips::eip2718::Encodable2718;
use alloy_genesis::GenesisAccount;
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_network::Base;
use base_common_rpc_types::BaseTransactionRequest;
use base_execution_chainspec::BaseChainSpec;
use base_node_runner::test_utils::TestHarness;
use base_test_utils::{DEVNET_CHAIN_ID, GENESIS_GAS_LIMIT, build_test_genesis};
use base_tx_ingress::{
    SubmitRequest, TransactionIngressExtension, submit_response,
    transaction_ingress_service_client::TransactionIngressServiceClient,
};
use eyre::{Result, bail, eyre};
use futures::{StreamExt, stream::FuturesUnordered};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

const DEFAULT_TRANSACTION_COUNT: usize = 5_000;
const DEFAULT_REPETITIONS: usize = 1;
const DEFAULT_REQUEST_TIMEOUT_MS: usize = 10_000;
const DEFAULT_IN_FLIGHT: &[usize] = &[1, 64, 256, 1024];
const ACCOUNT_BALANCE: u128 = 100_000_000_000_000_000_000;

#[derive(Debug, Clone)]
struct BenchmarkConfig {
    transactions: usize,
    senders: usize,
    in_flight: Vec<usize>,
    repetitions: usize,
    request_timeout: Duration,
}

impl BenchmarkConfig {
    fn from_env() -> Result<Self> {
        let transactions = parse_env("TX_INGRESS_BENCH_TRANSACTIONS", DEFAULT_TRANSACTION_COUNT)?;
        let senders = parse_env("TX_INGRESS_BENCH_SENDERS", transactions)?;
        let repetitions = parse_env("TX_INGRESS_BENCH_REPETITIONS", DEFAULT_REPETITIONS)?;
        let request_timeout_ms =
            parse_env("TX_INGRESS_BENCH_REQUEST_TIMEOUT_MS", DEFAULT_REQUEST_TIMEOUT_MS)?;
        let in_flight = match env::var("TX_INGRESS_BENCH_IN_FLIGHT") {
            Ok(value) => value
                .split(',')
                .map(|item| {
                    item.trim().parse::<usize>().map_err(|error| {
                        eyre!("invalid TX_INGRESS_BENCH_IN_FLIGHT value {item:?}: {error}")
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            Err(env::VarError::NotPresent) => DEFAULT_IN_FLIGHT.to_vec(),
            Err(error) => return Err(error.into()),
        };

        if transactions == 0 {
            bail!("TX_INGRESS_BENCH_TRANSACTIONS must be greater than zero");
        }
        if senders == 0 {
            bail!("TX_INGRESS_BENCH_SENDERS must be greater than zero");
        }
        if repetitions == 0 {
            bail!("TX_INGRESS_BENCH_REPETITIONS must be greater than zero");
        }
        if request_timeout_ms == 0 {
            bail!("TX_INGRESS_BENCH_REQUEST_TIMEOUT_MS must be greater than zero");
        }
        if in_flight.is_empty() || in_flight.contains(&0) {
            bail!("TX_INGRESS_BENCH_IN_FLIGHT must contain positive integers");
        }

        Ok(Self {
            transactions,
            senders,
            in_flight,
            repetitions,
            request_timeout: Duration::from_millis(request_timeout_ms as u64),
        })
    }
}

#[derive(Debug, Clone)]
struct BenchmarkTransaction {
    raw: Bytes,
    hash: B256,
}

#[derive(Debug, Clone, Copy)]
enum Transport {
    JsonRpc,
    Grpc,
}

impl Display for Transport {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::JsonRpc => f.write_str("jsonrpc"),
            Self::Grpc => f.write_str("grpc"),
        }
    }
}

#[derive(Debug)]
struct Completion {
    latency: Duration,
    accepted: bool,
    error: Option<String>,
}

#[derive(Debug)]
struct TrialReport {
    transport: Transport,
    in_flight: usize,
    repetition: usize,
    elapsed: Duration,
    latencies: Vec<Duration>,
    accepted: usize,
    rejected: usize,
    first_error: Option<String>,
}

impl TrialReport {
    fn new(
        transport: Transport,
        in_flight: usize,
        repetition: usize,
        elapsed: Duration,
        completions: Vec<Completion>,
    ) -> Self {
        let mut accepted = 0;
        let mut rejected = 0;
        let mut first_error = None;
        let latencies = completions
            .into_iter()
            .map(|completion| {
                if completion.accepted {
                    accepted += 1;
                } else {
                    rejected += 1;
                    if first_error.is_none() {
                        first_error = completion.error;
                    }
                }
                completion.latency
            })
            .collect();

        Self {
            transport,
            in_flight,
            repetition,
            elapsed,
            latencies,
            accepted,
            rejected,
            first_error,
        }
    }

    fn print_csv(&mut self, transactions: usize, senders: usize) {
        self.latencies.sort_unstable();
        let mean = self.latencies.iter().map(Duration::as_secs_f64).sum::<f64>()
            / self.latencies.len() as f64;
        let throughput = transactions as f64 / self.elapsed.as_secs_f64();
        println!(
            "{},{},{},{},{},{:.3},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{},{},{}",
            self.transport,
            self.in_flight,
            self.repetition,
            transactions,
            senders,
            self.elapsed.as_secs_f64() * 1000.0,
            throughput,
            mean * 1_000_000.0,
            percentile_micros(&self.latencies, 500),
            percentile_micros(&self.latencies, 950),
            percentile_micros(&self.latencies, 990),
            percentile_micros(&self.latencies, 999),
            self.accepted,
            self.rejected,
            self.first_error.as_deref().unwrap_or("").replace(',', ";"),
        );
    }
}

fn main() -> Result<()> {
    tokio::runtime::Runtime::new()?.block_on(run())
}

async fn run() -> Result<()> {
    let config = BenchmarkConfig::from_env()?;
    let (chain_spec, transactions) = build_workload(&config)?;
    let workers = std::thread::available_parallelism().map_or(1, usize::from);

    eprintln!(
        "tx ingress benchmark: transactions={}, senders={}, in_flight={:?}, repetitions={}, request_timeout_ms={}, workers={workers}",
        config.transactions,
        config.senders,
        config.in_flight,
        config.repetitions,
        config.request_timeout.as_millis(),
    );
    println!(
        "transport,in_flight,repetition,transactions,senders,elapsed_ms,tx_per_second,mean_us,p50_us,p95_us,p99_us,p999_us,accepted,rejected,first_error"
    );

    for repetition in 0..config.repetitions {
        for &in_flight in &config.in_flight {
            let transports = if repetition % 2 == 0 {
                [Transport::JsonRpc, Transport::Grpc]
            } else {
                [Transport::Grpc, Transport::JsonRpc]
            };
            for transport in transports {
                let mut report = run_trial(
                    transport,
                    Arc::clone(&chain_spec),
                    &transactions,
                    in_flight.min(config.transactions),
                    repetition,
                    config.request_timeout,
                )
                .await?;
                report.print_csv(config.transactions, config.senders);
            }
        }
    }

    Ok(())
}

async fn run_trial(
    transport: Transport,
    chain_spec: Arc<BaseChainSpec>,
    transactions: &[BenchmarkTransaction],
    in_flight: usize,
    repetition: usize,
    request_timeout: Duration,
) -> Result<TrialReport> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let grpc_address = listener.local_addr()?;
    let extension = TransactionIngressExtension::from_listener(listener)?;
    let harness = TestHarness::builder()
        .with_chain_spec(chain_spec)
        .with_extension(extension)
        .build()
        .await?;

    match transport {
        Transport::JsonRpc => {
            run_json_rpc_trial(&harness, transactions, in_flight, repetition, request_timeout).await
        }
        Transport::Grpc => {
            run_grpc_trial(
                &harness,
                grpc_address,
                transactions,
                in_flight,
                repetition,
                request_timeout,
            )
            .await
        }
    }
}

async fn run_json_rpc_trial(
    harness: &TestHarness,
    transactions: &[BenchmarkTransaction],
    in_flight: usize,
    repetition: usize,
    request_timeout: Duration,
) -> Result<TrialReport> {
    let provider = harness.provider();
    let _ = tokio::time::timeout(request_timeout, provider.send_raw_transaction(&[0xff])).await;

    let started = Instant::now();
    let mut next = 0;
    let mut submissions = FuturesUnordered::new();
    while next < transactions.len() && submissions.len() < in_flight {
        submissions.push(submit_json_rpc(
            provider.clone(),
            transactions[next].clone(),
            request_timeout,
        ));
        next += 1;
    }

    let mut completions = Vec::with_capacity(transactions.len());
    while let Some(completion) = submissions.next().await {
        completions.push(completion);
        if next < transactions.len() {
            submissions.push(submit_json_rpc(
                provider.clone(),
                transactions[next].clone(),
                request_timeout,
            ));
            next += 1;
        }
    }

    Ok(TrialReport::new(Transport::JsonRpc, in_flight, repetition, started.elapsed(), completions))
}

async fn submit_json_rpc(
    provider: RootProvider<Base>,
    transaction: BenchmarkTransaction,
    request_timeout: Duration,
) -> Completion {
    let started = Instant::now();
    match tokio::time::timeout(request_timeout, provider.send_raw_transaction(&transaction.raw))
        .await
    {
        Ok(Ok(pending)) if *pending.tx_hash() == transaction.hash => {
            Completion { latency: started.elapsed(), accepted: true, error: None }
        }
        Ok(Ok(pending)) => Completion {
            latency: started.elapsed(),
            accepted: false,
            error: Some(format!(
                "hash mismatch: expected {}, received {}",
                transaction.hash,
                pending.tx_hash()
            )),
        },
        Ok(Err(error)) => Completion {
            latency: started.elapsed(),
            accepted: false,
            error: Some(error.to_string()),
        },
        Err(_) => Completion {
            latency: started.elapsed(),
            accepted: false,
            error: Some(format!("request timed out after {} ms", request_timeout.as_millis())),
        },
    }
}

async fn run_grpc_trial(
    _harness: &TestHarness,
    grpc_address: SocketAddr,
    transactions: &[BenchmarkTransaction],
    in_flight: usize,
    repetition: usize,
    request_timeout: Duration,
) -> Result<TrialReport> {
    let mut client =
        TransactionIngressServiceClient::connect(format!("http://{grpc_address}")).await?;
    let (requests, request_stream) = mpsc::channel(in_flight);
    let mut responses = client.submit(ReceiverStream::new(request_stream)).await?.into_inner();

    tokio::time::timeout(
        request_timeout,
        requests.send(SubmitRequest { request_id: u64::MAX, raw_transaction: vec![0xff] }),
    )
    .await??;
    tokio::time::timeout(request_timeout, responses.message())
        .await??
        .ok_or_else(|| eyre!("gRPC stream ended during warm-up"))?;

    let started = Instant::now();
    let mut next = 0;
    let mut started_at = HashMap::with_capacity(in_flight);
    while next < transactions.len() && started_at.len() < in_flight {
        send_grpc_request(&requests, transactions, &mut started_at, next, request_timeout).await?;
        next += 1;
    }

    let mut completions = Vec::with_capacity(transactions.len());
    while completions.len() < transactions.len() {
        let response = tokio::time::timeout(request_timeout, responses.message())
            .await??
            .ok_or_else(|| eyre!("gRPC stream ended with requests in flight"))?;
        let request_id = usize::try_from(response.request_id)?;
        let request_started = started_at
            .remove(&request_id)
            .ok_or_else(|| eyre!("response for unknown request id {}", response.request_id))?;
        let transaction = transactions
            .get(request_id)
            .ok_or_else(|| eyre!("response request id {} is out of range", response.request_id))?;
        let (accepted, error) = match response.outcome {
            Some(submit_response::Outcome::TransactionHash(hash))
                if hash.as_slice() == transaction.hash.as_slice() =>
            {
                (true, None)
            }
            Some(submit_response::Outcome::TransactionHash(hash)) => (
                false,
                Some(format!(
                    "hash mismatch: expected {}, received 0x{}",
                    transaction.hash,
                    alloy_primitives::hex::encode(hash)
                )),
            ),
            Some(submit_response::Outcome::Error(error)) => {
                (false, Some(format!("{} ({})", error.message, error.code)))
            }
            None => (false, Some("response had no outcome".to_owned())),
        };
        completions.push(Completion { latency: request_started.elapsed(), accepted, error });

        if next < transactions.len() {
            send_grpc_request(&requests, transactions, &mut started_at, next, request_timeout)
                .await?;
            next += 1;
        }
    }

    Ok(TrialReport::new(Transport::Grpc, in_flight, repetition, started.elapsed(), completions))
}

async fn send_grpc_request(
    requests: &mpsc::Sender<SubmitRequest>,
    transactions: &[BenchmarkTransaction],
    started_at: &mut HashMap<usize, Instant>,
    request_id: usize,
    request_timeout: Duration,
) -> Result<()> {
    let transaction = &transactions[request_id];
    started_at.insert(request_id, Instant::now());
    tokio::time::timeout(
        request_timeout,
        requests.send(SubmitRequest {
            request_id: request_id as u64,
            raw_transaction: transaction.raw.to_vec(),
        }),
    )
    .await??;
    Ok(())
}

fn build_workload(
    config: &BenchmarkConfig,
) -> Result<(Arc<BaseChainSpec>, Vec<BenchmarkTransaction>)> {
    let signers = (0..config.senders)
        .map(|index| {
            let key = B256::from(U256::from(index + 1).to_be_bytes::<32>());
            PrivateKeySigner::from_bytes(&key).map_err(Into::into)
        })
        .collect::<Result<Vec<_>>>()?;
    let accounts = signers
        .iter()
        .map(|signer| {
            (signer.address(), GenesisAccount::default().with_balance(U256::from(ACCOUNT_BALANCE)))
        })
        .collect::<Vec<_>>();
    let genesis = build_test_genesis().extend_accounts(accounts).with_gas_limit(GENESIS_GAS_LIMIT);
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(genesis));

    let transactions = (0..config.transactions)
        .map(|index| {
            let signer = &signers[index % signers.len()];
            let nonce = (index / signers.len()) as u64;
            let request = BaseTransactionRequest::default()
                .from(signer.address())
                .transaction_type(2_u8)
                .with_gas_limit(21_000)
                .with_max_fee_per_gas(1_000_000_000)
                .with_max_priority_fee_per_gas(0)
                .with_chain_id(DEVNET_CHAIN_ID)
                .to(Address::ZERO)
                .with_value(U256::from(1))
                .with_nonce(nonce);
            let transaction = request
                .build_typed_tx()
                .map_err(|error| eyre!("failed to build transaction {index}: {error:?}"))?;
            let signature = signer
                .sign_hash_sync(&transaction.signature_hash())
                .map_err(|error| eyre!("failed to sign transaction {index}: {error}"))?;
            let raw = Bytes::from(transaction.into_signed(signature).encoded_2718());
            let hash = keccak256(&raw);
            Ok(BenchmarkTransaction { raw, hash })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok((chain_spec, transactions))
}

fn percentile_micros(sorted: &[Duration], permille: usize) -> f64 {
    let index = ((sorted.len() - 1) * permille).div_ceil(1000);
    sorted[index].as_secs_f64() * 1_000_000.0
}

fn parse_env(name: &str, default: usize) -> Result<usize> {
    match env::var(name) {
        Ok(value) => {
            value.parse().map_err(|error| eyre!("invalid {name} value {value:?}: {error}"))
        }
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error.into()),
    }
}
