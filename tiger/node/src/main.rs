//! The tiger client binary.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
};

use base_tiger::{Genesis, JwtSecret, Node};
use base_tiger_net::{BASE_BOOTNODES, ForkSchedule, NetworkConfig, NodeRecord};
use secp256k1::SecretKey;

const USAGE: &str = "usage:
  tiger node --chain <base|base-sepolia|genesis.json> --datadir <dir>
             [--http] [--http.addr <ip>] [--http.port <port>]
             [--authrpc.addr <ip>] [--authrpc.port <port>] [--authrpc.jwtsecret <file>]
             [--http.api <modules>] [--rpc.eth-proof-window <blocks>] [--rollup.sequencer-http <url>]
             [--stateless.upstream <http-url>] [--stateless.start <block|tag>]
             [--engine.persistence-threshold <blocks>] [--engine.memory-block-buffer-target <blocks>]
             [--engine.persistence-backpressure-threshold <blocks>]
             [--port <port>] [--discovery.port <port>] [--disable-discovery]
             [--bootnodes <enode,...>] [--trusted-peers <enode,...>] [--p2p-secret-key <file>]
             [--max-outbound-peers <n>] [--max-inbound-peers <n>] [--nat extip:<ip>]
  tiger diff-adapter
  tiger bench fetch|replay (see node/src/bench.rs)";

/// `tiger node` options, named as base-reth names them.
#[derive(Debug)]
struct NodeArgs {
    chain: String,
    datadir: PathBuf,
    http: SocketAddr,
    auth: SocketAddr,
    jwt_secret: Option<PathBuf>,
    rpc: base_tiger::RpcConfig,
    net: NetArgs,
    /// `--stateless.upstream` and `--stateless.start`.
    stateless: Option<(base_tiger::Upstream, String)>,
    /// `--engine.persistence-threshold`, `--engine.memory-block-buffer-target` and
    /// `--engine.persistence-backpressure-threshold`.
    persistence: (u64, u64, Option<u64>),
}

/// Peer-to-peer options, named as base-reth names them.
#[derive(Debug)]
struct NetArgs {
    port: u16,
    discovery_port: Option<u16>,
    disable_discovery: bool,
    bootnodes: Option<Vec<String>>,
    trusted_peers: Vec<String>,
    secret_key: Option<PathBuf>,
    max_outbound: Option<usize>,
    max_inbound: Option<usize>,
    external_ip: Option<IpAddr>,
}

