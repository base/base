use getrandom as _;
use getrandom_03 as _;
use wasm_bindgen::prelude::*;

use crate::Devnet;

/// JS-accessible entry point for the in-browser devnet.
#[derive(Debug)]
#[wasm_bindgen]
pub struct WasmDevnet {
    inner: Devnet,
}

#[wasm_bindgen]
impl WasmDevnet {
    /// Create and initialize a new devnet.
    pub async fn create() -> WasmDevnet {
        WasmDevnet { inner: Devnet::new().await }
    }

    /// Mine `n` L1 blocks; returns the new L1 tip block number.
    pub fn mine_l1_blocks(&mut self, n: u64) -> u64 {
        self.inner.mine_l1_blocks(n)
    }

    /// Run one epoch: mine L1 blocks, produce L2 blocks, submit, and derive.
    pub async fn run_epoch(&mut self, l1_blocks: u64, l2_blocks: u64) -> usize {
        self.inner.run_epoch(l1_blocks, l2_blocks).await
    }

    /// Return the sequencer unsafe head block number.
    pub fn sequencer_head_number(&self) -> u64 {
        self.inner.sequencer_head().block_info.number
    }

    /// Return the validator safe head block number.
    pub fn validator_safe_number(&self) -> u64 {
        self.inner.validator_safe().block_info.number
    }

    /// Return the validator unsafe head block number.
    pub fn validator_unsafe_number(&self) -> u64 {
        self.inner.validator_unsafe().block_info.number
    }

    /// Return the current L1 tip block number.
    pub fn l1_tip_number(&self) -> u64 {
        self.inner.l1_tip_number()
    }

    /// Advance derivation without producing new blocks; returns the number of new safe blocks.
    pub async fn derive_until_idle(&mut self) -> usize {
        self.inner.derive_until_idle().await
    }

    /// Return the number of L2 blocks whose derived hash matches the sequenced hash.
    pub fn verified_block_count(&self) -> u64 {
        self.inner.verified_block_count()
    }

    /// Queue a raw EIP-2718 encoded transaction for inclusion in the next L2 block.
    pub fn queue_transaction(&mut self, tx_bytes: Vec<u8>) -> bool {
        self.inner.queue_transaction(tx_bytes).is_ok()
    }

    /// Create a signed EIP-1559 ETH transfer from the devnet key; returns the encoded bytes.
    pub fn create_test_transfer(&mut self, to: Vec<u8>, value_wei: u64) -> Vec<u8> {
        let arr: [u8; 20] = to.as_slice().try_into().expect("to must be 20 bytes");
        self.inner.create_test_transfer(alloy_primitives::Address::from(arr), value_wei)
    }

    /// Create a signed EIP-1559 contract-deployment transaction from the devnet key.
    pub fn create_test_contract_deploy(&mut self, init_code: Vec<u8>, value_wei: u64) -> Vec<u8> {
        self.inner.create_test_contract_deploy(init_code, value_wei)
    }

    /// Return the L2 chain ID.
    pub fn chain_id(&self) -> u64 {
        self.inner.devnet_chain_id()
    }

    /// Mine + produce + encode `n` blocks without deriving; returns total frame count.
    pub fn mine_and_encode(&mut self, n: u64) -> u64 {
        self.inner.mine_and_encode(n)
    }

    /// Return per-block derivation debug info (one line per derived block).
    pub fn get_derive_debug(&self) -> String {
        self.inner.get_derive_debug()
    }

    /// Handle one JSON-RPC 2.0 request string and return the serialized response string.
    pub fn rpc_request(&mut self, request_json: String) -> String {
        self.inner.rpc_request(&request_json)
    }

    /// Return the hex-encoded address of the pre-funded developer account.
    pub fn dev_account_address(&self) -> String {
        format!("{}", self.inner.dev_account_address())
    }
}
