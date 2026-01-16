#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod block_driver;
mod cli;

#[cfg(feature = "tui")]
mod tui;

use std::{
    fs,
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

#[cfg(feature = "tui")]
use std::sync::Mutex;

use base_primitives::{DEVNET_CHAIN_ID, build_test_genesis};
use clap::Parser;
use cli::DevnetArgs;
use eyre::{Result, eyre};
use tracing::{error, info, warn};

#[cfg(feature = "tui")]
use tui::{App, LogState, TuiResult, init_terminal, restore_terminal, run_tui};

/// Version string set by build.rs
const VERSION: &str = env!("BASE_DEVNET_VERSION");

fn main() -> Result<()> {
    // Initialize version info
    base_cli_utils::Version::init();

    let args = DevnetArgs::parse();

    // TUI mode vs standard logging mode
    #[cfg(feature = "tui")]
    if args.no_tui {
        run_standard(args)
    } else {
        run_with_tui(args)
    }

    #[cfg(not(feature = "tui"))]
    run_standard(args)
}

/// Run devnet with TUI interface.
#[cfg(feature = "tui")]
fn run_with_tui(args: DevnetArgs) -> Result<()> {
    // Create data directories
    let data_dir = args.data_dir();
    fs::create_dir_all(&data_dir)?;
    fs::create_dir_all(args.client_datadir())?;
    fs::create_dir_all(args.builder_datadir())?;

    // Generate and write genesis file
    let genesis_path = data_dir.join("genesis.json");
    write_genesis(&genesis_path)?;

    // Generate JWT secret for Engine API auth
    let jwt_path = data_dir.join("jwt.hex");
    write_jwt_secret(&jwt_path)?;

    // Initialize terminal once
    let mut terminal = init_terminal()?;

    // Setup shutdown signal (only once, before the loop)
    let ctrl_c_pressed = Arc::new(AtomicBool::new(false));
    let ctrl_c_flag = Arc::clone(&ctrl_c_pressed);
    ctrlc_handler(move || {
        ctrl_c_flag.store(true, Ordering::SeqCst);
    });

    // Main loop (supports restart)
    loop {
        // Reset ctrl-c flag for this run
        ctrl_c_pressed.store(false, Ordering::SeqCst);

        // Create fresh log state for each run
        let log_state = Arc::new(Mutex::new(LogState::new()));

        let mut processes: Vec<Child> = Vec::new();

        // Launch client node with TUI log capture
        if !args.no_client {
            match launch_client_tui(&args, &genesis_path, &jwt_path, Arc::clone(&log_state)) {
                Ok(client) => {
                    let mut state = log_state.lock().unwrap();
                    state.client_pid = Some(client.id());
                    state.client_running = true;
                    drop(state);
                    processes.push(client);
                }
                Err(e) => {
                    log_state
                        .lock()
                        .unwrap()
                        .push_client_log(format!("[ERR] Failed to start: {e}"));
                }
            }
        }

        // Give the client time to start
        if !args.no_client && !args.no_builder {
            thread::sleep(Duration::from_secs(2));
        }

        // Launch builder node with TUI log capture
        if !args.no_builder {
            match launch_builder_tui(&args, &genesis_path, &jwt_path, Arc::clone(&log_state)) {
                Ok(builder) => {
                    let mut state = log_state.lock().unwrap();
                    state.builder_pid = Some(builder.id());
                    state.builder_running = true;
                    drop(state);
                    processes.push(builder);
                }
                Err(e) => {
                    log_state
                        .lock()
                        .unwrap()
                        .push_builder_log(format!("[ERR] Failed to start: {e}"));
                }
            }
        }

        // Start block driver in background (unless --no-driver)
        if !args.no_driver {
            let block_driver_config = block_driver::BlockDriverConfig {
                engine_url: format!("http://127.0.0.1:{}", args.engine_port),
                rpc_url: format!("http://127.0.0.1:{}", args.http_port),
                block_time_ms: args.block_time_ms,
            };

            let log_state_driver = Arc::clone(&log_state);
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
                rt.block_on(async {
                    if let Err(e) = block_driver::run_block_driver(block_driver_config).await {
                        log_state_driver
                            .lock()
                            .unwrap()
                            .push_client_log(format!("[DRIVER] Error: {e}"));
                    }
                });
            });
        } else {
            info!(
                "Block driver disabled (--no-driver). Connect external consensus to drive blocks."
            );
        }

        // Create fresh app state
        let mut app =
            App::new(Arc::clone(&log_state), DEVNET_CHAIN_ID, args.http_port, args.ws_port);

        // Run TUI event loop
        let result = run_tui(&mut terminal, &mut app)?;

        // Shutdown all processes
        for mut proc in processes {
            let _ = proc.kill();
            let _ = proc.wait();
        }

        // Check if we should restart or quit
        match result {
            TuiResult::Restart => {
                // Small delay to ensure ports are released
                thread::sleep(Duration::from_millis(500));
                continue;
            }
            TuiResult::Quit => {
                break;
            }
        }
    }

    // Restore terminal
    restore_terminal(&mut terminal)?;

    Ok(())
}

