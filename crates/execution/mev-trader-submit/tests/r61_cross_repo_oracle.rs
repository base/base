//! Cross-repository R6-1 manifest oracle against a spawned loopback-only Anvil.
#![cfg(feature = "phase-b")]

mod support;

use std::{
    cell::Cell,
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command},
    time::{Duration, Instant},
};

use alloy_primitives::{Address, U256, address, hex};
use alloy_sol_types::{SolCall, sol};
use base_mev_trader::ExactProtocol;
use mev_trader_submit::assembler::{AssembleInput, HopExecutionParams, encode_executor_calldata};
use serde_json::Value;
use support::bytecode;

const MANIFEST_TEXT: &str = include_str!("fixtures/r61-manifest.json");
const CHAIN_ID: u64 = 8453;
const OWNER: Address = address!("1000000000000000000000000000000000000001");
const PROFIT_RECIPIENT: Address = address!("3000000000000000000000000000000000000003");
const BASE_WETH: Address = address!("4200000000000000000000000000000000000006");
const AMOUNT_IN: u128 = 1_000_000_000_000_000_000;
const PRINCIPAL: u128 = 5_000_000_000_000_000_000;
const RESERVE: u128 = 1_000_000_000_000_000_000_000;

sol! {
    function mint(address to, uint256 amount) external;
    function balanceOf(address account) external view returns (uint256);
    function setReserves(uint256 r0, uint256 r1) external;
}

fn anvil_bin() -> PathBuf {
    let path = PathBuf::from(
        std::env::var_os("ANVIL_BIN").expect("ANVIL_BIN must name the local Anvil executable"),
    );
    assert!(path.is_file(), "ANVIL_BIN is not a file: {}", path.display());
    path
}

struct AnvilGuard(Child);

impl Drop for AnvilGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Rpc {
    addr: SocketAddr,
    id: Cell<u64>,
}

impl Rpc {
    fn new(port: u16) -> Self {
        Self { addr: SocketAddr::from((Ipv4Addr::LOCALHOST, port)), id: Cell::new(0) }
    }

    fn try_call(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.id.get() + 1;
        self.id.set(id);
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
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
            .ok_or("Anvil HTTP response has no body separator")?;
        let response: Value =
            serde_json::from_slice(&raw[separator + 4..]).map_err(|error| error.to_string())?;
        if let Some(error) = response.get("error").filter(|error| !error.is_null()) {
            return Err(format!("Anvil RPC {method} failed: {error}"));
        }
        Ok(response["result"].clone())
    }

    fn call(&self, method: &str, params: Value) -> Value {
        self.try_call(method, params).unwrap_or_else(|error| panic!("{error}"))
    }

    fn call_str(&self, method: &str, params: Value) -> String {
        self.call(method, params).as_str().expect("Anvil string result").to_owned()
    }
}

fn parse_u256(value: &str) -> U256 {
    U256::from_str_radix(value.trim_start_matches("0x"), 16).expect("hex quantity")
}

fn spawn_anvil() -> (AnvilGuard, Rpc) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind loopback port");
    let port = listener.local_addr().expect("loopback address").port();
    drop(listener);
    let bin = anvil_bin();
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
        if let Ok(chain_id) = rpc.try_call("eth_chainId", serde_json::json!([]))
            && parse_u256(chain_id.as_str().unwrap_or("0x0")) == U256::from(CHAIN_ID)
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

fn wait_for_receipt(rpc: &Rpc, hash: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let receipt = rpc.call("eth_getTransactionReceipt", serde_json::json!([hash]));
        if !receipt.is_null() {
            return receipt;
        }
        assert!(Instant::now() < deadline, "missing receipt for {hash}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn send(rpc: &Rpc, to: Option<Address>, data: &[u8]) -> Value {
    rpc.call("anvil_impersonateAccount", serde_json::json!([OWNER.to_string()]));
    let mut tx = serde_json::json!({
        "from": OWNER.to_string(),
        "data": hex::encode_prefixed(data),
        "gas": "0xb71b00",
    });
    if let Some(to) = to {
        tx["to"] = Value::String(to.to_string());
    }
    let hash = rpc.call_str("eth_sendTransaction", serde_json::json!([tx]));
    wait_for_receipt(rpc, &hash)
}

fn encode_addresses(addresses: &[Address]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(addresses.len() * 32);
    for address in addresses {
        encoded.extend_from_slice(&[0u8; 12]);
        encoded.extend_from_slice(address.as_slice());
    }
    encoded
}

fn deploy(rpc: &Rpc, creation: &str, constructor_args: &[u8]) -> Address {
    let mut data = hex::decode(creation).expect("manifest creation bytecode must be hex");
    data.extend_from_slice(constructor_args);
    let receipt = send(rpc, None, &data);
    assert_eq!(receipt["status"], "0x1", "manifest contract deployment reverted");
    receipt["contractAddress"]
        .as_str()
        .expect("deployed contract address")
        .parse()
        .expect("address")
}

fn manifest_creation<'a>(manifest: &'a Value, section: &str, name: &str) -> &'a str {
    let entries = manifest
        .get(section)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("manifest {section} must be an array"));
    let mut matches =
        entries.iter().filter(|entry| entry.get("name").and_then(Value::as_str) == Some(name));
    let entry =
        matches.next().unwrap_or_else(|| panic!("manifest is missing {section} contract {name}"));
    assert!(matches.next().is_none(), "manifest contains duplicate {section} contract {name}");
    let creation = entry
        .get("creation_bytecode_hex")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("manifest contract {name} has no creation bytecode"));
    assert!(
        creation.starts_with("0x") && creation.len() > 2,
        "manifest contract {name} bytecode is empty"
    );
    hex::decode(creation)
        .unwrap_or_else(|_| panic!("manifest contract {name} bytecode is not hex"));
    creation
}

