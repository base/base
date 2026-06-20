//! Checks TDX prover RPC egress to zeronet endpoints.

use std::{
    error::Error,
    net::{TcpStream, ToSocketAddrs},
    process::ExitCode,
    time::{Duration, Instant},
};

const RPC_URLS: &[&str] = &[
    "https://c3-chainproxy-eth-hoodi-full-dev.cbhq.net",
    "https://base-zeronet-reth-proofs-donotuse.cbhq.net:8545",
    "https://base-prover-service-dev.cbhq.net:9090",
];
const BODY_LIMIT: usize = 2048;
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const TCP_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> ExitCode {
    let client = match reqwest::Client::builder().timeout(HTTP_TIMEOUT).build() {
        Ok(client) => client,
        Err(err) => {
            eprintln!("failed to build HTTP client: {err}");
            return ExitCode::FAILURE;
        }
    };

    let mut failed = false;
    for url in RPC_URLS {
        println!("checking {url}");
        if !check_rpc_url(&client, url).await {
            failed = true;
        }
    }

    if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

async fn check_rpc_url(client: &reqwest::Client, url: &str) -> bool {
    let Some((host, port)) = endpoint(url) else {
        return false;
    };

    let tcp_ok = tcp_check(&host, port);
    let http_ok = http_check(client, url).await;

    tcp_ok && http_ok
}

fn endpoint(url: &str) -> Option<(String, u16)> {
    let parsed = match reqwest::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(err) => {
            eprintln!("{url}: invalid URL: {err}");
            return None;
        }
    };
    let Some(host) = parsed.host_str() else {
        eprintln!("{url}: missing host");
        return None;
    };
    let Some(port) = parsed.port_or_known_default() else {
        eprintln!("{url}: missing port");
        return None;
    };

    Some((host.to_string(), port))
}

fn tcp_check(host: &str, port: u16) -> bool {
    let addrs = match (host, port).to_socket_addrs() {
        Ok(addrs) => addrs.collect::<Vec<_>>(),
        Err(err) => {
            eprintln!("dns {host}:{port}: {err}");
            return false;
        }
    };
    if addrs.is_empty() {
        eprintln!("dns {host}:{port}: no addresses");
        return false;
    }

    println!("dns {host}:{port}: {addrs:?}");
    let mut ok = false;
    for addr in addrs {
        let started = Instant::now();
        match TcpStream::connect_timeout(&addr, TCP_TIMEOUT) {
            Ok(_) => {
                ok = true;
                println!("tcp {addr}: ok in {:?}", started.elapsed());
            }
            Err(err) => eprintln!("tcp {addr}: {err} after {:?}", started.elapsed()),
        }
    }

    ok
}

async fn http_check(client: &reqwest::Client, url: &str) -> bool {
    let response = match client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(r#"{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}"#)
        .send()
        .await
    {
        Ok(response) => response,
        Err(err) => {
            eprintln!("http {url}: {err}");
            print_error_chain(&err);
            return false;
        }
    };

    let status = response.status();
    match response.text().await {
        Ok(body) => println!("http {url}: status={status} body={}", trim_body(&body)),
        Err(err) => {
            eprintln!("http {url}: failed reading body: {err}");
            print_error_chain(&err);
            return false;
        }
    }

    status.is_success()
}

fn trim_body(body: &str) -> String {
    let trimmed = body.trim();
    let mut out: String = trimmed.chars().take(BODY_LIMIT).collect();
    if out.len() < trimmed.len() {
        out.push_str("...");
    }
    out
}

fn print_error_chain(err: &reqwest::Error) {
    eprintln!(
        "http error flags: timeout={} connect={} request={} status={:?}",
        err.is_timeout(),
        err.is_connect(),
        err.is_request(),
        err.status()
    );

    let mut source = err.source();
    while let Some(err) = source {
        eprintln!("caused by: {err}");
        source = err.source();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_uses_explicit_or_default_https_port() {
        assert_eq!(endpoint("https://example.com"), Some(("example.com".to_string(), 443)));
        assert_eq!(endpoint("https://example.com:8545"), Some(("example.com".to_string(), 8545)));
    }
}
