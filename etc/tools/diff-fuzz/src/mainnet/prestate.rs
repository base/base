//! Synthetic pre-states built from `prestateTracer` traces, for mainnet blocks whose real
//! witness is unavailable.

use std::collections::BTreeMap;

use alloy_primitives::{Address, B256, Bytes, U256};
use eyre::{Context, ensure};
use serde::Deserialize;
use serde_json::json;

use super::RpcClient;
use crate::{Account, Witness, WorldState};

/// One account of a `prestateTracer` result; absent fields are zero or empty.
#[derive(Debug, Default, Deserialize)]
struct TracedAccount {
    #[serde(default)]
    balance: Option<U256>,
    #[serde(default)]
    nonce: Option<u64>,
    #[serde(default)]
    code: Option<Bytes>,
    #[serde(default)]
    storage: BTreeMap<B256, U256>,
}

#[derive(Debug, Deserialize)]
struct TxPrestate {
    result: BTreeMap<Address, TracedAccount>,
}

/// Builds the pre-state of a block from per-transaction `prestateTracer` traces: every account
/// and slot takes the value the first transaction touching it saw, which is its value before the
/// block, since a change by an earlier transaction would have put it in that transaction's trace.
///
/// The result is a full trie of only the state the block touches, so its root differs from the
/// parent's state root and is sent as [`Witness::root`]. Execution results that do not depend on
/// untouched state (receipts, logs, gas) still match the canonical block; the state root, the
/// withdrawals root and the block hash do not. State the node reads outside the EVM journal
/// (system calls) is missing and reads as empty, the same for every client. Empty accounts are
/// left out, which is equivalent after EIP-161.
#[derive(Debug, Clone)]
pub struct PrestateWitness {
    /// Endpoints serving archive `debug_traceBlockByNumber` with the `prestateTracer`.
    pub trace: RpcClient,
}

impl PrestateWitness {
    /// The synthetic pre-state of block `number` and its root.
    pub fn build(&self, number: u64) -> eyre::Result<(B256, Witness)> {
        let traces: Vec<TxPrestate> = serde_json::from_value(self.trace.call(
            "debug_traceBlockByNumber",
            json!([format!("{number:#x}"), { "tracer": "prestateTracer" }]),
        )?)
        .wrap_err("decoding prestate traces")?;
        ensure!(!traces.is_empty(), "block {number} has no prestate traces");
        let mut world = WorldState::default();
        for trace in traces {
            for (address, traced) in trace.result {
                if !world.accounts.contains_key(&address) {
                    let code_hash = traced
                        .code
                        .filter(|code| !code.is_empty())
                        .map(|code| world.register_code(code));
                    let account = world.accounts.entry(address).or_default();
                    account.nonce = traced.nonce.unwrap_or_default();
                    account.balance = traced.balance.unwrap_or_default();
                    if let Some(code_hash) = code_hash {
                        account.code_hash = code_hash;
                    }
                }
                let account = world.accounts.get_mut(&address).expect("inserted above");
                for (slot, value) in traced.storage {
                    account.storage.entry(slot).or_insert(value);
                }
            }
        }
        for account in world.accounts.values_mut() {
            account.storage.retain(|_, value| !value.is_zero());
        }
        world.accounts.retain(|_, account| *account != Account::default());
        let (root, mut witness) = world.witness();
        witness.root = Some(root);
        Ok((root, witness))
    }
}