fn mint(rpc: &Rpc, token: Address, to: Address, amount: U256) {
    let receipt = send(rpc, Some(token), &mintCall { to, amount }.abi_encode());
    assert_eq!(receipt["status"], "0x1", "token mint reverted");
}

fn balance(rpc: &Rpc, token: Address, account: Address) -> U256 {
    let result = rpc.call_str(
        "eth_call",
        serde_json::json!([{
            "to": token.to_string(),
            "data": hex::encode_prefixed(balanceOfCall { account }.abi_encode()),
        }, "latest"]),
    );
    parse_u256(&result)
}

fn seed_aerodrome(rpc: &Rpc, pool: Address, reserve0: U256, reserve1: U256) {
    let receipt =
        send(rpc, Some(pool), &setReservesCall { r0: reserve0, r1: reserve1 }.abi_encode());
    assert_eq!(receipt["status"], "0x1", "Aerodrome reserve initialization reverted");
}

fn calldata_for(
    protocols: [ExactProtocol; 2],
    pools: [Address; 2],
    adapters: [Address; 2],
    token: Address,
    executor: Address,
) -> Vec<u8> {
    let (victim_raw, victim) = support::victim_with_priority(1);
    let mut plan = support::backrun_plan(protocols, victim);
    plan.route[0].pool = pools[0];
    plan.route[0].token_in = BASE_WETH;
    plan.route[0].token_out = token;
    plan.route[1].pool = pools[1];
    plan.route[1].token_in = token;
    plan.route[1].token_out = BASE_WETH;
    plan.amount_in = U256::from(AMOUNT_IN);
    plan.amount_out = U256::from(AMOUNT_IN + 1);
    plan.gross_profit = U256::ONE;
    support::finalize_plan_digest(&mut plan);
    encode_executor_calldata(&AssembleInput {
        plan: &plan,
        current_frame: support::matching_frame(&plan),
        executor,
        hops: [
            HopExecutionParams { adapter: adapters[0], min_amount_out: U256::ONE },
            HopExecutionParams { adapter: adapters[1], min_amount_out: U256::ONE },
        ],
        chain_id: CHAIN_ID,
        nonce: 0,
        gas: 12_000_000,
        max_fee_per_gas: 1,
        valid_until_block: 1_000_000,
        victim_raw_tx: &victim_raw,
        victim_tx_hash: victim,
        expected_victim_priority_fee: Some(1),
    })
    .expect("production executor calldata encoding must succeed")
}

fn assert_selector_and_funding(calldata: &[u8], first: Address, second: Address) {
    assert_eq!(&calldata[..4], &[0x3b, 0x83, 0xf2, 0x72], "R6-1 selector mismatch");
    assert_eq!(calldata.len(), 4 + 17 * 32, "R6-1 static calldata word count changed");
    for (word, expected) in [(6usize, first), (13usize, second)] {
        let start = 4 + word * 32;
        assert_eq!(
            &calldata[start..start + 12],
            &[0u8; 12],
            "fundingTarget word is not an address"
        );
        assert_eq!(
            &calldata[start + 12..start + 32],
            expected.as_slice(),
            "fundingTarget word {word} mismatch"
        );
    }
}

