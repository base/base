//! The upstream base-reth JSON-RPC endpoint a stateless node reads what it does not hold: the
//! start block, and an execution witness for each block whose state is not all local.
//!
//! The client speaks plain HTTP/1.1 (one request per connection) and nothing else: the upstream
//! is an operator's own node, and every answer is verified (witness nodes by hash against tiger's
//! own state roots, blocks by hash), so the channel adds no trust.

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};

use alloy_primitives::{B256, Bytes};
use serde::Deserialize;
use serde_json::{Value, json};

/// How long one upstream request may take, connect to last byte.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The largest upstream response read (a witness of a full mainnet block is a few megabytes).
const MAX_RESPONSE_BYTES: usize = 256 * 1024 * 1024;

/// An execution witness as base-reth's `debug_executionWitness` returns it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ExecutionWitness {
    /// Trie nodes (RLP) the block reads and its root computation needs.
    pub state: Vec<Bytes>,
    /// Bytecodes the block runs.
    pub codes: Vec<Bytes>,
    /// Ancestor headers (RLP) back to the oldest block hash the block reads.
    #[serde(default)]
    pub headers: Vec<Bytes>,
}

/// An upstream JSON-RPC endpoint (`http://host:port/path`).
#[derive(Debug, Clone)]
pub struct Upstream {
    host: String,
    port: u16,
    path: String,
}

impl Upstream {
    /// Parses `url`, which must be `http://host[:port][/path]`.
    pub fn new(url: &str) -> Result<Self, String> {
        let rest = url.strip_prefix("http://").ok_or_else(|| format!("upstream {url}: only http:// is supported"))?;
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => {
                (host, port.parse::<u16>().map_err(|error| format!("upstream {url}: port: {error}"))?)
            }
            None => (authority, 80),
        };
        if host.is_empty() {
            return Err(format!("upstream {url}: no host"));
        }
        Ok(Self { host: host.to_owned(), port, path: path.to_owned() })
    }

    /// Calls `method` with `params` and returns its result.
    pub fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string();
        let response = self.post(&body).map_err(|error| format!("upstream {method}: {error}"))?;
        let mut reply: Value =
            serde_json::from_slice(&response).map_err(|error| format!("upstream {method}: reply: {error}"))?;
        if let Some(error) = reply.get("error") {
            return Err(format!("upstream {method}: {error}"));
        }
        Ok(reply["result"].take())
    }

    /// The execution witness of block `hash` (number `number`): by hash when the upstream
    /// serves `debug_executionWitnessByBlockHash`, else of its canonical block at `number`, which
    /// may be another block (the witness then fails verification against tiger's roots).
    pub fn execution_witness(&self, hash: B256, number: u64) -> Result<ExecutionWitness, String> {
        let result = match self.call("debug_executionWitnessByBlockHash", json!([hash])) {
            Ok(result) => result,
            Err(_) => self.call("debug_executionWitness", json!([format!("{number:#x}")]))?,
        };
        serde_json::from_value(result).map_err(|error| format!("upstream debug_executionWitness: {error}"))
    }

    /// The RLP of canonical block `block` (a number or tag) and its receipts (EIP-2718).
    pub fn raw_block(&self, block: &str) -> Result<(Bytes, Vec<Bytes>), String> {
        let block_rlp: Bytes = serde_json::from_value(self.call("debug_getRawBlock", json!([block]))?)
            .map_err(|error| format!("upstream debug_getRawBlock: {error}"))?;
        let receipts: Vec<Bytes> = serde_json::from_value(self.call("debug_getRawReceipts", json!([block]))?)
            .map_err(|error| format!("upstream debug_getRawReceipts: {error}"))?;
        Ok((block_rlp, receipts))
    }

    fn post(&self, body: &str) -> std::io::Result<Vec<u8>> {
        let address = (self.host.as_str(), self.port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| std::io::Error::other("host does not resolve"))?;
        let mut stream = TcpStream::connect_timeout(&address, REQUEST_TIMEOUT)?;
        stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
        stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
        write!(
            stream,
            "POST {} HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.path,
            self.host,
            self.port,
            body.len()
        )?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let status = line.split_whitespace().nth(1).unwrap_or_default().to_owned();
        if status != "200" {
            return Err(std::io::Error::other(format!("HTTP status {}", line.trim())));
        }
        let (mut length, mut chunked) = (None, false);
        loop {
            line.clear();
            reader.read_line(&mut line)?;
            let header = line.trim_end();
            if header.is_empty() {
                break;
            }
            let Some((name, value)) = header.split_once(':') else { continue };
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                length = Some(value.parse::<usize>().map_err(std::io::Error::other)?);
            } else if name.eq_ignore_ascii_case("transfer-encoding") && value.eq_ignore_ascii_case("chunked") {
                chunked = true;
            }
        }
        let mut body = Vec::new();
        if chunked {
            loop {
                line.clear();
                reader.read_line(&mut line)?;
                let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or("0"), 16)
                    .map_err(std::io::Error::other)?;
                if size == 0 {
                    break;
                }
                if body.len() + size > MAX_RESPONSE_BYTES {
                    return Err(std::io::Error::other("response too large"));
                }
                let start = body.len();
                body.resize(start + size, 0);
                reader.read_exact(&mut body[start..])?;
                let mut crlf = [0u8; 2];
                reader.read_exact(&mut crlf)?;
            }
        } else if let Some(length) = length {
            if length > MAX_RESPONSE_BYTES {
                return Err(std::io::Error::other("response too large"));
            }
            body.resize(length, 0);
            reader.read_exact(&mut body)?;
        } else {
            reader.take(MAX_RESPONSE_BYTES as u64).read_to_end(&mut body)?;
        }
        Ok(body)
    }
}
