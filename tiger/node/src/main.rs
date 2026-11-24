//! The tiger client binary.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
};

use base_tiger::{Genesis, JwtSecret, Node};

const USAGE: &str = "usage:
  tiger node --chain <base|base-sepolia|genesis.json> --datadir <dir>
             [--http] [--http.addr <ip>] [--http.port <port>]
             [--authrpc.addr <ip>] [--authrpc.port <port>] [--authrpc.jwtsecret <file>]
  tiger diff-adapter";

/// `tiger node` options, named as base-reth names them.
#[derive(Debug)]
struct NodeArgs {
    chain: String,
    datadir: PathBuf,
    http: SocketAddr,
    auth: SocketAddr,
    jwt_secret: Option<PathBuf>,
}

impl NodeArgs {
    /// base-reth flags tiger accepts and ignores: what they configure does not exist in tiger
    /// (metrics, logging, pruning, the transaction pool, peering it does not do yet).
    const IGNORED_WITH_VALUE: [&'static str; 12] = [
        "--http.api",
        "--http.corsdomain",
        "--ws.api",
        "--metrics",
        "--log.file.directory",
        "--rpc.gascap",
        "--txpool.max-account-slots",
        "--engine.persistence-threshold",
        "--engine.memory-block-buffer-target",
        "--port",
        "--discovery.port",
        "--verbosity",
    ];
    const IGNORED_FLAGS: [&'static str; 5] =
        ["--ws", "--disable-discovery", "--rollup.disable-tx-pool-gossip", "-v", "--http.disable-compression"];

    fn parse(args: &[String]) -> Result<Self, String> {
        let localhost = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let (mut chain, mut datadir, mut jwt_secret) = (None, None, None);
        let (mut http_addr, mut http_port) = (localhost, 8545u16);
        let (mut auth_addr, mut auth_port) = (localhost, 8551u16);
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
        let node = Arc::new(Node::open(&self.datadir, genesis)?);
        {
            let chain = node.chain.read().expect("chain lock");
            let head = chain.head();
            eprintln!("tiger: head {} {}", head.number(), head.hash);
        }
        base_tiger::serve(Arc::clone(&node), self.auth, Some(jwt))?;
        base_tiger::serve(node, self.http, None)?;
        eprintln!("tiger: serving http on {} and the Engine API on {}", self.http, self.auth);
        loop {
            std::thread::park();
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("diff-adapter") => base_tiger::Adapter::run().map_err(|error| error.to_string()),
        Some("node") => NodeArgs::parse(&args[1..]).and_then(NodeArgs::run),
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
