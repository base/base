//! Local-anvil end-to-end: the rung-2 ephemeral-signed backrun is sent to a
//! spawned loopback anvil and must (a) execute the two-hop WETH closed loop with a
//! ceil-75% kickback and preserved principal, and (b) revert on strict-minOut when
//! pool state drifts below the sized floor. Mirrors the TS `runRung2EphemeralPaperSim`.
//!
//! Red-line: the RPC endpoint is a `127.0.0.1` socket constructed here from
//! `Ipv4Addr::LOCALHOST` (there is NO url/endpoint parameter). The child is a
//! spawned anvil. There is no real sequencer/Blink connection. `ANVIL_BIN` is
//! mandatory and every harness startup/readiness failure fails the test.
#![cfg(feature = "phase-b")]

mod support;

use std::{
    cell::{Cell, RefCell},
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command},
    time::{Duration, Instant},
};

use alloy_consensus::TxEip1559;
use alloy_primitives::{
    Address, B256, Bytes, TxKind, U256, address,
    aliases::{U24, U112},
    hex,
};
use alloy_sol_types::{SolCall, sol};
use base_mev_trader::ExactProtocol;
use mev_trader_submit::{
    BLINK_OFA_KICKBACK_RECIPIENT,
    assembler::{AssembleInput, HopExecutionParams, encode_executor_calldata},
    signer::build_and_sign_ephemeral_atomic_tx,
};
use support::bytecode;

const CHAIN_ID: u64 = 8453;
const OWNER: Address = address!("1000000000000000000000000000000000000001");
const PROFIT_RECIPIENT: Address = address!("3000000000000000000000000000000000000003");
const DRIFTER: Address = address!("4000000000000000000000000000000000000004");
const BASE_WETH: Address = address!("4200000000000000000000000000000000000006");

const AMOUNT_IN: u128 = 1_000_000_000_000_000_000; // 1 WETH
const EXECUTOR_PRINCIPAL: u128 = 5_000_000_000_000_000_000; // 5 WETH
const FIRST_WETH_RESERVE: u128 = 1_000_000_000_000_000_000_000; // 1000
const FIRST_TOKEN_RESERVE: u128 = 2_000_000_000_000_000_000_000; // 2000
const SECOND_TOKEN_RESERVE: u128 = 1_000_000_000_000_000_000_000; // 1000
const SECOND_WETH_RESERVE: u128 = 1_000_000_000_000_000_000_000; // 1000
const FEE_BPS: u32 = 30;
const FEE_PIPS: u32 = 3_000;
const GAS_LIMIT: u64 = 3_000_000;
const MAX_FEE_PER_GAS: u128 = 2_000_000_000;
const VICTIM_PRIORITY_FEE: u128 = 1_000_000;

sol! {
    function deposit() external payable;
    function transfer(address to, uint256 amount) external returns (bool);
    function mint(address to, uint256 amount) external;
    function setReserves(uint112 r0, uint112 r1) external;
    function balanceOf(address account) external view returns (uint256);
    function swap(address pool, address tokenIn, uint256 amountIn, uint256 minOut, uint256 feeBps)
        external returns (uint256);
}

/// The impersonated EOA authorized as the executor for the Aerodrome scenario.
const AERO_CALLER: Address = address!("5000000000000000000000000000000000000005");

/// `MockAerodromePool.setReserves(uint256,uint256)` — distinct selector from the
/// v2 `setReserves(uint112,uint112)`, so it lives in its own ABI namespace.
mod aero_abi {
    alloy_sol_types::sol! {
        function setReserves(uint256 r0, uint256 r1) external;
    }
}

/// The executor entrypoint, used ONLY to hand-encode the NAIVE mispriced calldata
/// (`fee_pips` passed straight through as `feeBps`) that the R8 fee-SOURCE path
/// would never emit — proving the executor's strict-minOut reverts it.
mod exec_abi {
    alloy_sol_types::sol! {
        struct SwapHop {
            address adapter;
            address pool;
            address tokenIn;
            address tokenOut;
            uint24 feeBps;
            uint256 minAmountOut;
            address fundingTarget;
        }
        function executeBlinkOfaAtomic(
            SwapHop firstHop,
            SwapHop secondHop,
            uint256 amountIn,
            uint256 minFinalAmount,
            uint256 validUntilBlock
        );
    }
}