impl NodeArgs {
    /// base-reth flags tiger accepts and ignores: what they configure does not exist in tiger
    /// (metrics, logging, pruning, the transaction pool, peering it does not do yet).
    const IGNORED_WITH_VALUE: &'static [&'static str] = &[
        "--http.corsdomain",
        "--ws.api",
        "--metrics",
        "--log.file.directory",
        "--rpc.gascap",
        "--txpool.max-account-slots",
        "--verbosity",
        "--log.stdout.format",
    ];
    const IGNORED_FLAGS: &'static [&'static str] = &
        ["--ws", "--rollup.disable-tx-pool-gossip", "-v", "--http.disable-compression"];

    fn parse(args: &[String]) -> Result<Self, String> {
        let localhost = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let (mut chain, mut datadir, mut jwt_secret) = (None, None, None);
        let (mut upstream, mut start) = (None, "finalized".to_owned());
        let mut rpc = base_tiger::RpcConfig::default();
        let mut persistence = (base_tiger::PERSISTENCE_THRESHOLD, base_tiger::MEMORY_BLOCK_BUFFER_TARGET, None);
        let (mut http_addr, mut http_port) = (localhost, 8545u16);
        let (mut auth_addr, mut auth_port) = (localhost, 8551u16);
        let mut net = NetArgs {
            port: 30303,
            discovery_port: None,
            disable_discovery: false,
            bootnodes: None,
            trusted_peers: Vec::new(),
            secret_key: None,
            max_outbound: None,
            max_inbound: None,
            external_ip: None,
        };
        let list = |text: String| -> Vec<String> {
            text.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
        };
        let mut index = 0;
        while index < args.len() {
            let (flag, inline) = match args[index].split_once('=') {
                Some((flag, value)) => (flag.to_owned(), Some(value.to_owned())),
                None => (args[index].clone(), None),
            };
            let mut value = || -> Result<String, String> {
                if let Some(value) = inline.clone() {
                    return Ok(value);
                }
                index += 1;
                args.get(index).cloned().ok_or_else(|| format!("{flag} needs a value"))
            };
            let parse_ip = |text: String| text.parse::<IpAddr>().map_err(|error| format!("{text}: {error}"));
            let parse_port = |text: String| text.parse::<u16>().map_err(|error| format!("{text}: {error}"));
            match flag.as_str() {
                "--chain" => chain = Some(value()?),
                "--datadir" => datadir = Some(PathBuf::from(value()?)),
                "--http" => {}
                "--http.addr" => http_addr = parse_ip(value()?)?,
                "--http.port" => http_port = parse_port(value()?)?,
                "--authrpc.addr" => auth_addr = parse_ip(value()?)?,
                "--authrpc.port" => auth_port = parse_port(value()?)?,
                "--authrpc.jwtsecret" => jwt_secret = Some(PathBuf::from(value()?)),
                "--http.api" => rpc = rpc.with_http_api(&value()?),
                "--rollup.sequencer" | "--rollup.sequencer-http" | "--rollup.sequencer-ws" => {
                    rpc.sequencer = Some(value()?);
                }
                "--rpc.eth-proof-window" => {
                    let text = value()?;
                    let blocks = text.parse::<u64>().map_err(|error| format!("--rpc.eth-proof-window {text}: {error}"))?;
                    rpc = rpc.with_eth_proof_window(blocks)?;
                }
                "--engine.persistence-threshold" => {
                    persistence.0 = value()?.parse().map_err(|error| format!("{flag}: {error}"))?;
                }
                "--engine.memory-block-buffer-target" => {
                    persistence.1 = value()?.parse().map_err(|error| format!("{flag}: {error}"))?;
                }
                "--engine.persistence-backpressure-threshold" => {
                    persistence.2 = Some(value()?.parse().map_err(|error| format!("{flag}: {error}"))?);
                }
                "--stateless.upstream" => upstream = Some(base_tiger::Upstream::new(&value()?)?),
                "--stateless.start" => start = value()?,
                "--port" => net.port = parse_port(value()?)?,
                "--discovery.port" | "--discovery.v5.port" => net.discovery_port = Some(parse_port(value()?)?),
                "--disable-discovery" => net.disable_discovery = true,
                "--bootnodes" => net.bootnodes = Some(list(value()?)),
                "--trusted-peers" => net.trusted_peers = list(value()?),
                "--p2p-secret-key" => net.secret_key = Some(PathBuf::from(value()?)),
                "--max-outbound-peers" => {
                    net.max_outbound = Some(value()?.parse().map_err(|error| format!("{flag}: {error}"))?)
                }
                "--max-inbound-peers" => {
                    net.max_inbound = Some(value()?.parse().map_err(|error| format!("{flag}: {error}"))?)
                }
                "--nat" => {
                    let text = value()?;
                    let ip = text.strip_prefix("extip:").ok_or_else(|| format!("--nat {text}: only extip:<ip> is supported"))?;
                    net.external_ip = Some(parse_ip(ip.to_owned())?);
                }
                flag if Self::IGNORED_FLAGS.contains(&flag) => {
                    eprintln!("tiger: ignoring {flag}");
                }
                flag if Self::IGNORED_WITH_VALUE.contains(&flag) => {
                    let ignored = value()?;
                    eprintln!("tiger: ignoring {flag} {ignored}");
                }
                flag => return Err(format!("unknown flag {flag}")),
            }
            index += 1;
        }
        Ok(Self {
            chain: chain.ok_or("--chain is required")?,
            datadir: datadir.ok_or("--datadir is required")?,
            http: SocketAddr::new(http_addr, http_port),
            auth: SocketAddr::new(auth_addr, auth_port),
            jwt_secret,
            rpc,
            net,
            stateless: upstream.map(|upstream| (upstream, start)),
            persistence,
        })
    }

    fn run(self) -> Result<(), String> {
        let genesis = Genesis::load(&self.chain)?;
        eprintln!(
            "tiger: chain {} genesis {} {}",
            genesis.chain.chain_id, genesis.header.number, genesis.hash
        );
        let jwt_path = self.jwt_secret.unwrap_or_else(|| self.datadir.join("jwt.hex"));
        let jwt = JwtSecret::load_or_create(&jwt_path)?;
        let mut node = Node::open(&self.datadir, genesis, self.stateless)?;
        node.rpc_config = self.rpc;
        node.chain.get_mut().expect("chain lock").set_persistence(self.persistence.0, self.persistence.1, self.persistence.2)?;
        let node = Arc::new(node);
        {
            let chain = node.chain.read().expect("chain lock");
            let head = chain.head();
            eprintln!("tiger: head {} {}", head.number(), head.hash);
        }
        let secret_path = self.net.secret_key.clone().unwrap_or_else(|| self.datadir.join("discovery-secret"));
        let config = self.net.config(&node, &self.chain, &secret_path)?;
        eprintln!("tiger: p2p on port {}", self.net.port);
        node.start_sync(config)?;
        node.start_prewarm();
        node.start_persistence()?;
        base_tiger::serve(Arc::clone(&node), self.auth, Some(jwt))?;
        base_tiger::serve(node, self.http, None)?;
        eprintln!("tiger: serving http on {} and the Engine API on {}", self.http, self.auth);
        loop {
            std::thread::park();
        }
    }
}

