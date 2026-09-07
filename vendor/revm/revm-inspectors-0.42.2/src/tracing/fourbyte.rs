//! Fourbyte tracing inspector
//!
//! Solidity contract functions are addressed using the first four bytes of the Keccak-256 hash of
//! their signature. Therefore when calling the function of a contract, the caller must send this
//! function selector as well as the ABI-encoded arguments as call data.
//!
//! The 4byteTracer collects the function selectors of every function executed in the lifetime of a
//! transaction, along with the size of the supplied call data. The result is a map of
//! SELECTOR-CALLDATASIZE to number of occurrences entries, where the keys are SELECTOR-CALLDATASIZE
//! and the values are number of occurrences of this key. For example:
//!
//! ```json
//! {
//!   "0x27dc297e-128": 1,
//!   "0x38cc4831-0": 2,
//!   "0x524f3889-96": 1,
//!   "0xadf59f99-288": 1,
//!   "0xc281d19e-0": 1
//! }
//! ```
//!
//! See also <https://geth.ethereum.org/docs/developers/evm-tracing/built-in-tracers>
use alloc::format;
use alloy_primitives::{hex, map::HashMap, Selector};
use alloy_rpc_types_trace::geth::FourByteFrame;
use revm::{
    context::{ContextTr, LocalContextTr},
    interpreter::{CallInput, CallInputs, CallOutcome},
    Inspector,
};

/// Fourbyte tracing inspector that records all function selectors and their calldata sizes.
#[derive(Clone, Debug, Default)]
pub struct FourByteInspector {
    /// The map of SELECTOR to number of occurrences entries
    inner: HashMap<(Selector, usize), u64>,
}

impl FourByteInspector {
    /// Returns the map of SELECTOR to number of occurrences entries
    pub const fn inner(&self) -> &HashMap<(Selector, usize), u64> {
        &self.inner
    }
}

impl<CTX: ContextTr> Inspector<CTX> for FourByteInspector {
    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        if inputs.input.len() >= 4 {
            let r;
            let input_bytes = match &inputs.input {
                CallInput::SharedBuffer(range) => {
                    match context.local().shared_memory_buffer_slice(range.clone()) {
                        Some(slice) => {
                            r = slice;
                            &*r
                        }
                        None => &[],
                    }
                }
                CallInput::Bytes(bytes) => bytes.as_ref(),
            };

            let selector =
                Selector::try_from(&input_bytes[..4]).expect("input is at least 4 bytes");
            let calldata_size = input_bytes[4..].len();
            *self.inner.entry((selector, calldata_size)).or_default() += 1;
        }

        None
    }
}

impl From<FourByteInspector> for FourByteFrame {
    fn from(value: FourByteInspector) -> Self {
        Self::from(&value)
    }
}

impl From<&FourByteInspector> for FourByteFrame {
    fn from(value: &FourByteInspector) -> Self {
        Self(
            value
                .inner
                .iter()
                .map(|((selector, calldata_size), count)| {
                    let key = format!("0x{}-{}", hex::encode(selector), *calldata_size);
                    (key, *count)
                })
                .collect(),
        )
    }
}