fn anvil_bin() -> PathBuf {
    let path = PathBuf::from(
        std::env::var_os("ANVIL_BIN").expect("ANVIL_BIN must name the local anvil executable"),
    );
    assert!(path.is_file(), "ANVIL_BIN is not a file: {}", path.display());
    path
}

/// Kills the spawned anvil on drop so an assertion panic never leaks the child.
struct AnvilGuard(Child);
impl Drop for AnvilGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Minimal loopback JSON-RPC client: one HTTP/1.1 POST per call to a `127.0.0.1`
/// socket. The address is built from `Ipv4Addr::LOCALHOST` — never a parameter.
struct Rpc {
    addr: SocketAddr,
    id: Cell<u64>,
}

impl Rpc {
    fn new(port: u16) -> Self {
        Self { addr: SocketAddr::from((Ipv4Addr::LOCALHOST, port)), id: Cell::new(0) }
    }

    fn try_call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let id = self.id.get() + 1;
        self.id.set(id);
        let body =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
                .to_string();
        let request = format!(
            "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let mut stream = TcpStream::connect(self.addr).map_err(|error| error.to_string())?;
        stream.set_read_timeout(Some(Duration::from_secs(20))).ok();
        stream.write_all(request.as_bytes()).map_err(|error| error.to_string())?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).map_err(|error| error.to_string())?;
        let separator = raw
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .ok_or("no HTTP body separator")?;
        let value: serde_json::Value =
            serde_json::from_slice(&raw[separator + 4..]).map_err(|error| error.to_string())?;
        if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
            return Err(format!("rpc error for {method}: {error}"));
        }
        Ok(value["result"].clone())
    }

    fn call(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.try_call(method, params).unwrap_or_else(|error| panic!("{error}"))
    }

    fn call_str(&self, method: &str, params: serde_json::Value) -> String {
        self.call(method, params).as_str().expect("string result").to_owned()
    }
}

fn hex_quantity(value: U256) -> String {
    format!("0x{value:x}")
}

fn parse_u256(hex_value: &str) -> U256 {
    U256::from_str_radix(hex_value.trim_start_matches("0x"), 16).expect("hex quantity")
}

fn find_free_port() -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

