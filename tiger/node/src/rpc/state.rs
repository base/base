//! The state RPC reads use: a block's post-state, and on a stateless node (`--stateless.upstream`)
//! the upstream's `eth_getProof` answers for state the partial datadir does not hold, each
//! verified against tiger's own state root for that block before it is used.

use std::{cell::RefCell, collections::HashMap};

use alloy_primitives::{Address, B256, Bytes, KECCAK256_EMPTY, U256, keccak256};
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use alloy_serde::JsonStorageKey;
use alloy_trie::{Nibbles, TrieAccount, proof::verify_proof};
use base_tiger_trie::{AccountInfo, AccountProof};
use evm2::{AccountInfo as EvmAccount, bytecode::Bytecode, evm::Database};
use serde_json::{Value, json};

use crate::{Chain, Node, NodeState, RpcError, StateReadError, chain::StoredBlock};

/// The JSON-RPC error code of a state read a stateless node cannot serve: the state is not in
/// its partial datadir and no upstream is configured, the upstream failed, or its answer did not
/// verify against tiger's state root. base-reth never answers it; the message always starts with
/// [`STATE_NOT_LOCAL_MESSAGE`].
pub(super) const STATE_NOT_LOCAL_CODE: i64 = -32020;

/// The message prefix of a [`STATE_NOT_LOCAL_CODE`] error; the detail follows after `": "`.
pub(super) const STATE_NOT_LOCAL_MESSAGE: &str = "state not local";

/// The refusal of a state read this node does not hold and could not verify from an upstream.
fn not_local(detail: impl core::fmt::Display) -> RpcError {
    RpcError::new(STATE_NOT_LOCAL_CODE, format!("{STATE_NOT_LOCAL_MESSAGE}: {detail}"))
}

/// The post-state of one block for one request, falling back to a verified upstream read for
/// state the datadir does not hold. It holds a read transaction: keep it per request.
#[derive(Debug)]
pub(super) struct RpcState<'d> {
    /// The local state.
    pub(super) local: NodeState<'d>,
    upstream: Option<&'d crate::Upstream>,
    block_hash: B256,
    state_root: B256,
    /// Upstream accounts verified against `state_root`, with their storage roots.
    accounts: RefCell<HashMap<Address, (Option<AccountInfo>, B256)>>,
    /// Upstream storage values verified against their account's storage root.
    storage: RefCell<HashMap<(Address, U256), U256>>,
    /// Upstream code verified against its hash.
    codes: RefCell<HashMap<B256, Bytes>>,
    /// The account each code hash was read for, to fetch the code from the upstream by address.
    code_owners: RefCell<HashMap<B256, Address>>,
    /// The first read neither the datadir nor the upstream could serve.
    failure: RefCell<Option<RpcError>>,
}

impl Node {
    /// The post-state of `block` for a request's reads.
    pub(super) fn rpc_state<'d>(
        &'d self,
        chain: &'d Chain,
        block: &StoredBlock,
    ) -> Result<RpcState<'d>, RpcError> {
        Self::check_state_held(chain, block)?;
        let local = chain.state_after(block).map_err(|error| Self::state_error(chain, block, error))?;
        Ok(self.rpc_state_of(local, block))
    }

    /// The error for a state of `block` that would not open: [`RpcError::REORGED`] when `block`
    /// left the canonical chain (a reorg dropped its branch after the request resolved it), else
    /// `-32603`.
    pub fn state_error(chain: &Chain, block: &StoredBlock, error: String) -> RpcError {
        match chain.is_canonical(&block.hash) {
            true => RpcError::internal(error),
            false => RpcError::new(RpcError::REORGED, error),
        }
    }

    /// An error when the datadir pruned the post-state of canonical `block`.
    pub(super) fn check_state_held(chain: &Chain, block: &StoredBlock) -> Result<(), RpcError> {
        if block.number() < chain.earliest_state() && chain.is_canonical(&block.hash) {
            return Err(Self::pruned(block.number(), chain.earliest_state()));
        }
        Ok(())
    }

    /// `local`, the post-state of `block`, for a request's reads.
    pub(super) fn rpc_state_of<'d>(&'d self, local: NodeState<'d>, block: &StoredBlock) -> RpcState<'d> {
        RpcState {
            local,
            upstream: self.upstream.as_ref(),
            block_hash: block.hash,
            state_root: block.header.state_root,
            accounts: RefCell::default(),
            storage: RefCell::default(),
            codes: RefCell::default(),
            code_owners: RefCell::default(),
            failure: RefCell::default(),
        }
    }
}

