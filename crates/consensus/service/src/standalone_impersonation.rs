//! Synthetic deposits submitted through `dev_impersonateTransaction`.

use std::{
    collections::{HashSet, VecDeque, hash_map::RandomState},
    hash::BuildHasher,
    sync::{Arc, Mutex, PoisonError},
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_consensus::Sealed;
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, keccak256};
use base_common_chains::BaseUpgradeExt;
use base_common_consensus::TxDeposit;
use base_common_genesis::BaseUpgrade;
use jsonrpsee::{
    RpcModule,
    types::{
        ErrorObjectOwned,
        error::{CALL_EXECUTION_FAILED_CODE, INVALID_PARAMS_CODE},
    },
};
use revm::{
    context::{TxEnv, result::InvalidTransaction},
    handler::validation::validate_initial_tx_gas,
    primitives::{eip3860::MAX_INITCODE_SIZE, hardfork::SpecId},
};
use serde::Deserialize;

/// Parameters of `dev_impersonateTransaction`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImpersonationRequest {
    /// Account the deposit executes as. No key or signature is needed.
    pub from: Address,
    /// Call target; omit it to create a contract.
    #[serde(default)]
    pub to: Option<Address>,
    /// Calldata, or init code when `to` is omitted.
    #[serde(default)]
    pub input: Bytes,
    /// Wei transferred from `from` to `to`.
    #[serde(default)]
    pub value: U256,
    /// Gas limit of the deposit.
    #[serde(with = "alloy_serde::quantity")]
    pub gas: u64,
    /// Wei minted to `from` before execution.
    #[serde(default, with = "alloy_serde::quantity")]
    pub mint: u128,
}

/// Bounded queue of impersonation deposits shared by the RPC endpoint and the standalone
/// attributes builder.
///
/// Every request gets a source hash from per-process entropy and a request counter, so identical
/// requests are distinct transactions and a request keeps its hash when a build is retried. A
/// request leaves the queue only when an acknowledged canonical payload contains it. Pending
/// requests live only in memory.
#[derive(Debug, Clone)]
pub struct StandaloneImpersonationQueue {
    entropy: B256,
    state: Arc<Mutex<StandaloneImpersonationState>>,
}

/// State shared by every clone of a [`StandaloneImpersonationQueue`].
#[derive(Debug, Default)]
pub struct StandaloneImpersonationState {
    /// Largest request gas limit and the EVM spec used to validate requests; unset until the
    /// sequencer starts.
    pub limits: Option<(u64, SpecId)>,
    /// Number of accepted requests, which numbers source hashes.
    pub accepted: u64,
    /// Accepted deposits not yet acknowledged in a canonical payload, oldest first.
    pub pending: VecDeque<Sealed<TxDeposit>>,
}

impl Default for StandaloneImpersonationQueue {
    fn default() -> Self {
        let time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let random = RandomState::new().hash_one(time);
        Self {
            entropy: keccak256([time.to_be_bytes(), u128::from(random).to_be_bytes()].concat()),
            state: Default::default(),
        }
    }
}

impl StandaloneImpersonationQueue {
    /// JSON-RPC method name.
    pub const METHOD: &str = "dev_impersonateTransaction";
    /// Maximum number of pending requests.
    pub const MAX_PENDING: usize = 256;
    /// Maximum calldata length of a call request.
    pub const MAX_INPUT_BYTES: usize = 128 * 1024;

