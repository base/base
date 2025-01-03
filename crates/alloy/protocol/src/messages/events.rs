//! Message event primitives for OP stack interoperability.
//!
//! <https://specs.optimism.io/interop/messaging.html#messaging>
//! <https://github.com/ethereum-optimism/optimism/blob/34d5f66ade24bd1f3ce4ce7c0a6cfc1a6540eca1/packages/contracts-bedrock/src/L2/CrossL2Inbox.sol>
use alloc::vec;
use alloy_primitives::{keccak256, Address, Bytes, Log, U256};
use alloy_sol_types::{sol, SolType};
use derive_more::{AsRef, From};

sol! {
    /// @notice The struct for a pointer to a message payload in a remote (or local) chain.
    #[derive(Default, Debug, PartialEq, Eq)]
    struct MessageIdentifierAbi {
        address origin;
        uint256 blockNumber;
        uint256 logIndex;
        uint256 timestamp;
        uint256 chainId;
    }

    /// @notice Emitted when a cross chain message is being executed.
    /// @param msgHash Hash of message payload being executed.
    /// @param id Encoded Identifier of the message.
    #[derive(Default, Debug, PartialEq, Eq)]
    event ExecutingMessage(bytes32 indexed msgHash, MessageIdentifierAbi id);

    /// @notice Executes a cross chain message on the destination chain.
    /// @param _id      Identifier of the message.
    /// @param _target  Target address to call.
    /// @param _message Message payload to call target with.
    function executeMessage(
        MessageIdentifierAbi calldata _id,
        address _target,
        bytes calldata _message
    ) external;
}

/// A [`MessagePayload`] is the raw payload of an initiating message.
#[derive(Debug, Clone, From, AsRef, PartialEq, Eq)]
pub struct MessagePayload(Bytes);

impl From<Log> for MessagePayload {
    fn from(log: Log) -> Self {
        let mut data = vec![0u8; log.topics().len() * 32 + log.data.data.len()];
        for (i, topic) in log.topics().iter().enumerate() {
            data[i * 32..(i + 1) * 32].copy_from_slice(topic.as_ref());
        }
        data[(log.topics().len() * 32)..].copy_from_slice(log.data.data.as_ref());
        Bytes::from(data).into()
    }
}

/// A [`MessageIdentifier`] uniquely represents a log that is emitted from a chain within
/// the broader dependency set. It is included in the calldata of a transaction sent to the
/// CrossL2Inbox contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct MessageIdentifier {
    /// The account that sent the message.
    pub origin: Address,
    /// The block number that the message was sent in.
    pub block_number: u64,
    /// The log index of the message in the block (global).
    pub log_index: u64,
    /// The timestamp of the message.
    pub timestamp: u64,
    /// The chain ID of the chain that the message was sent on.
    #[cfg_attr(feature = "serde", serde(rename = "chainID"))]
    pub chain_id: u64,
}

impl MessageIdentifier {
    /// Decode a [`MessageIdentifier`] from ABI-encoded data.
    pub fn abi_decode(data: &[u8], validate: bool) -> Result<Self, alloy_sol_types::Error> {
        MessageIdentifierAbi::abi_decode(data, validate).map(|abi| abi.into())
    }
}

impl From<MessageIdentifierAbi> for MessageIdentifier {
    fn from(abi: MessageIdentifierAbi) -> Self {
        Self {
            origin: abi.origin,
            block_number: abi.blockNumber.to(),
            log_index: abi.logIndex.to(),
            timestamp: abi.timestamp.to(),
            chain_id: abi.chainId.to(),
        }
    }
}

impl From<MessageIdentifier> for MessageIdentifierAbi {
    fn from(id: MessageIdentifier) -> Self {
        Self {
            origin: id.origin,
            blockNumber: U256::from(id.block_number),
            logIndex: U256::from(id.log_index),
            timestamp: U256::from(id.timestamp),
            chainId: U256::from(id.chain_id),
        }
    }
}

impl From<executeMessageCall> for ExecutingMessage {
    fn from(call: executeMessageCall) -> Self {
        Self { id: call._id, msgHash: keccak256(call._message.as_ref()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_identifier_serde() {
        let raw_id = r#"
            {
                "origin": "0x6887246668a3b87F54DeB3b94Ba47a6f63F32985",
                "blockNumber": 123456,
                "logIndex": 789,
                "timestamp": 1618932000,
                "chainID": 420
            }
        "#;
        let id: MessageIdentifier = serde_json::from_str(raw_id).unwrap();
        let expected = MessageIdentifier {
            origin: "0x6887246668a3b87F54DeB3b94Ba47a6f63F32985".parse().unwrap(),
            block_number: 123456,
            log_index: 789,
            timestamp: 1618932000,
            chain_id: 420,
        };
        assert_eq!(id, expected);
    }
}
