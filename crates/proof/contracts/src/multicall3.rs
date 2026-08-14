//! `Multicall3` bindings for batching several L1 calls into a single transaction.

use alloy_primitives::{Address, Bytes, address};
use alloy_sol_types::{SolCall, sol};

sol! {
    /// `Multicall3` batching interface.
    interface IMulticall3 {
        /// A single call in a batch.
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }

        /// The outcome of a single call in a batch.
        struct Result {
            bool success;
            bytes returnData;
        }

        /// Executes each call in order, returning per-call outcomes.
        function aggregate3(Call3[] calls) external payable returns (Result[] returnData);
    }
}

/// Helpers for the canonical `Multicall3` deployment.
///
/// Batching matters for the dispute-game lifecycle beyond saving gas: calls inside one
/// `aggregate3` execute **sequentially within a single transaction**, so a parent game's
/// `resolve()` takes effect before a child's is attempted. That satisfies the
/// `ParentGameNotResolved` ordering constraint by construction, and it also makes
/// `eth_estimateGas` succeed for the whole batch — estimating the child on its own would revert
/// against pre-state while the parent is still unresolved.
#[derive(Debug, Clone, Copy)]
pub struct Multicall3;

impl Multicall3 {
    /// The canonical `Multicall3` address, identical on every supported chain.
    ///
    /// The contract is immutable, holds no funds, and is deployed via a deterministic
    /// keyless deployment. Verify presence with `eth_getCode` before relying on it for a chain.
    pub const CANONICAL_ADDRESS: Address = address!("0xcA11bde05977b3631167028862bE2a173976CA11");

    /// Encodes `aggregate3` calldata for the given `(target, calldata)` pairs, preserving order.
    ///
    /// Every call is encoded with `allowFailure = true` so that one unresolvable game cannot
    /// revert the whole batch; per-call outcomes must therefore be attributed from the receipt
    /// rather than assumed from the transaction status.
    pub fn encode_aggregate3(calls: impl IntoIterator<Item = (Address, Bytes)>) -> Bytes {
        let calls = calls
            .into_iter()
            .map(|(target, call_data)| IMulticall3::Call3 {
                target,
                allowFailure: true,
                callData: call_data,
            })
            .collect();
        Bytes::from(IMulticall3::aggregate3Call { calls }.abi_encode())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_aggregate3_preserves_call_order() {
        let first = Address::repeat_byte(0x01);
        let second = Address::repeat_byte(0x02);
        let calldata = Multicall3::encode_aggregate3(vec![
            (first, Bytes::from(vec![0xAA])),
            (second, Bytes::from(vec![0xBB])),
        ]);

        let decoded = IMulticall3::aggregate3Call::abi_decode(&calldata).expect("decodes");
        let targets: Vec<Address> = decoded.calls.iter().map(|call| call.target).collect();
        assert_eq!(targets, vec![first, second], "batch must preserve parent-before-child order");
        assert!(decoded.calls.iter().all(|call| call.allowFailure));
    }

    #[test]
    fn encode_aggregate3_handles_empty_batch() {
        let calldata = Multicall3::encode_aggregate3(Vec::new());
        let decoded = IMulticall3::aggregate3Call::abi_decode(&calldata).expect("decodes");
        assert!(decoded.calls.is_empty());
    }
}