    /// Starts accepting requests with gas limits up to `max_gas`, validated under `upgrade`.
    pub fn activate(&self, max_gas: u64, upgrade: BaseUpgrade) {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).limits =
            Some((max_gas, upgrade.into_eth_spec()));
    }

    /// Validates and queues a request, returning its deposit transaction hash.
    pub fn submit(&self, request: ImpersonationRequest) -> Result<B256, ImpersonationError> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (max_gas, spec) = state.limits.ok_or(ImpersonationError::NotReady)?;
        if request.gas > max_gas {
            return Err(ImpersonationError::GasAboveLimit { gas: request.gas, limit: max_gas });
        }
        let max_input =
            if request.to.is_some() { Self::MAX_INPUT_BYTES } else { MAX_INITCODE_SIZE };
        if request.input.len() > max_input {
            return Err(ImpersonationError::InputTooLarge {
                len: request.input.len(),
                limit: max_input,
            });
        }
        let to = request.to.map_or(TxKind::Create, TxKind::Call);
        let tx = TxEnv {
            kind: to,
            data: request.input.clone(),
            gas_limit: request.gas,
            ..Default::default()
        };
        validate_initial_tx_gas(tx, spec, false, false, u64::MAX, None)?;
        if state.pending.len() >= Self::MAX_PENDING {
            return Err(ImpersonationError::QueueFull);
        }

        let source_hash =
            keccak256([self.entropy.as_slice(), &state.accepted.to_be_bytes()].concat());
        state.accepted += 1;
        let deposit = Sealed::new(TxDeposit {
            source_hash,
            from: request.from,
            to,
            mint: request.mint,
            value: request.value,
            gas_limit: request.gas,
            is_system_transaction: false,
            input: request.input,
        });
        let hash = deposit.hash();
        state.pending.push_back(deposit);
        Ok(hash)
    }

    /// Returns the oldest pending deposits, in order, whose gas limits fit within `gas`.
    pub fn select(&self, mut gas: u64) -> Vec<Bytes> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pending
            .iter()
            .map_while(|deposit| {
                gas = gas.checked_sub(deposit.gas_limit)?;
                Some(deposit.encoded_2718().into())
            })
            .collect()
    }

    /// Removes pending deposits contained in an acknowledged canonical payload.
    pub fn retire(&self, transactions: &[Bytes]) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.pending.is_empty() {
            let included: HashSet<_> = transactions.iter().map(keccak256).collect();
            state.pending.retain(|deposit| !included.contains(&deposit.hash()));
        }
    }

    /// Returns an RPC module serving [`Self::METHOD`] from this queue.
    pub fn into_rpc(self) -> RpcModule<Self> {
        let mut module = RpcModule::new(self);
        module
            .register_method(Self::METHOD, |params, queue, _| {
                queue.submit(params.one()?).map_err(|error| {
                    let code = match error {
                        ImpersonationError::NotReady | ImpersonationError::QueueFull => {
                            CALL_EXECUTION_FAILED_CODE
                        }
                        _ => INVALID_PARAMS_CODE,
                    };
                    ErrorObjectOwned::owned(code, error.to_string(), None::<()>)
                })
            })
            .expect("method is registered once");
        module
    }
}

/// Reason a `dev_impersonateTransaction` request was not queued.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImpersonationError {
    /// The sequencer has not started; retry later.
    #[error("development chain is not sequencing yet")]
    NotReady,
    /// Too many requests are pending; retry after some are included.
    #[error("impersonation queue is full")]
    QueueFull,
    /// The gas limit cannot fit in a block beside the system deposits.
    #[error("gas {gas} exceeds the per-request limit {limit}")]
    GasAboveLimit {
        /// Requested gas limit.
        gas: u64,
        /// Largest accepted gas limit.
        limit: u64,
    },
    /// The input is too long.
    #[error("input of {len} bytes exceeds the {limit}-byte limit")]
    InputTooLarge {
        /// Input length.
        len: usize,
        /// Largest accepted input length.
        limit: usize,
    },
    /// The gas limit is below the intrinsic or calldata floor cost.
    #[error("gas limit too low: {0}")]
    Gas(#[from] InvalidTransaction),
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bytes, U256, keccak256};
    use base_common_genesis::BaseUpgrade;

    use super::{ImpersonationError, ImpersonationRequest, StandaloneImpersonationQueue};

    #[test]
    fn selects_oldest_requests_that_fit_until_retired() {
        let queue = StandaloneImpersonationQueue::default();
        let request = ImpersonationRequest {
            from: Address::repeat_byte(1),
            to: Some(Address::repeat_byte(2)),
            input: Bytes::new(),
            value: U256::ZERO,
            gas: 30_000,
            mint: 0,
        };
        assert_eq!(queue.submit(request.clone()), Err(ImpersonationError::NotReady));
        queue.activate(30_000, BaseUpgrade::Denim);
        let first = queue.submit(request.clone()).unwrap();
        let second = queue.submit(request.clone()).unwrap();
        queue.submit(ImpersonationRequest { gas: 21_000, ..request.clone() }).unwrap();
        assert_ne!(first, second);

        // A later request that fits does not overtake an earlier one that does not.
        let selected = queue.select(51_000);
        assert_eq!(selected.iter().map(keccak256).collect::<Vec<_>>(), [first]);
        assert_eq!(queue.select(51_000), selected);

        queue.retire(&selected);
        let remaining = queue.select(u64::MAX);
        assert_eq!(remaining.len(), 2);
        assert_eq!(keccak256(&remaining[0]), second);

        for _ in 2..StandaloneImpersonationQueue::MAX_PENDING {
            queue.submit(request.clone()).unwrap();
        }
        assert_eq!(queue.submit(request.clone()), Err(ImpersonationError::QueueFull));
        queue.retire(&remaining[..1]);
        queue.submit(request).unwrap();
    }
}