#[test]
fn r61_manifest_drives_pool_and_adapter_funding_oracle() {
    let manifest: Value = serde_json::from_str(MANIFEST_TEXT).expect("vendored R6-1 manifest JSON");
    assert_eq!(
        manifest.get("generated_from_commit").and_then(Value::as_str),
        Some("4a41e9fcd63c46b142de568d22a72c1ec9f2b812"),
        "vendored manifest source commit changed"
    );
    assert_eq!(
        manifest.get("artifact_sha256").and_then(Value::as_str),
        Some("844b5d1876e26c0aa7345fb18e8b6aa6d0d62c88360d1cef10e523349965e1ac"),
        "vendored manifest artifact seal changed"
    );
    let executor_creation = bytecode::executor_creation();
    assert_eq!(
        executor_creation,
        manifest_creation(&manifest, "contracts", "BlinkAtomicExecutor"),
        "executor accessor must return the manifest bytecode"
    );

    let (_anvil, rpc) = spawn_anvil();
    rpc.call("anvil_setBalance", serde_json::json!([OWNER.to_string(), "0x3635c9adc5dea00000"]));

    let temporary_weth =
        deploy(&rpc, manifest_creation(&manifest, "dependency_fixtures", "MockWETH"), &[]);
    let weth_runtime =
        rpc.call_str("eth_getCode", serde_json::json!([temporary_weth.to_string(), "latest"]));
    assert_ne!(weth_runtime, "0x", "MockWETH deployed runtime is empty");
    rpc.call("anvil_setCode", serde_json::json!([BASE_WETH.to_string(), weth_runtime]));
    rpc.call(
        "anvil_setBalance",
        serde_json::json!([
            BASE_WETH.to_string(),
            format!("0x{:x}", U256::from(100u64) * U256::from(AMOUNT_IN)),
        ]),
    );

    let token = deploy(&rpc, manifest_creation(&manifest, "dependency_fixtures", "MockERC20"), &[]);
    let aero_adapter =
        deploy(&rpc, manifest_creation(&manifest, "contracts", "AerodromeAdapter"), &[]);
    let v3_adapter = deploy(&rpc, manifest_creation(&manifest, "contracts", "UniV3Adapter"), &[]);
    let aero_first = deploy(
        &rpc,
        manifest_creation(&manifest, "dependency_fixtures", "MockAerodromePool"),
        &encode_addresses(&[BASE_WETH, token]),
    );
    let aero_second = deploy(
        &rpc,
        manifest_creation(&manifest, "dependency_fixtures", "MockAerodromePool"),
        &encode_addresses(&[token, BASE_WETH]),
    );
    let v3_first = deploy(
        &rpc,
        manifest_creation(&manifest, "dependency_fixtures", "MockUniV3Pool"),
        &encode_addresses(&[BASE_WETH, token]),
    );
    let v3_second = deploy(
        &rpc,
        manifest_creation(&manifest, "dependency_fixtures", "MockUniV3Pool"),
        &encode_addresses(&[token, BASE_WETH]),
    );
    let executor =
        deploy(&rpc, executor_creation, &encode_addresses(&[OWNER, BASE_WETH, PROFIT_RECIPIENT]));

    mint(&rpc, BASE_WETH, executor, U256::from(PRINCIPAL));
    seed_aerodrome(&rpc, aero_first, U256::from(RESERVE), U256::from(2 * RESERVE));
    seed_aerodrome(&rpc, aero_second, U256::from(RESERVE), U256::from(RESERVE));
    mint(&rpc, token, v3_first, U256::from(2 * AMOUNT_IN));
    mint(&rpc, BASE_WETH, v3_second, U256::from(2 * AMOUNT_IN));

    let pool_funded = calldata_for(
        [ExactProtocol::AerodromeVolatile, ExactProtocol::AerodromeVolatile],
        [aero_first, aero_second],
        [aero_adapter, aero_adapter],
        token,
        executor,
    );
    assert_selector_and_funding(&pool_funded, aero_first, aero_second);
    let receipt = send(&rpc, Some(executor), &pool_funded);
    assert_eq!(receipt["status"], "0x1", "pool-funded executor call reverted");
    assert_eq!(
        balance(&rpc, BASE_WETH, executor),
        U256::from(PRINCIPAL),
        "executor principal changed"
    );
    assert_eq!(
        balance(&rpc, BASE_WETH, aero_adapter),
        U256::ZERO,
        "Aerodrome adapter retained WETH"
    );
    assert_eq!(balance(&rpc, token, aero_adapter), U256::ZERO, "Aerodrome adapter retained token");
    assert_eq!(balance(&rpc, token, executor), U256::ZERO, "executor retained intermediate token");
    assert!(
        balance(&rpc, BASE_WETH, aero_first) > U256::from(RESERVE),
        "first pool was not funded"
    );
    assert!(balance(&rpc, token, aero_second) > U256::from(RESERVE), "second pool was not funded");

    let adapter_funded = calldata_for(
        [ExactProtocol::UniswapV3, ExactProtocol::UniswapV3],
        [v3_first, v3_second],
        [v3_adapter, v3_adapter],
        token,
        executor,
    );
    assert_selector_and_funding(&adapter_funded, v3_adapter, v3_adapter);
    let receipt = send(&rpc, Some(executor), &adapter_funded);
    assert_eq!(receipt["status"], "0x1", "adapter-funded V3 executor call reverted");
    assert_eq!(
        balance(&rpc, BASE_WETH, executor),
        U256::from(PRINCIPAL),
        "V3 changed executor principal"
    );
    assert_eq!(balance(&rpc, BASE_WETH, v3_adapter), U256::ZERO, "V3 adapter retained WETH");
    assert_eq!(balance(&rpc, token, v3_adapter), U256::ZERO, "V3 adapter retained token");
    assert_eq!(
        balance(&rpc, token, executor),
        U256::ZERO,
        "V3 left intermediate token on executor"
    );
    assert_eq!(
        balance(&rpc, BASE_WETH, v3_first),
        U256::from(AMOUNT_IN),
        "first V3 callback pool did not receive adapter-funded WETH"
    );
    assert_eq!(
        balance(&rpc, token, v3_second),
        U256::from(AMOUNT_IN),
        "second V3 callback pool did not receive adapter-funded token"
    );

    // TODO(Claude): add wrong-target revert and full rollback assertions with the adversarial mock body.
}