/// Run devnet with standard console logging.
fn run_standard(args: DevnetArgs) -> Result<()> {
    // Setup logging
    setup_logging(&args.log_level);

    info!("Starting Base Devnet v{VERSION}");
    info!("Chain ID: {DEVNET_CHAIN_ID}");

    // Create data directories
    let data_dir = args.data_dir();
    fs::create_dir_all(&data_dir)?;
    fs::create_dir_all(args.client_datadir())?;
    fs::create_dir_all(args.builder_datadir())?;

    // Generate and write genesis file
    let genesis_path = data_dir.join("genesis.json");
    write_genesis(&genesis_path)?;
    info!("Genesis written to: {}", genesis_path.display());

    // Generate JWT secret for Engine API auth
    let jwt_path = data_dir.join("jwt.hex");
    write_jwt_secret(&jwt_path)?;

    // Setup shutdown signal handler
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc_handler(move || {
        r.store(false, Ordering::SeqCst);
    });

    let mut processes: Vec<Child> = Vec::new();

    // Launch client node
    if !args.no_client {
        info!("Starting base-reth-node (client)...");
        let client = launch_client(&args, &genesis_path, &jwt_path)?;
        processes.push(client);
    }

    // Give the client time to start before launching builder
    if !args.no_client && !args.no_builder {
        thread::sleep(Duration::from_secs(3));
    }

    // Launch builder node
    if !args.no_builder {
        info!("Starting base-builder...");
        let builder = launch_builder(&args, &genesis_path, &jwt_path)?;
        processes.push(builder);
    }

    // Print connection info
    print_connection_info(&args);

    // Start block driver in background (unless --no-driver)
    if !args.no_driver {
        let block_driver_config = block_driver::BlockDriverConfig {
            engine_url: format!("http://127.0.0.1:{}", args.engine_port),
            rpc_url: format!("http://127.0.0.1:{}", args.http_port),
            block_time_ms: args.block_time_ms,
        };

        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
            rt.block_on(async {
                if let Err(e) = block_driver::run_block_driver(block_driver_config).await {
                    error!("Block driver error: {}", e);
                }
            });
        });
    } else {
        info!("Block driver disabled (--no-driver). Connect external consensus to drive blocks.");
    }

    // Wait for shutdown signal or process exit
    info!("Devnet running. Press Ctrl+C to stop.");
    while running.load(Ordering::SeqCst) {
        // Check if any process has exited unexpectedly
        for (i, proc) in processes.iter_mut().enumerate() {
            match proc.try_wait() {
                Ok(Some(status)) => {
                    warn!("Process {i} exited with status: {status}");
                    running.store(false, Ordering::SeqCst);
                    break;
                }
                Ok(None) => {} // Still running
                Err(e) => {
                    error!("Error checking process {i}: {e}");
                }
            }
        }
        thread::sleep(Duration::from_millis(500));
    }

    // Shutdown all processes
    info!("Shutting down devnet...");
    for mut proc in processes {
        if let Err(e) = proc.kill() {
            warn!("Failed to kill process: {e}");
        }
        let _ = proc.wait();
    }

    info!("Devnet stopped.");
    Ok(())
}

