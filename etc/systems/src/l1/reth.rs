use std::time::Duration;

use eyre::{Result, WrapErr, ensure, eyre};
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{CmdWaitFor, ContainerRequest, ExecCommand, IntoContainerPort, Mount, WaitFor},
    runners::AsyncRunner,
};
use url::Url;

use super::config::L1ContainerConfig;
use crate::{
    containers::L1_RETH_NAME,
    images::RETH_IMAGE,
    network::{ensure_network_exists, ensure_network_exists_with_name, network_name},
    unique_name,
};

const HTTP_PORT: u16 = 8545;
const ENGINE_PORT: u16 = 8551;
const GENESIS_PATH: &str = "/genesis/el/genesis.json";
const JWT_PATH: &str = "/genesis/jwt.hex";
const RETH_SUPERVISOR: &str = r#"#!/bin/sh
set -eu

rm -f /tmp/reth-paused /tmp/reth-restart
child=""
forward_signal() {
    if [ -n "$child" ]; then
        kill -TERM "$child" 2>/dev/null || true
    fi
}
trap forward_signal TERM INT

while true; do
    /usr/local/bin/reth "$@" &
    child="$!"
    echo "$child" > /tmp/reth-child.pid
    wait "$child" || status="$?"
    touch /tmp/reth-paused

    while [ -f /tmp/reth-pause ]; do
        sleep 0.05
    done

    if [ -f /tmp/reth-restart ]; then
        rm -f /tmp/reth-restart /tmp/reth-paused
        unset status
        continue
    fi
    exit "${status:-0}"
done
"#;

#[derive(Debug)]
/// A container running the Reth execution layer.
pub struct RethContainer {
    container: ContainerAsync<GenericImage>,
    name: String,
    reorg_control_enabled: bool,
}

impl RethContainer {
    /// Starts a new Reth container with the given genesis and JWT secret.
    pub async fn start(
        genesis_json: impl AsRef<[u8]>,
        jwt_secret_hex: impl AsRef<[u8]>,
        config: Option<L1ContainerConfig>,
    ) -> Result<Self> {
        let config = config.unwrap_or_default();

        if let Some(ref net) = config.network_name {
            ensure_network_exists_with_name(net)?;
        } else {
            ensure_network_exists()?;
        }

        let (image_name, image_tag) =
            RETH_IMAGE.split_once(':').ok_or_else(|| eyre!("Reth image tag is missing"))?;

        let image = GenericImage::new(image_name, image_tag)
            .with_exposed_port(HTTP_PORT.tcp())
            .with_exposed_port(ENGINE_PORT.tcp())
            .with_wait_for(WaitFor::message_on_stdout("RPC HTTP server started"));
        let (image, command) = if config.enable_reorg_control {
            let image = image
                .with_entrypoint("sh")
                .with_copy_to("/reth-supervisor.sh", RETH_SUPERVISOR.as_bytes().to_vec());
            let command: Vec<String> = std::iter::once("/reth-supervisor.sh".to_string())
                .chain(reth_args().into_iter().map(str::to_string))
                .collect();
            (image, command)
        } else {
            let image: ContainerRequest<_> = image.with_entrypoint("reth").into();
            let command: Vec<String> = reth_args().into_iter().map(str::to_string).collect();
            (image, command)
        };

        let name = if config.use_stable_names {
            L1_RETH_NAME.to_string()
        } else {
            unique_name(L1_RETH_NAME)
        };
        let network = config.network_name.unwrap_or_else(|| network_name().to_string());

        let mut container_builder = image
            .with_container_name(&name)
            .with_network(&network)
            .with_cmd(command)
            .with_copy_to(GENESIS_PATH, genesis_json.as_ref().to_vec())
            .with_copy_to(JWT_PATH, jwt_secret_hex.as_ref().to_vec());

        if config.tmpfs_datadir {
            // reth's mdbx database needs a writable MAP_SHARED mmap, which the container's overlayfs
            // upper layer rejects on some hosts (e.g. docker-in-docker CI) with "Remote I/O error
            // (121)", crashing the node at startup. Back the datadir with tmpfs, which supports it.
            container_builder = container_builder.with_mount(Mount::tmpfs_mount("/data"));
        }

        if let Some(port) = config.http_port {
            container_builder = container_builder.with_mapped_port(port, HTTP_PORT.tcp());
        }
        if let Some(port) = config.engine_port {
            container_builder = container_builder.with_mapped_port(port, ENGINE_PORT.tcp());
        }

        let container =
            container_builder.start().await.wrap_err("Failed to start Reth container")?;

        Ok(Self { container, name, reorg_control_enabled: config.enable_reorg_control })
    }