impl NetArgs {
    fn config(&self, node: &Node, chain_arg: &str, secret_path: &std::path::Path) -> Result<NetworkConfig, String> {
        let key = Self::secret_key(secret_path)?;
        let spec = &node.spec;
        let upgrades = &spec.upgrades;
        let timestamps: Vec<u64> = [
            upgrades.regolith_time,
            upgrades.canyon_time,
            upgrades.ecotone_time,
            upgrades.fjord_time,
            upgrades.granite_time,
            upgrades.holocene_time,
            upgrades.isthmus_time,
            upgrades.jovian_time,
            upgrades.base.azul,
            upgrades.base.beryl,
            upgrades.base.cobalt,
            upgrades.base.denim,
            upgrades.base.everest,
        ]
        .into_iter()
        .flatten()
        .collect();
        let genesis = node.chain.read().expect("chain lock").genesis.clone();
        let forks = ForkSchedule::base(genesis.hash, genesis.header.timestamp, 0, &timestamps);
        let mut config = NetworkConfig::new(key, spec.chain_id, forks);
        let any = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        config.listen_addr = Some(SocketAddr::new(any, self.port));
        config.discovery_addr = (!self.disable_discovery)
            .then(|| SocketAddr::new(any, self.discovery_port.unwrap_or(self.port)));
        let built_in = matches!(chain_arg, "base" | "base-mainnet" | "base-sepolia");
        let bootnodes = match &self.bootnodes {
            Some(list) => list.clone(),
            None if built_in => BASE_BOOTNODES.iter().map(|node| (*node).to_owned()).collect(),
            None => Vec::new(),
        };
        let parse = |text: &String| text.parse::<NodeRecord>().map_err(|error| format!("{text}: {error}"));
        config.bootnodes = bootnodes.iter().map(parse).collect::<Result<_, _>>()?;
        config.trusted_peers = self.trusted_peers.iter().map(parse).collect::<Result<_, _>>()?;
        if let Some(max) = self.max_outbound {
            config.max_outbound = max;
        }
        if let Some(max) = self.max_inbound {
            config.max_inbound = max;
        }
        config.external_ip = self.external_ip;
        config.client_id = format!("tiger/v{}", env!("CARGO_PKG_VERSION"));
        Ok(config)
    }

    /// The node key in `path` (hex), created at random when missing, as base-reth's
    /// `discovery-secret`.
    fn secret_key(path: &std::path::Path) -> Result<SecretKey, String> {
        let bytes = match std::fs::read_to_string(path) {
            Ok(text) => alloy_primitives_hex_decode(text.trim())?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut bytes = [0u8; 32];
                std::io::Read::read_exact(
                    &mut std::fs::File::open("/dev/urandom").map_err(|e| e.to_string())?,
                    &mut bytes,
                )
                .map_err(|e| e.to_string())?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                let text: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                std::fs::write(path, text).map_err(|e| format!("writing {}: {e}", path.display()))?;
                bytes
            }
            Err(error) => return Err(format!("reading {}: {error}", path.display())),
        };
        SecretKey::from_byte_array(bytes).map_err(|error| format!("node key {}: {error}", path.display()))
    }
}

fn alloy_primitives_hex_decode(text: &str) -> Result<[u8; 32], String> {
    let text = text.strip_prefix("0x").unwrap_or(text);
    if text.len() != 64 {
        return Err("node key must be 32 bytes of hex".into());
    }
    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * index..2 * index + 2], 16).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

fn main() -> ExitCode {
    base_tiger_evm::install_ecrecover_cache();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("diff-adapter") => base_tiger::Adapter::run().map_err(|error| error.to_string()),
        Some("node") => NodeArgs::parse(&args[1..]).and_then(NodeArgs::run),
        Some("bench") => base_tiger::bench(&args[1..]),
        _ => Err(USAGE.to_owned()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("tiger: {error}");
            ExitCode::FAILURE
        }
    }
}