fn setup_logging(level: &str) {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

    tracing_subscriber::registry().with(fmt::layer().with_target(true)).with(filter).init();
}

fn ctrlc_handler<F: FnOnce() + Send + 'static>(handler: F) {
    let handler = std::sync::Mutex::new(Some(handler));
    ctrlc::set_handler(move || {
        if let Some(h) = handler.lock().unwrap().take() {
            h();
        }
    })
    .expect("Error setting Ctrl-C handler");
}

fn write_genesis(path: &PathBuf) -> Result<()> {
    let genesis = build_test_genesis();
    let genesis_json = serde_json::to_string_pretty(&genesis)?;
    fs::write(path, genesis_json)?;
    Ok(())
}

fn write_jwt_secret(path: &PathBuf) -> Result<()> {
    // Use all-zeros JWT secret to match DEFAULT_JWT_SECRET in test_utils
    let jwt_secret = "0000000000000000000000000000000000000000000000000000000000000000";
    fs::write(path, jwt_secret)?;
    Ok(())
}

/// Find a binary, checking ./target/release/ first, then PATH.
fn find_binary(name: &str) -> PathBuf {
    let release_path = PathBuf::from("./target/release").join(name);
    if release_path.exists() { release_path } else { PathBuf::from(name) }
}

fn launch_client(args: &DevnetArgs, genesis_path: &PathBuf, jwt_path: &PathBuf) -> Result<Child> {
    let mut cmd = Command::new(find_binary("base-reth-node"));

    // Reduce log noise - filter out repetitive forkchoice/payload messages
    cmd.env("RUST_LOG", "info,reth_engine_util::engine_store=warn,reth::consensus::engine=warn");

    cmd.arg("node")
        .arg("--chain")
        .arg(genesis_path)
        .arg("--datadir")
        .arg(args.client_datadir())
        .arg("--http")
        .arg("--http.addr")
        .arg("0.0.0.0")
        .arg("--http.port")
        .arg(args.http_port.to_string())
        .arg("--http.api")
        .arg("eth,net,web3,debug,trace,txpool")
        .arg("--ws")
        .arg("--ws.addr")
        .arg("0.0.0.0")
        .arg("--ws.port")
        .arg(args.ws_port.to_string())
        .arg("--ws.api")
        .arg("eth,net,web3,debug,trace,txpool")
        .arg("--authrpc.addr")
        .arg("127.0.0.1")
        .arg("--authrpc.port")
        .arg(args.engine_port.to_string())
        .arg("--authrpc.jwtsecret")
        .arg(jwt_path)
        .arg("--metrics")
        .arg(format!("127.0.0.1:{}", args.metrics_port))
        // Disable discovery for local devnet
        .arg("--disable-discovery")
        .arg("--nat")
        .arg("none")
        // P2P port
        .arg("--port")
        .arg(args.p2p_port.to_string())
        // Enable dev mode for auto-mining etc
        .arg("--dev");

    if args.flashblocks {
        cmd.arg("--enable-metering");
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        eyre!(
            "Failed to start base-reth-node. Is it in your PATH? Error: {e}\n\
             Try running: cargo build --release -p base-reth-node"
        )
    })?;

    // Spawn thread to forward stdout
    if let Some(stdout) = child.stdout.take() {
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                info!(target: "client", "{line}");
            }
        });
    }

    // Spawn thread to forward stderr
    if let Some(stderr) = child.stderr.take() {
        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                warn!(target: "client", "{line}");
            }
        });
    }

    Ok(child)
}