    /// Returns the public RPC URL of the container.
    pub async fn rpc_url(&self) -> Result<Url> {
        self.host_url(HTTP_PORT).await
    }

    /// Returns the public Engine API URL of the container.
    pub async fn engine_url(&self) -> Result<Url> {
        self.host_url(ENGINE_PORT).await
    }

    /// Returns the internal RPC URL of the container within the Docker network.
    pub fn internal_rpc_url(&self) -> String {
        format!("http://{}:{}", self.name, HTTP_PORT)
    }

    /// Returns the internal Engine API URL of the container within the Docker network.
    pub fn internal_engine_url(&self) -> String {
        format!("http://{}:{}", self.name, ENGINE_PORT)
    }

    async fn host_url(&self, container_port: u16) -> Result<Url> {
        let host = self.container.get_host().await.wrap_err("Failed to resolve container host")?;
        let host_port = self
            .container
            .get_host_port_ipv4(container_port)
            .await
            .wrap_err("Failed to resolve container port")?;
        Url::parse(&format!("http://{host}:{host_port}")).wrap_err("Failed to build container URL")
    }

    /// Stops the supervised Reth node, unwinds its database to `block_number`, and restarts it.
    pub async fn unwind_to(&self, block_number: u64) -> Result<()> {
        ensure!(self.reorg_control_enabled, "Reth reorg control was not enabled for this stack");

        let control_script = format!(
            "touch /tmp/reth-pause; \
             kill -TERM \"$(cat /tmp/reth-child.pid)\" 2>/dev/null || true; \
             attempts=0; \
             while [ ! -f /tmp/reth-paused ] && [ \"$attempts\" -lt 200 ]; do \
                 attempts=$((attempts + 1)); sleep 0.05; \
             done; \
             if [ ! -f /tmp/reth-paused ]; then \
                 kill -KILL \"$(cat /tmp/reth-child.pid)\" 2>/dev/null || true; \
                 attempts=0; \
                 while [ ! -f /tmp/reth-paused ] && [ \"$attempts\" -lt 200 ]; do \
                     attempts=$((attempts + 1)); sleep 0.05; \
                 done; \
             fi; \
             if [ ! -f /tmp/reth-paused ]; then rm -f /tmp/reth-pause; exit 1; fi; \
             /usr/local/bin/reth stage unwind --datadir /data to-block \
                 --chain {GENESIS_PATH} {block_number}; \
             status=$?; \
             touch /tmp/reth-restart; \
             rm -f /tmp/reth-pause; \
             exit \"$status\""
        );
        self.container
            .exec(
                ExecCommand::new(["sh", "-c", &control_script])
                    .with_cmd_ready_condition(CmdWaitFor::exit_code(0)),
            )
            .await
            .wrap_err("Failed to unwind and restart supervised Reth process")?;

        let rpc_url = self.rpc_url().await?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .wrap_err("Failed to build Reth readiness client")?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let response = client
                .post(rpc_url.clone())
                .json(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "eth_blockNumber",
                    "params": [],
                }))
                .send()
                .await;
            let ready = match response {
                Ok(response) if response.status().is_success() => response
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|body| {
                        body.get("result").and_then(serde_json::Value::as_str).map(str::to_owned)
                    })
                    .is_some(),
                _ => false,
            };
            if ready {
                return Ok(());
            }
            ensure!(
                tokio::time::Instant::now() < deadline,
                "Reth RPC did not recover after database unwind"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

fn reth_args() -> Vec<&'static str> {
    vec![
        "node",
        "--chain=/genesis/el/genesis.json",
        "--datadir=/data",
        "--http",
        "--http.addr=0.0.0.0",
        "--http.port=8545",
        "--http.api=admin,eth,web3,net,rpc,debug,txpool",
        "--authrpc.port=8551",
        "--authrpc.addr=0.0.0.0",
        "--authrpc.jwtsecret=/genesis/jwt.hex",
        "--disable-discovery",
        "-vvv",
    ]
}