impl RpcState<'_> {
    /// The account at `address`.
    pub(super) fn account(&self, address: &Address) -> Result<Option<AccountInfo>, RpcError> {
        let account = match self.local.account(address) {
            Ok(account) => account,
            Err(error) => self.fallback(error, || Ok(self.upstream_account(*address, &[])?.0))?,
        };
        if let Some(account) = &account {
            self.code_owners.borrow_mut().entry(account.code_hash).or_insert(*address);
        }
        Ok(account)
    }

    /// The value of storage `slot` of `address`.
    pub(super) fn storage(&self, address: &Address, slot: &U256) -> Result<U256, RpcError> {
        match self.local.storage(address, slot) {
            Ok(value) => Ok(value),
            Err(error) => self.fallback(error, || {
                if let Some(value) = self.storage.borrow().get(&(*address, *slot)) {
                    return Ok(*value);
                }
                self.upstream_account(*address, &[*slot])?;
                Ok(self.storage.borrow()[&(*address, *slot)])
            }),
        }
    }

    /// The code with hash `hash`, of an account read before.
    pub(super) fn code(&self, hash: &B256) -> Result<Bytes, RpcError> {
        match self.local.code(hash) {
            Ok(code) => Ok(code),
            Err(error) => self.fallback(error, || self.upstream_code(hash)),
        }
    }

    /// `eth_getProof` of `address` and `keys`, which echo back in the form given.
    pub(super) fn proof_json(
        &self,
        address: Address,
        keys: &[JsonStorageKey],
    ) -> Result<Value, RpcError> {
        let slots: Vec<U256> =
            keys.iter().map(|key| U256::from_be_bytes(key.as_b256().0)).collect();
        let proof: AccountProof = match self.local.proof(address, &slots) {
            Ok(proof) => proof,
            // The upstream is base-reth: its verified answer is the reply.
            Err(error) => {
                return self.fallback(error, || {
                    self.upstream_proof(address, keys).map(|(reply, _)| reply)
                });
            }
        };
        let storage_proof: Vec<Value> = keys
            .iter()
            .zip(&proof.storage)
            .map(|(key, storage)| json!({ "key": key, "value": storage.value, "proof": storage.proof }))
            .collect();
        let account = proof.account;
        Ok(json!({
            "address": address,
            "accountProof": proof.proof,
            "balance": account.map_or(U256::ZERO, |account| account.balance),
            "codeHash": account.and_then(|account| account.bytecode_hash).unwrap_or(KECCAK256_EMPTY),
            "nonce": alloy_primitives::U64::from(account.map_or(0, |account| account.nonce)),
            "storageHash": proof.storage_root,
            "storageProof": storage_proof,
        }))
    }

    /// Maps a call failure caused by a state read to the read's error.
    pub(super) fn read_error(&self, error: RpcError) -> RpcError {
        match (self.failure.borrow().as_ref(), crate::ParentState::missing(&self.local)) {
            (Some(failure), _) if error.code == -32603 => failure.clone(),
            (None, Some(missing)) if error.code == -32603 => RpcError::internal(missing),
            _ => error,
        }
    }

    /// Answers a failed local read from the upstream when the read needed state the datadir
    /// does not hold and an upstream is configured; otherwise the read fails.
    fn fallback<T>(
        &self,
        local: String,
        upstream: impl FnOnce() -> Result<T, RpcError>,
    ) -> Result<T, RpcError> {
        let result = match self.upstream {
            Some(_) if self.local.is_unknown() => upstream(),
            Some(_) => Err(RpcError::internal(local)),
            None if self.local.is_unknown() => {
                Err(not_local(local))
            }
            None => Err(RpcError::internal(local)),
        };
        if let Err(error) = &result {
            self.failure.borrow_mut().get_or_insert_with(|| error.clone());
        }
        result
    }

    fn block_id(&self) -> Value {
        json!({ "blockHash": self.block_hash })
    }

    /// The upstream's `eth_getProof` of `address` and `keys` at this block, verified against
    /// this block's state root; caches the account and the slots.
    fn upstream_proof(
        &self,
        address: Address,
        keys: &[JsonStorageKey],
    ) -> Result<(Value, EIP1186AccountProofResponse), RpcError> {
        let upstream = self.upstream.expect("fallbacks run with an upstream");
        let reply = upstream
            .call("eth_getProof", json!([address, keys, self.block_id()]))
            .map_err(not_local)?;
        let proof: EIP1186AccountProofResponse = serde_json::from_value(reply.clone())
            .map_err(|error| not_local(format!("upstream eth_getProof: {error}")))?;
        let account = (!proof.is_empty()).then_some(AccountInfo {
            nonce: proof.nonce,
            balance: proof.balance,
            code_hash: proof.code_hash,
        });
        let leaf = account.map(|account| {
            alloy_rlp::encode(TrieAccount {
                nonce: account.nonce,
                balance: account.balance,
                storage_root: proof.storage_hash,
                code_hash: account.code_hash,
            })
        });
        let unverified = |what: String| {
            not_local(format!("upstream eth_getProof of {address} does not verify: {what}"))
        };
        if proof.address != address || proof.storage_proof.len() != keys.len() {
            return Err(unverified("wrong account or slots".into()));
        }
        verify_proof(
            self.state_root,
            Nibbles::unpack(keccak256(address)),
            leaf,
            &proof.account_proof,
        )
        .map_err(|error| unverified(error.to_string()))?;
        for (key, slot) in keys.iter().zip(&proof.storage_proof) {
            if slot.key.as_b256() != key.as_b256() {
                return Err(unverified("wrong slot".into()));
            }
            let value = (!slot.value.is_zero()).then(|| alloy_rlp::encode(slot.value));
            verify_proof(
                proof.storage_hash,
                Nibbles::unpack(keccak256(key.as_b256())),
                value,
                &slot.proof,
            )
            .map_err(|error| unverified(error.to_string()))?;
            self.storage
                .borrow_mut()
                .insert((address, U256::from_be_bytes(key.as_b256().0)), slot.value);
        }
        self.accounts.borrow_mut().insert(address, (account, proof.storage_hash));
        Ok((reply, proof))
    }

    /// The verified upstream account at `address`, reading `slots` with it.
    fn upstream_account(
        &self,
        address: Address,
        slots: &[U256],
    ) -> Result<(Option<AccountInfo>, B256), RpcError> {
        if slots.is_empty() {
            if let Some(account) = self.accounts.borrow().get(&address) {
                return Ok(*account);
            }
        }
        let keys: Vec<JsonStorageKey> =
            slots.iter().map(|slot| JsonStorageKey::from(B256::from(*slot))).collect();
        self.upstream_proof(address, &keys)?;
        Ok(self.accounts.borrow()[&address])
    }

    /// The upstream's code with `hash`, read by the address it was read for and checked against
    /// the hash.
    fn upstream_code(&self, hash: &B256) -> Result<Bytes, RpcError> {
        if let Some(code) = self.codes.borrow().get(hash) {
            return Ok(code.clone());
        }
        let Some(owner) = self.code_owners.borrow().get(hash).copied() else {
            return Err(not_local(format!("code {hash} of no account read")));
        };
        let reply = self.upstream.expect("fallbacks run with an upstream");
        let reply =
            reply.call("eth_getCode", json!([owner, self.block_id()])).map_err(not_local)?;
        let code: Bytes = serde_json::from_value(reply)
            .map_err(|error| not_local(format!("upstream eth_getCode: {error}")))?;
        if keccak256(&code) != *hash {
            return Err(not_local(format!("upstream code of {owner} does not hash to {hash}")));
        }
        self.codes.borrow_mut().insert(*hash, code.clone());
        Ok(code)
    }
}

/// The evm2 database over an [`RpcState`].
#[derive(Debug)]
pub(super) struct RpcDb<'s, 'd>(pub &'s RpcState<'d>);

impl Database for RpcDb<'_, '_> {
    type Error = StateReadError;

    fn get_account(&mut self, address: &Address) -> Result<Option<EvmAccount>, StateReadError> {
        let account = self.0.account(address).map_err(|error| StateReadError(error.message))?;
        Ok(account.map(|account| EvmAccount {
            balance: account.balance,
            nonce: account.nonce,
            code_hash: account.code_hash,
            code: None,
            ..EvmAccount::default()
        }))
    }

    fn get_code_by_hash(&mut self, code_hash: &B256) -> Result<Bytecode, StateReadError> {
        Ok(Bytecode::new_raw(
            self.0.code(code_hash).map_err(|error| StateReadError(error.message))?,
        ))
    }

    fn get_storage(&mut self, address: &Address, key: &U256) -> Result<U256, StateReadError> {
        self.0.storage(address, key).map_err(|error| StateReadError(error.message))
    }

    fn get_block_hash(&mut self, number: &U256) -> Result<B256, StateReadError> {
        self.0.local.block_hash(number.saturating_to()).map_err(StateReadError)
    }
}