fn spawn_anvil() -> (AnvilGuard, Rpc) {
    let bin = anvil_bin();
    let port = find_free_port();
    let child = Command::new(&bin)
        .args([
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--chain-id",
            &CHAIN_ID.to_string(),
            "--silent",
        ])
        .spawn()
        .unwrap_or_else(|error| panic!("failed to spawn ANVIL_BIN {}: {error}", bin.display()));
    let guard = AnvilGuard(child);
    let rpc = Rpc::new(port);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(result) = rpc.try_call("eth_chainId", serde_json::json!([]))
            && parse_u256(result.as_str().unwrap_or("0x0")) == U256::from(CHAIN_ID)
        {
            return (guard, rpc);
        }
        assert!(
            Instant::now() <= deadline,
            "ANVIL_BIN {} failed readiness on 127.0.0.1:{port}",
            bin.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_receipt(rpc: &Rpc, hash: &str) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let receipt = rpc.call("eth_getTransactionReceipt", serde_json::json!([hash]));
        if !receipt.is_null() {
            return receipt;
        }
        assert!(Instant::now() < deadline, "no receipt for {hash}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn send_impersonated(
    rpc: &Rpc,
    from: Address,
    to: Option<Address>,
    data: &[u8],
    value: Option<U256>,
) -> serde_json::Value {
    rpc.call("anvil_impersonateAccount", serde_json::json!([from.to_string()]));
    let mut tx = serde_json::json!({
        "from": from.to_string(),
        "data": hex::encode_prefixed(data),
        "gas": hex_quantity(U256::from(12_000_000u64)),
    });
    if let Some(to) = to {
        tx["to"] = serde_json::Value::String(to.to_string());
    }
    if let Some(value) = value {
        tx["value"] = serde_json::Value::String(hex_quantity(value));
    }
    let hash = rpc.call_str("eth_sendTransaction", serde_json::json!([tx]));
    wait_for_receipt(rpc, &hash)
}

fn assert_r61_executor_dispatcher_seal(creation_hex: &str) {
    let creation = hex::decode(creation_hex).expect("sealed executor creation bytecode");
    assert!(
        creation.windows(4).any(|window| window == [0x3b, 0x83, 0xf2, 0x72]),
        "R6-1 executor creation must contain the new dispatcher selector"
    );
    assert!(
        !creation.windows(4).any(|window| window == [0x21, 0xde, 0xf2, 0x96]),
        "R6-1 executor creation must not contain the retired dispatcher selector"
    );
}

fn deploy(rpc: &Rpc, creation_hex: &str, constructor_args: &[u8]) -> Address {
    let mut data = hex::decode(creation_hex).expect("creation bytecode");
    data.extend_from_slice(constructor_args);
    let receipt = send_impersonated(rpc, OWNER, None, &data, None);
    assert_eq!(receipt["status"], "0x1", "deployment reverted");
    receipt["contractAddress"].as_str().expect("contract address").parse().expect("address")
}

fn encode_addresses(addresses: &[Address]) -> Vec<u8> {
    let mut out = Vec::with_capacity(addresses.len() * 32);
    for address in addresses {
        out.extend_from_slice(&[0u8; 12]);
        out.extend_from_slice(address.as_slice());
    }
    out
}

fn call_balance_of(rpc: &Rpc, token: Address, account: Address) -> U256 {
    let data = balanceOfCall { account }.abi_encode();
    let result = rpc.call_str(
        "eth_call",
        serde_json::json!([{ "to": token.to_string(), "data": hex::encode_prefixed(data) }, "latest"]),
    );
    parse_u256(&result)
}

fn native_balance(rpc: &Rpc, account: Address) -> U256 {
    parse_u256(&rpc.call_str("eth_getBalance", serde_json::json!([account.to_string(), "latest"])))
}

fn quote_v2(amount_in: U256, reserve_in: U256, reserve_out: U256, fee_bps: u32) -> U256 {
    let amount_in_with_fee = amount_in * U256::from(10_000 - fee_bps);
    let numerator = amount_in_with_fee * reserve_out;
    let denominator = reserve_in * U256::from(10_000u32) + amount_in_with_fee;
    numerator / denominator
}

/// Deployed fixture + sized expectations for one scenario.
#[derive(Clone, Copy)]
struct Fixture {
    token: Address,
    first_pair: Address,
    second_pair: Address,
    adapter: Address,
    executor: Address,
    expected_intermediate: U256,
    expected_final: U256,
}

fn deploy_fixture(rpc: &Rpc, ephemeral_signer: Address) -> Fixture {
    // Deploy MockWETH once and stamp its runtime code at the canonical BASE_WETH.
    let temp_weth = deploy(rpc, bytecode::MOCK_WETH_CREATION, &[]);
    let weth_runtime =
        rpc.call_str("eth_getCode", serde_json::json!([temp_weth.to_string(), "latest"]));
    assert_ne!(weth_runtime, "0x", "MockWETH runtime missing");
    rpc.call("anvil_setCode", serde_json::json!([BASE_WETH.to_string(), weth_runtime]));

    let token = deploy(rpc, bytecode::MOCK_ERC20_CREATION, &[]);
    let first_pair =
        deploy(rpc, bytecode::MOCK_PAIR_CREATION, &encode_addresses(&[BASE_WETH, token]));
    let second_pair =
        deploy(rpc, bytecode::MOCK_PAIR_CREATION, &encode_addresses(&[token, BASE_WETH]));
    let adapter = deploy(rpc, bytecode::UNIV2_ADAPTER_CREATION, &[]);
    let executor_creation = bytecode::executor_creation();
    assert_r61_executor_dispatcher_seal(executor_creation);
    let executor = deploy(
        rpc,
        executor_creation,
        &encode_addresses(&[ephemeral_signer, BASE_WETH, PROFIT_RECIPIENT]),
    );

    // Fund WETH: OWNER deposits ETH, then seeds the executor principal.
    let deposit = send_impersonated(
        rpc,
        OWNER,
        Some(BASE_WETH),
        &depositCall {}.abi_encode(),
        Some(U256::from(10u64) * U256::from(AMOUNT_IN)),
    );
    assert_eq!(deposit["status"], "0x1", "WETH deposit reverted");
    let fund = send_impersonated(
        rpc,
        OWNER,
        Some(BASE_WETH),
        &transferCall { to: executor, amount: U256::from(EXECUTOR_PRINCIPAL) }.abi_encode(),
        None,
    );
    assert_eq!(fund["status"], "0x1", "executor funding reverted");

    seed_reserves(rpc, first_pair, FIRST_WETH_RESERVE, FIRST_TOKEN_RESERVE);
    seed_reserves(rpc, second_pair, SECOND_TOKEN_RESERVE, SECOND_WETH_RESERVE);

    let expected_intermediate = quote_v2(
        U256::from(AMOUNT_IN),
        U256::from(FIRST_WETH_RESERVE),
        U256::from(FIRST_TOKEN_RESERVE),
        FEE_BPS,
    );
    let expected_final = quote_v2(
        expected_intermediate,
        U256::from(SECOND_TOKEN_RESERVE),
        U256::from(SECOND_WETH_RESERVE),
        FEE_BPS,
    );
    assert!(expected_final > U256::from(AMOUNT_IN), "fixture must be gross-positive");

    Fixture {
        token,
        first_pair,
        second_pair,
        adapter,
        executor,
        expected_intermediate,
        expected_final,
    }
}

fn seed_reserves(rpc: &Rpc, pair: Address, r0: u128, r1: u128) {
    let data = setReservesCall { r0: U112::from(r0), r1: U112::from(r1) }.abi_encode();
    let receipt = send_impersonated(rpc, OWNER, Some(pair), &data, None);
    assert_eq!(receipt["status"], "0x1", "reserve seed reverted");
}

fn victim_envelope() -> (Vec<u8>, alloy_primitives::B256) {
    let (raw, hash) = support::victim_with_priority(VICTIM_PRIORITY_FEE);
    (raw, hash)
}

/// Build the unsigned backrun for a deployed fixture (sized to `expected_final`).
fn build_unsigned(
    fixture: &Fixture,
    plan: &base_mev_trader::BackrunPlan,
    victim_raw: &[u8],
    victim_hash: alloy_primitives::B256,
) -> TxEip1559 {
    let input = AssembleInput {
        plan,
        current_frame: support::matching_frame(plan),
        executor: fixture.executor,
        hops: [
            HopExecutionParams {
                adapter: fixture.adapter,
                min_amount_out: fixture.expected_intermediate,
            },
            HopExecutionParams { adapter: fixture.adapter, min_amount_out: U256::from(1u64) },
        ],
        chain_id: CHAIN_ID,
        nonce: 0,
        gas: GAS_LIMIT,
        max_fee_per_gas: MAX_FEE_PER_GAS,
        valid_until_block: 1_000_000,
        victim_raw_tx: victim_raw,
        victim_tx_hash: victim_hash,
        expected_victim_priority_fee: Some(VICTIM_PRIORITY_FEE),
    };
    let calldata = encode_executor_calldata(&input).expect("calldata");
    TxEip1559 {
        chain_id: CHAIN_ID,
        nonce: 0,
        gas_limit: GAS_LIMIT,
        max_fee_per_gas: MAX_FEE_PER_GAS,
        max_priority_fee_per_gas: VICTIM_PRIORITY_FEE,
        to: TxKind::Call(fixture.executor),
        value: U256::ZERO,
        access_list: Default::default(),
        input: Bytes::from(calldata),
    }
}

fn plan_for(fixture: &Fixture, victim: alloy_primitives::B256) -> base_mev_trader::BackrunPlan {
    let mut plan =
        support::backrun_plan([ExactProtocol::UniswapV2, ExactProtocol::UniswapV2], victim);
    plan.route[0].pool = fixture.first_pair;
    plan.route[0].token_in = BASE_WETH;
    plan.route[0].token_out = fixture.token;
    plan.route[1].pool = fixture.second_pair;
    plan.route[1].token_in = fixture.token;
    plan.route[1].token_out = BASE_WETH;
    plan.amount_in = U256::from(AMOUNT_IN);
    plan.amount_out = fixture.expected_final;
    plan.gross_profit = fixture.expected_final - U256::from(AMOUNT_IN);
    // Fields (not fee_pips) were mutated after `backrun_plan`; re-seal the digest so
    // the assembler's field-integrity gate accepts the finalized plan.
    support::finalize_plan_digest(&mut plan);
    plan
}

fn inject_drift(rpc: &Rpc, fixture: &Fixture) {
    let drift = U256::from(250u64) * U256::from(AMOUNT_IN);
    let mint = send_impersonated(
        rpc,
        OWNER,
        Some(fixture.token),
        &mintCall { to: DRIFTER, amount: drift }.abi_encode(),
        None,
    );
    assert_eq!(mint["status"], "0x1");
    let transfer = send_impersonated(
        rpc,
        DRIFTER,
        Some(fixture.token),
        &transferCall { to: fixture.second_pair, amount: drift }.abi_encode(),
        None,
    );
    assert_eq!(transfer["status"], "0x1");
    let swap = send_impersonated(
        rpc,
        DRIFTER,
        Some(fixture.adapter),
        &swapCall {
            pool: fixture.second_pair,
            tokenIn: fixture.token,
            amountIn: drift,
            minOut: U256::from(1u64),
            feeBps: U256::from(FEE_BPS),
        }
        .abi_encode(),
        None,
    );
    assert_eq!(swap["status"], "0x1", "drift swap reverted");
}

/// Deployed `AerodromeVolatile` fixture + sized expectations.
#[derive(Clone, Copy)]
struct AeroFixture {
    token: Address,
    first_pool: Address,
    second_pool: Address,
    adapter: Address,
    executor: Address,
    expected_intermediate: U256,
    expected_final: U256,
}

fn seed_aero_reserves(rpc: &Rpc, pool: Address, r0: u128, r1: u128) {
    let data = aero_abi::setReservesCall { r0: U256::from(r0), r1: U256::from(r1) }.abi_encode();
    let receipt = send_impersonated(rpc, OWNER, Some(pool), &data, None);
    assert_eq!(receipt["status"], "0x1", "aero reserve seed reverted");
}

fn deploy_aero_fixture(rpc: &Rpc, caller: Address) -> AeroFixture {
    let temp_weth = deploy(rpc, bytecode::MOCK_WETH_CREATION, &[]);
    let weth_runtime =
        rpc.call_str("eth_getCode", serde_json::json!([temp_weth.to_string(), "latest"]));
    assert_ne!(weth_runtime, "0x", "MockWETH runtime missing");
    rpc.call("anvil_setCode", serde_json::json!([BASE_WETH.to_string(), weth_runtime]));

    let token = deploy(rpc, bytecode::MOCK_ERC20_CREATION, &[]);
    let first_pool =
        deploy(rpc, bytecode::MOCK_AERODROME_POOL_CREATION, &encode_addresses(&[BASE_WETH, token]));
    let second_pool =
        deploy(rpc, bytecode::MOCK_AERODROME_POOL_CREATION, &encode_addresses(&[token, BASE_WETH]));
    let adapter = deploy(rpc, bytecode::AERODROME_ADAPTER_CREATION, &[]);
    let executor_creation = bytecode::executor_creation();
    assert_r61_executor_dispatcher_seal(executor_creation);
    let executor =
        deploy(rpc, executor_creation, &encode_addresses(&[caller, BASE_WETH, PROFIT_RECIPIENT]));

    let deposit = send_impersonated(
        rpc,
        OWNER,
        Some(BASE_WETH),
        &depositCall {}.abi_encode(),
        Some(U256::from(10u64) * U256::from(AMOUNT_IN)),
    );
    assert_eq!(deposit["status"], "0x1", "WETH deposit reverted");
    let fund = send_impersonated(
        rpc,
        OWNER,
        Some(BASE_WETH),
        &transferCall { to: executor, amount: U256::from(EXECUTOR_PRINCIPAL) }.abi_encode(),
        None,
    );
    assert_eq!(fund["status"], "0x1", "executor funding reverted");

    seed_aero_reserves(rpc, first_pool, FIRST_WETH_RESERVE, FIRST_TOKEN_RESERVE);
    seed_aero_reserves(rpc, second_pool, SECOND_TOKEN_RESERVE, SECOND_WETH_RESERVE);

    let expected_intermediate = quote_v2(
        U256::from(AMOUNT_IN),
        U256::from(FIRST_WETH_RESERVE),
        U256::from(FIRST_TOKEN_RESERVE),
        FEE_BPS,
    );
    let expected_final = quote_v2(
        expected_intermediate,
        U256::from(SECOND_TOKEN_RESERVE),
        U256::from(SECOND_WETH_RESERVE),
        FEE_BPS,
    );
    assert!(expected_final > U256::from(AMOUNT_IN), "aero fixture must be gross-positive");

    AeroFixture {
        token,
        first_pool,
        second_pool,
        adapter,
        executor,
        expected_intermediate,
        expected_final,
    }
}

fn aero_plan_for(fixture: &AeroFixture, victim: B256) -> base_mev_trader::BackrunPlan {
    let mut plan = support::backrun_plan(
        [ExactProtocol::AerodromeVolatile, ExactProtocol::AerodromeVolatile],
        victim,
    );
    plan.route[0].pool = fixture.first_pool;
    plan.route[0].token_in = BASE_WETH;
    plan.route[0].token_out = fixture.token;
    plan.route[1].pool = fixture.second_pool;
    plan.route[1].token_in = fixture.token;
    plan.route[1].token_out = BASE_WETH;
    plan.amount_in = U256::from(AMOUNT_IN);
    plan.amount_out = fixture.expected_final;
    plan.gross_profit = fixture.expected_final - U256::from(AMOUNT_IN);
    support::finalize_plan_digest(&mut plan);
    plan
}

#[test]
fn aerodrome_volatile_fee_parity_and_passthrough_revert() {
    let (_anvil, rpc) = spawn_anvil();
    for funded in [OWNER, DRIFTER, AERO_CALLER] {
        rpc.call(
            "anvil_setBalance",
            serde_json::json!([
                funded.to_string(),
                hex_quantity(U256::from(100u64) * U256::from(AMOUNT_IN))
            ]),
        );
    }

    // ---- Fee-parity: the assembler converts the carried fee_pips (3000) to feeBps
    // 30, and the AerodromeVolatile ON-CHAIN output equals the constant-product
    // sizing quote (sizing == execution). ----
    let parity = deploy_aero_fixture(&rpc, AERO_CALLER);
    let plan = aero_plan_for(&parity, B256::repeat_byte(0xab));
    let input = AssembleInput {
        plan: &plan,
        current_frame: support::matching_frame(&plan),
        executor: parity.executor,
        hops: [
            HopExecutionParams {
                adapter: parity.adapter,
                min_amount_out: parity.expected_intermediate,
            },
            HopExecutionParams { adapter: parity.adapter, min_amount_out: U256::from(1u64) },
        ],
        chain_id: CHAIN_ID,
        nonce: 0,
        gas: GAS_LIMIT,
        max_fee_per_gas: MAX_FEE_PER_GAS,
        valid_until_block: 1_000_000,
        victim_raw_tx: &[],
        victim_tx_hash: plan.victim,
        expected_victim_priority_fee: None,
    };
    let calldata = encode_executor_calldata(&input).expect("aero fee-source calldata");

    let kickback_before = native_balance(&rpc, BLINK_OFA_KICKBACK_RECIPIENT);
    let residual_before = call_balance_of(&rpc, BASE_WETH, PROFIT_RECIPIENT);
    let principal_before = call_balance_of(&rpc, BASE_WETH, parity.executor);
    let receipt = send_impersonated(&rpc, AERO_CALLER, Some(parity.executor), &calldata, None);
    assert_eq!(receipt["status"], "0x1", "aero fee-parity backrun did not execute");

    let realized = (native_balance(&rpc, BLINK_OFA_KICKBACK_RECIPIENT) - kickback_before)
        + (call_balance_of(&rpc, BASE_WETH, PROFIT_RECIPIENT) - residual_before);
    assert_eq!(
        realized,
        parity.expected_final - U256::from(AMOUNT_IN),
        "aero realized profit != sized profit (fee-parity broken)"
    );
    assert_eq!(
        call_balance_of(&rpc, BASE_WETH, parity.executor),
        principal_before,
        "aero principal not preserved"
    );
    assert_eq!(
        call_balance_of(&rpc, parity.token, parity.executor),
        U256::ZERO,
        "aero intermediate token retained"
    );

    // ---- Passthrough revert: the NAIVE calldata passes fee_pips (3000) straight
    // through as feeBps (100x the correct 30 bps), so AerodromeAdapter under-delivers
    // below the per-hop floor — the whole tx reverts and atomically rolls back. ----
    let drifted = deploy_aero_fixture(&rpc, AERO_CALLER);
    let naive = exec_abi::executeBlinkOfaAtomicCall {
        firstHop: exec_abi::SwapHop {
            adapter: drifted.adapter,
            pool: drifted.first_pool,
            tokenIn: BASE_WETH,
            tokenOut: drifted.token,
            // NAIVE mispricing: fee_pips as feeBps (30% fee, not the correct 0.30%).
            feeBps: U24::from(FEE_PIPS),
            minAmountOut: drifted.expected_intermediate,
            fundingTarget: drifted.first_pool,
        },
        secondHop: exec_abi::SwapHop {
            adapter: drifted.adapter,
            pool: drifted.second_pool,
            tokenIn: drifted.token,
            tokenOut: BASE_WETH,
            feeBps: U24::from(FEE_PIPS),
            minAmountOut: U256::from(1u64),
            fundingTarget: drifted.second_pool,
        },
        amountIn: U256::from(AMOUNT_IN),
        minFinalAmount: drifted.expected_final,
        validUntilBlock: U256::from(1_000_000u64),
    }
    .abi_encode();

    let weth_before = call_balance_of(&rpc, BASE_WETH, drifted.executor);
    let token_before = call_balance_of(&rpc, drifted.token, drifted.executor);
    let receipt = send_impersonated(&rpc, AERO_CALLER, Some(drifted.executor), &naive, None);
    assert_eq!(receipt["status"], "0x0", "naive feeBps=fee_pips passthrough did not revert");
    assert_eq!(
        call_balance_of(&rpc, BASE_WETH, drifted.executor),
        weth_before,
        "aero WETH state not rolled back"
    );
    assert_eq!(
        call_balance_of(&rpc, drifted.token, drifted.executor),
        token_before,
        "aero token state not rolled back"
    );
}

#[test]
fn ephemeral_backrun_executes_and_reverts_on_drift() {
    let (_anvil, rpc) = spawn_anvil();
    for funded in [OWNER, DRIFTER] {
        rpc.call(
            "anvil_setBalance",
            serde_json::json!([
                funded.to_string(),
                hex_quantity(U256::from(100u64) * U256::from(AMOUNT_IN))
            ]),
        );
    }

    // ---- Scenario A: success ----
    let (victim_raw, victim_hash) = victim_envelope();
    let captured: RefCell<Option<Fixture>> = RefCell::new(None);
    let signed = build_and_sign_ephemeral_atomic_tx(|signer_address| {
        rpc.call(
            "anvil_setBalance",
            serde_json::json!([signer_address.to_string(), hex_quantity(U256::from(AMOUNT_IN))]),
        );
        let fixture = deploy_fixture(&rpc, signer_address);
        let plan = plan_for(&fixture, victim_hash);
        let unsigned = build_unsigned(&fixture, &plan, &victim_raw, victim_hash);
        *captured.borrow_mut() = Some(fixture);
        unsigned
    })
    .expect("ephemeral sign");
    assert!(signed.verification.recovered_signer && signed.verification.canonical_low_s);
    let fixture = captured.borrow().as_ref().copied().expect("fixture");

    let kickback_before = native_balance(&rpc, BLINK_OFA_KICKBACK_RECIPIENT);
    let residual_before = call_balance_of(&rpc, BASE_WETH, PROFIT_RECIPIENT);
    let principal_before = call_balance_of(&rpc, BASE_WETH, fixture.executor);

    let hash = rpc.call_str(
        "eth_sendRawTransaction",
        serde_json::json!([hex::encode_prefixed(&signed.raw_backrun)]),
    );
    let receipt = wait_for_receipt(&rpc, &hash);
    assert_eq!(receipt["status"], "0x1", "backrun did not execute");

    let kickback_delta = native_balance(&rpc, BLINK_OFA_KICKBACK_RECIPIENT) - kickback_before;
    let residual_delta = call_balance_of(&rpc, BASE_WETH, PROFIT_RECIPIENT) - residual_before;
    let principal_after = call_balance_of(&rpc, BASE_WETH, fixture.executor);
    let realized_profit = kickback_delta + residual_delta;
    let expected_kickback =
        (realized_profit * U256::from(7_500u32)).div_ceil(U256::from(10_000u32));

    assert_eq!(
        realized_profit,
        fixture.expected_final - U256::from(AMOUNT_IN),
        "realized profit != sized profit"
    );
    assert_eq!(kickback_delta, expected_kickback, "kickback is not ceil(75%)");
    assert!(
        kickback_delta * U256::from(10_000u32) >= realized_profit * U256::from(7_500u32),
        "kickback below 75%"
    );
    assert!(residual_delta > U256::ZERO, "residual profit not paid");
    assert_eq!(principal_after, principal_before, "principal not preserved");
    assert_eq!(
        principal_after,
        U256::from(EXECUTOR_PRINCIPAL),
        "principal not equal to seeded principal"
    );
    assert_eq!(
        call_balance_of(&rpc, fixture.token, fixture.executor),
        U256::ZERO,
        "intermediate token retained"
    );

    // ---- Scenario B: strict-minOut revert on drift ----
    let (victim_raw_b, victim_hash_b) = victim_envelope();
    let captured_b: RefCell<Option<Fixture>> = RefCell::new(None);
    let signed_b = build_and_sign_ephemeral_atomic_tx(|signer_address| {
        rpc.call(
            "anvil_setBalance",
            serde_json::json!([signer_address.to_string(), hex_quantity(U256::from(AMOUNT_IN))]),
        );
        let fixture = deploy_fixture(&rpc, signer_address);
        let plan = plan_for(&fixture, victim_hash_b);
        let unsigned = build_unsigned(&fixture, &plan, &victim_raw_b, victim_hash_b);
        // Drift the second pool AFTER sizing so realized output falls below the floor.
        inject_drift(&rpc, &fixture);
        *captured_b.borrow_mut() = Some(fixture);
        unsigned
    })
    .expect("ephemeral sign (drift)");
    let fixture_b = captured_b.borrow().as_ref().copied().expect("fixture b");

    let executor_weth_before = call_balance_of(&rpc, BASE_WETH, fixture_b.executor);
    let executor_token_before = call_balance_of(&rpc, fixture_b.token, fixture_b.executor);
    let hash_b = rpc.call_str(
        "eth_sendRawTransaction",
        serde_json::json!([hex::encode_prefixed(&signed_b.raw_backrun)]),
    );
    let receipt_b = wait_for_receipt(&rpc, &hash_b);
    assert_eq!(receipt_b["status"], "0x0", "drifted backrun did not revert");
    // Atomic rollback: the executor's business balances are unchanged.
    assert_eq!(
        call_balance_of(&rpc, BASE_WETH, fixture_b.executor),
        executor_weth_before,
        "WETH state not rolled back"
    );
    assert_eq!(
        call_balance_of(&rpc, fixture_b.token, fixture_b.executor),
        executor_token_before,
        "token state not rolled back"
    );
    // The signer nonce advanced on the reverted tx (gas charged; real submission).
    let nonce = parse_u256(&rpc.call_str(
        "eth_getTransactionCount",
        serde_json::json!([signed_b.signer_address.to_string(), "latest"]),
    ));
    assert_eq!(nonce, U256::from(1u64), "signer nonce did not advance on revert");
}