fn launch_builder(args: &DevnetArgs, genesis_path: &PathBuf, jwt_path: &PathBuf) -> Result<Child> {
    let mut cmd = Command::new(find_binary("base-builder"));

    // Reduce log noise - filter out repetitive forkchoice/payload messages
    cmd.env("RUST_LOG", "info,reth_engine_util::engine_store=warn,reth::consensus::engine=warn");

    cmd.arg("node")
        .arg("--chain")
        .arg(genesis_path)
        .arg("--datadir")
        .arg(args.builder_datadir())
        .arg("--http")
        .arg("--http.addr")
        .arg("0.0.0.0")
        .arg("--http.port")
        .arg(args.builder_http_port.to_string())
        .arg("--http.api")
        .arg("eth,net,web3,debug,txpool")
        .arg("--authrpc.addr")
        .arg("127.0.0.1")
        .arg("--authrpc.port")
        .arg(args.builder_engine_port.to_string())
        .arg("--authrpc.jwtsecret")
        .arg(jwt_path)
        .arg("--metrics")
        .arg(format!("127.0.0.1:{}", args.builder_metrics_port))
        // Disable discovery for local devnet
        .arg("--disable-discovery")
        .arg("--nat")
        .arg("none")
        // P2P port
        .arg("--port")
        .arg(args.builder_p2p_port.to_string())
        // Builder-specific args
        .arg("--rollup.chain-block-time")
        .arg(args.block_time_ms.to_string());

    // Note: flashblocks is always enabled on builder

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        eyre!(
            "Failed to start base-builder. Is it in your PATH? Error: {e}\n\
             Try running: cargo build --release -p base-builder"
        )
    })?;

    // Spawn thread to forward stdout
    if let Some(stdout) = child.stdout.take() {
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                info!(target: "builder", "{line}");
            }
        });
    }

    // Spawn thread to forward stderr
    if let Some(stderr) = child.stderr.take() {
        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                warn!(target: "builder", "{line}");
            }
        });
    }

    Ok(child)
}

/// Launch client with TUI log capture.
#[cfg(feature = "tui")]
fn launch_client_tui(
    args: &DevnetArgs,
    genesis_path: &PathBuf,
    jwt_path: &PathBuf,
    log_state: Arc<Mutex<LogState>>,
) -> Result<Child> {
    let mut cmd = Command::new(find_binary("base-reth-node"));

    // Reduce log noise - filter out repetitive forkchoice/payload messages
    cmd.env("RUST_LOG", "info,reth_engine_util::engine_store=warn,reth::consensus::engine=warn");

    cmd.arg("node")
        .arg("--chain")
        .arg(genesis_path)
        .arg("--datadir")
        .arg(args.client_datadir())
        .arg("--http")
        .arg("--http.addr")
        .arg("0.0.0.0")
        .arg("--http.port")
        .arg(args.http_port.to_string())
        .arg("--http.api")
        .arg("eth,net,web3,debug,trace,txpool")
        .arg("--ws")
        .arg("--ws.addr")
        .arg("0.0.0.0")
        .arg("--ws.port")
        .arg(args.ws_port.to_string())
        .arg("--ws.api")
        .arg("eth,net,web3,debug,trace,txpool")
        .arg("--authrpc.addr")
        .arg("127.0.0.1")
        .arg("--authrpc.port")
        .arg(args.engine_port.to_string())
        .arg("--authrpc.jwtsecret")
        .arg(jwt_path)
        .arg("--metrics")
        .arg(format!("127.0.0.1:{}", args.metrics_port))
        .arg("--disable-discovery")
        .arg("--nat")
        .arg("none")
        .arg("--port")
        .arg(args.p2p_port.to_string())
        .arg("--dev");

    if args.flashblocks {
        cmd.arg("--enable-metering");
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        eyre!(
            "Failed to start base-reth-node. Is it in your PATH? Error: {e}\n\
             Try running: cargo build --release -p base-reth-node"
        )
    })?;

    // Capture stdout to TUI
    if let Some(stdout) = child.stdout.take() {
        let state = Arc::clone(&log_state);
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                state.lock().unwrap().push_client_log(line);
            }
        });
    }

    // Capture stderr to TUI
    if let Some(stderr) = child.stderr.take() {
        let state = Arc::clone(&log_state);
        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                state.lock().unwrap().push_client_log(format!("[ERR] {line}"));
            }
        });
    }

    Ok(child)
}

