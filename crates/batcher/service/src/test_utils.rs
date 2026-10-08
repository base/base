//! A fake sequencer endpoint for the tests of the leader search, and the clients the tests build
//! it with.
//!
//! The fake is hand-rolled because [`Sequencers`](crate::Sequencers) builds concrete clients from URLs, a
//! jsonrpsee `HttpClient` and an alloy `RootProvider`, so there is no trait to automock. It is a
//! real JSON-RPC server rather than an `httpmock` mock because the client rejects an answer whose
//! id is not its request's, and these tests call one endpoint several times.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use jsonrpsee::{
    RpcModule,
    server::{Server, ServerHandle},
    types::ErrorObjectOwned,
};
use serde_json::Value;
use url::Url;

use crate::{BatcherConfig, RpcClientBuilder};

/// A builder of clients with the network timeout of [`BatcherConfig::default`].
pub fn rpc_client_builder() -> RpcClientBuilder {
    RpcClientBuilder::new(BatcherConfig::default().network_timeout)
}

/// What a [`FakeSequencer`] answers to `admin_sequencerActive`.
#[derive(Debug, Clone, Copy)]
pub enum Activity {
    /// `true`, the answer of the leader.
    Active,
    /// `false`, the answer of a sequencer that is stopped.
    Stopped,
    /// An error, the answer of the conductor of a sequencer that is not the leader.
    NotLeader,
}

/// The state the fake's methods read and update.
#[derive(Debug)]
struct State {
    activity: Mutex<Activity>,
    rollup_config_calls: AtomicUsize,
}

/// A sequencer endpoint whose answer to `admin_sequencerActive` the test sets, which answers
/// `eth_chainId` with its own `chain_id` so a test can tell which one served a request, and
/// which counts the `optimism_rollupConfig` requests it answers with `null`. Its
/// `admin_sequencerActive` error message is `not the leader`. The server stops when the fake is
/// dropped.
#[derive(Debug)]
pub struct FakeSequencer {
    /// The endpoint URL.
    pub url: Url,
    state: Arc<State>,
    _server: ServerHandle,
}

impl FakeSequencer {
    /// Starts a fake sequencer answering `activity`.
    pub async fn start(activity: Activity, chain_id: u64) -> Self {
        let state = Arc::new(State {
            activity: Mutex::new(activity),
            rollup_config_calls: AtomicUsize::new(0),
        });
        let mut module = RpcModule::new(Arc::clone(&state));
        module
            .register_method("admin_sequencerActive", |_, state, _| {
                match *state.activity.lock().unwrap() {
                    Activity::Active => Ok(true),
                    Activity::Stopped => Ok(false),
                    Activity::NotLeader => {
                        Err(ErrorObjectOwned::owned(-32000, "not the leader", None::<()>))
                    }
                }
            })
            .unwrap();
        module.register_method("eth_chainId", move |_, _, _| format!("{chain_id:#x}")).unwrap();
        module
            .register_method("optimism_rollupConfig", |_, state, _| {
                state.rollup_config_calls.fetch_add(1, Ordering::Relaxed);
                Value::Null
            })
            .unwrap();

        let server = Server::builder().build("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", server.local_addr().unwrap()).parse().unwrap();
        Self { url, state, _server: server.start(module) }
    }

    /// Changes the answer to `admin_sequencerActive`.
    pub fn set_activity(&self, activity: Activity) {
        *self.state.activity.lock().unwrap() = activity;
    }

    /// How many `optimism_rollupConfig` requests the fake has answered.
    pub fn rollup_config_calls(&self) -> usize {
        self.state.rollup_config_calls.load(Ordering::Relaxed)
    }
}
