//! Checks TDX prover RPC egress to zeronet endpoints.

use std::{
    env,
    error::Error,
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    process::ExitCode,
    time::{Duration, Instant},
};

const RPC_URLS: &[&str] = &[
    "https://c3-chainproxy-eth-hoodi-full-dev.cbhq.net",
    "https://base-zeronet-reth-proofs-donotuse.cbhq.net:8545",
    "https://base-prover-service.aws-dev.cbhq.net:9090",
];
const BODY_LIMIT: usize = 2048;
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const TCP_TIMEOUT: Duration = Duration::from_secs(5);
const PROXY_ENV_VARS: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];

#[tokio::main]
async fn main() -> ExitCode {
    print_proxy_env();

    let http_clients = match http_clients() {
        Ok(http_clients) => http_clients,
        Err(err) => {
            eprintln!("failed to build HTTP clients: {err}");
            return ExitCode::FAILURE;
        }
    };

    let mut failed = false;
    for url in RPC_URLS {
        println!("checking {url}");
        if !check_rpc_url(&http_clients, url).await {
            failed = true;
        }
    }

    if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

async fn check_rpc_url(http_clients: &[(&'static str, reqwest::Client)], url: &str) -> bool {
    let Some((host, port)) = endpoint(url) else {
        return false;
    };

    let Some(addrs) = resolve(&host, port) else {
        return false;
    };
    let tcp_ok = tcp_check(&addrs);
    let mut http_ok = false;
    for (label, client) in http_clients {
        http_ok |= http_check(client, label, url).await;
    }
    if !http_ok {
        http_ok = direct_ip_http_checks(url, &host, &addrs).await;
    }

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

fn resolve(host: &str, port: u16) -> Option<Vec<SocketAddr>> {
    let addrs = match (host, port).to_socket_addrs() {
        Ok(addrs) => addrs.collect::<Vec<_>>(),
        Err(err) => {
            eprintln!("dns {host}:{port}: {err}");
            return None;
        }
    };
    if addrs.is_empty() {
        eprintln!("dns {host}:{port}: no addresses");
        return None;
    }

    println!("dns {host}:{port}: {addrs:?}");
    Some(addrs)
}

fn tcp_check(addrs: &[SocketAddr]) -> bool {
    let mut ok = false;
    for addr in addrs.iter().copied() {
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

async fn direct_ip_http_checks(url: &str, host: &str, addrs: &[SocketAddr]) -> bool {
    let mut ok = false;
    for addr in addrs.iter().copied() {
        let label = format!("http-direct-http1 {host}->{addr}");
        let client = match client_builder()
            .no_proxy()
            .http1_only()
            .resolve_to_addrs(host, &[addr])
            .build()
        {
            Ok(client) => client,
            Err(err) => {
                eprintln!("{label}: failed to build client: {err}");
                continue;
            }
        };
        ok |= http_check(&client, &label, url).await;
    }

    ok
}

async fn http_check(client: &reqwest::Client, label: &str, url: &str) -> bool {
    let started = Instant::now();
    let response = match client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(r#"{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}"#)
        .send()
        .await
    {
        Ok(response) => response,
        Err(err) => {
            eprintln!("{label} {url}: {err} after {:?}", started.elapsed());
            print_error_chain(&err);
            return false;
        }
    };

    let status = response.status();
    let version = response.version();
    match response.text().await {
        Ok(body) => println!(
            "{label} {url}: status={status} version={version:?} elapsed={:?} body={}",
            started.elapsed(),
            trim_body(&body)
        ),
        Err(err) => {
            eprintln!("{label} {url}: failed reading body: {err} after {:?}", started.elapsed());
            print_error_chain(&err);
            return false;
        }
    }

    status.is_success()
}

fn http_clients() -> Result<Vec<(&'static str, reqwest::Client)>, reqwest::Error> {
    Ok(vec![
        ("http-default", client_builder().build()?),
        ("http-no-proxy", client_builder().no_proxy().build()?),
        ("http-no-proxy-http1", client_builder().no_proxy().http1_only().build()?),
    ])
}

fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().timeout(HTTP_TIMEOUT).connect_timeout(TCP_TIMEOUT)
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

fn print_proxy_env() {
    for key in PROXY_ENV_VARS {
        match env::var(key) {
            Ok(value) => println!("env {key}={}", redact_proxy_value(&value)),
            Err(env::VarError::NotPresent) => println!("env {key}=<unset>"),
            Err(env::VarError::NotUnicode(_)) => println!("env {key}=<non-unicode>"),
        }
    }
}

fn redact_proxy_value(value: &str) -> String {
    let Some(scheme_end) = value.find("://") else {
        return value.to_string();
    };
    let authority_start = scheme_end + 3;
    let Some(at_offset) = value[authority_start..].find('@') else {
        return value.to_string();
    };
    let at = authority_start + at_offset;

    format!("{}<redacted>@{}", &value[..authority_start], &value[at + 1..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_uses_explicit_or_default_https_port() {
        assert_eq!(endpoint("https://example.com"), Some(("example.com".to_string(), 443)));
        assert_eq!(endpoint("https://example.com:8545"), Some(("example.com".to_string(), 8545)));
    }

    #[test]
    fn redacts_proxy_credentials() {
        assert_eq!(
            redact_proxy_value("http://user:pass@proxy.example:8080"),
            "http://<redacted>@proxy.example:8080"
        );
        assert_eq!(redact_proxy_value("http://proxy.example:8080"), "http://proxy.example:8080");
    }
}