/// Launch builder with TUI log capture.
#[cfg(feature = "tui")]
fn launch_builder_tui(
    args: &DevnetArgs,
    genesis_path: &PathBuf,
    jwt_path: &PathBuf,
    log_state: Arc<Mutex<LogState>>,
) -> Result<Child> {
    let mut cmd = Command::new(find_binary("base-builder"));

    // Reduce log noise - filter out repetitive forkchoice/payload messages
    cmd.env("RUST_LOG", "info,reth_engine_util::engine_store=warn,reth::consensus::engine=warn");

    cmd.arg("node")
        .arg("--chain")
        .arg(genesis_path)
        .arg("--datadir")
        .arg(args.builder_datadir())
        .arg("--http")
        .arg("--http.addr")
        .arg("0.0.0.0")
        .arg("--http.port")
        .arg(args.builder_http_port.to_string())
        .arg("--http.api")
        .arg("eth,net,web3,debug,txpool")
        .arg("--authrpc.addr")
        .arg("127.0.0.1")
        .arg("--authrpc.port")
        .arg(args.builder_engine_port.to_string())
        .arg("--authrpc.jwtsecret")
        .arg(jwt_path)
        .arg("--metrics")
        .arg(format!("127.0.0.1:{}", args.builder_metrics_port))
        .arg("--disable-discovery")
        .arg("--nat")
        .arg("none")
        .arg("--port")
        .arg(args.builder_p2p_port.to_string())
        .arg("--rollup.chain-block-time")
        .arg(args.block_time_ms.to_string());

    // Note: flashblocks is always enabled on builder

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        eyre!(
            "Failed to start base-builder. Is it in your PATH? Error: {e}\n\
             Try running: cargo build --release -p base-builder"
        )
    })?;

    // Capture stdout to TUI
    if let Some(stdout) = child.stdout.take() {
        let state = Arc::clone(&log_state);
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                state.lock().unwrap().push_builder_log(line);
            }
        });
    }

    // Capture stderr to TUI
    if let Some(stderr) = child.stderr.take() {
        let state = Arc::clone(&log_state);
        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                state.lock().unwrap().push_builder_log(format!("[ERR] {line}"));
            }
        });
    }

    Ok(child)
}

fn print_connection_info(args: &DevnetArgs) {
    info!("═══════════════════════════════════════════════════════════════");
    info!("                    Base Devnet Running                        ");
    info!("═══════════════════════════════════════════════════════════════");
    info!("");
    info!("  Chain ID:       {DEVNET_CHAIN_ID}");
    info!("  Block Time:     {}ms", args.block_time_ms);
    info!("");

    if !args.no_client {
        info!("  Client RPC:");
        info!("    HTTP:         http://127.0.0.1:{}", args.http_port);
        info!("    WebSocket:    ws://127.0.0.1:{}", args.ws_port);
        info!("    Engine API:   http://127.0.0.1:{}", args.engine_port);
        info!("    Metrics:      http://127.0.0.1:{}/metrics", args.metrics_port);
        info!("");
    }

    if !args.no_builder {
        info!("  Builder RPC:");
        info!("    HTTP:         http://127.0.0.1:{}", args.builder_http_port);
        info!("    Engine API:   http://127.0.0.1:{}", args.builder_engine_port);
        info!("    Metrics:      http://127.0.0.1:{}/metrics", args.builder_metrics_port);
        info!("");
    }

    info!("  Test Accounts (1M ETH each):");
    info!("    Alice:   0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    info!("    Bob:     0x70997970C51812dc3A010C7d01b50e0d17dc79C8");
    info!("    Charlie: 0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC");
    info!("");
    info!("═══════════════════════════════════════════════════════════════");
}
