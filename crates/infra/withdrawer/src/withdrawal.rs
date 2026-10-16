use alloy_primitives::{B256, Bytes, U256, keccak256};
use alloy_rlp::Decodable;
use alloy_rpc_types_eth::{EIP1186AccountProofResponse, Header};
use alloy_sol_types::{SolEvent, SolValue, sol};
use alloy_trie::{
    Nibbles, TrieAccount,
    nodes::TrieNode,
    proof::{ProofVerificationError, verify_proof},
};
use base_common_consensus::Predeploys;
use base_common_rpc_types::BaseTransactionReceipt;
use base_protocol::OutputRoot;
use thiserror::Error;

sol! {
    /// Withdrawal arguments accepted by the portal.
    #[derive(Debug, PartialEq, Eq)]
    struct WithdrawalTransaction {
        uint256 nonce;
        address sender;
        address target;
        uint256 value;
        uint256 gasLimit;
        bytes data;
    }

    /// Version-zero output commitment preimage accepted by the portal.
    #[derive(Debug, PartialEq, Eq)]
    struct OutputRootProof {
        bytes32 version;
        bytes32 stateRoot;
        bytes32 messagePasserStorageRoot;
        bytes32 latestBlockhash;
    }

    /// Withdrawal commitment emitted by the L2 message passer.
    #[derive(Debug)]
    event MessagePassed(
        uint256 indexed nonce,
        address indexed sender,
        address indexed target,
        uint256 value,
        uint256 gasLimit,
        bytes data,
        bytes32 withdrawalHash
    );
}

/// Receipt extraction or withdrawal inclusion verification failed.
#[derive(Debug, Error)]
pub enum WithdrawalError {
    /// The receipt has no withdrawal from the message passer.
    #[error("receipt contains no withdrawals")]
    NoWithdrawals,
    /// The receipt is missing its inclusion block number.
    #[error("receipt has no block number")]
    MissingBlockNumber,
    /// A message passer event could not be decoded.
    #[error("invalid MessagePassed event: {0}")]
    DecodeEvent(#[source] alloy_sol_types::Error),
    /// The event does not commit to its withdrawal fields.
    #[error("withdrawal hash mismatch: emitted {emitted}, computed {computed}")]
    WithdrawalHashMismatch {
        /// The event's commitment.
        emitted: B256,
        /// The hash of the decoded transaction.
        computed: B256,
    },
    /// The RPC hash does not commit to the supplied header.
    #[error("header hash mismatch: rpc {rpc}, computed {computed}")]
    HeaderHashMismatch {
        /// The RPC-provided block hash.
        rpc: B256,
        /// The locally computed block hash.
        computed: B256,
    },
    /// The proof is for another account.
    #[error("proof address is not the L2 message passer")]
    ProofAddressMismatch,
    /// There must be one storage entry, not zero or multiple entries.
    #[error("expected one storage proof, got {0}")]
    StorageProofCount(usize),
    /// The storage entry is not the withdrawal's sentMessages slot.
    #[error("storage proof key does not match the withdrawal slot")]
    StorageKeyMismatch,
    /// The withdrawal is not marked as sent.
    #[error("storage proof value is not one")]
    StorageValueMismatch,
    /// The account proof does not authenticate the message passer account.
    #[error("account proof verification failed: {0}")]
    AccountProof(#[source] ProofVerificationError),
    /// The storage proof does not authenticate the sent withdrawal.
    #[error("storage proof verification failed: {0}")]
    StorageProof(#[source] ProofVerificationError),
    /// The authenticated state does not match the onchain output claim.
    #[error("output root mismatch: expected {expected}, computed {computed}")]
    OutputRootMismatch {
        /// The onchain claim.
        expected: B256,
        /// The output root computed from the authenticated state.
        computed: B256,
    },
}

/// A withdrawal and the L2 block in which its event was included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withdrawal {
    /// Arguments for the portal withdrawal transaction.
    pub transaction: WithdrawalTransaction,
    /// The receipt's inclusion block number.
    pub block_number: u64,
}

/// Authenticated proof arguments for the portal's prove call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedWithdrawalProof {
    /// The version-zero output root preimage.
    pub output_root_proof: OutputRootProof,
    /// RLP-encoded nodes proving the withdrawal's sentMessages entry.
    pub storage_proof: Vec<Bytes>,
}

impl Withdrawal {
    /// Extracts all message passer withdrawals in receipt log order, including deposit receipts.
    pub fn from_receipt(receipt: &BaseTransactionReceipt) -> Result<Vec<Self>, WithdrawalError> {
        let mut withdrawals = Vec::new();
        for log in &receipt.inner.inner.receipt.as_receipt().logs {
            if log.address() != Predeploys::L2_TO_L1_MESSAGE_PASSER
                || log.topics().first() != Some(&MessagePassed::SIGNATURE_HASH)
            {
                continue;
            }
            let event = MessagePassed::decode_log_validate(log.as_ref())
                .map_err(WithdrawalError::DecodeEvent)?
                .data;
            let withdrawal = Self {
                transaction: WithdrawalTransaction {
                    nonce: event.nonce,
                    sender: event.sender,
                    target: event.target,
                    value: event.value,
                    gasLimit: event.gasLimit,
                    data: event.data,
                },
                block_number: receipt
                    .inner
                    .block_number
                    .ok_or(WithdrawalError::MissingBlockNumber)?,
            };
            let computed = withdrawal.hash();
            if computed != event.withdrawalHash {
                return Err(WithdrawalError::WithdrawalHashMismatch {
                    emitted: event.withdrawalHash,
                    computed,
                });
            }
            withdrawals.push(withdrawal);
        }
        if withdrawals.is_empty() {
            return Err(WithdrawalError::NoWithdrawals);
        }
        Ok(withdrawals)
    }

    /// Hashes the ABI-encoded withdrawal fields.
    pub fn hash(&self) -> B256 {
        keccak256(self.transaction.abi_encode_params())
    }

    /// Computes the sentMessages mapping slot for this withdrawal hash.
    pub fn slot(&self) -> B256 {
        let mut preimage = [0; 64];
        preimage[..32].copy_from_slice(self.hash().as_slice());
        keccak256(preimage)
    }

    /// Authenticates the header and withdrawal inclusion against an onchain output claim.
    pub fn verify_proof(
        &self,
        header: &Header,
        proof: &EIP1186AccountProofResponse,
        expected_output_root: B256,
    ) -> Result<VerifiedWithdrawalProof, WithdrawalError> {
        let block_hash = header.inner.hash_slow();
        if block_hash != header.hash {
            return Err(WithdrawalError::HeaderHashMismatch {
                rpc: header.hash,
                computed: block_hash,
            });
        }
        if proof.address != Predeploys::L2_TO_L1_MESSAGE_PASSER {
            return Err(WithdrawalError::ProofAddressMismatch);
        }
        let [storage] = proof.storage_proof.as_slice() else {
            return Err(WithdrawalError::StorageProofCount(proof.storage_proof.len()));
        };
        let slot = self.slot();
        if storage.key.as_b256() != slot {
            return Err(WithdrawalError::StorageKeyMismatch);
        }
        if storage.value != U256::from(1) {
            return Err(WithdrawalError::StorageValueMismatch);
        }
        // The output root check binds the state and storage roots to the onchain claim before
        // walking attacker-supplied proof nodes: Alloy's verifier panics on some malformed
        // inline nodes that a self-consistent fake header could otherwise reach.
        let output_root = OutputRoot::from_parts(header.state_root, proof.storage_hash, block_hash);
        let computed = output_root.hash();
        if computed != expected_output_root {
            return Err(WithdrawalError::OutputRootMismatch {
                expected: expected_output_root,
                computed,
            });
        }
        let account = TrieAccount {
            nonce: proof.nonce,
            balance: proof.balance,
            storage_root: proof.storage_hash,
            code_hash: proof.code_hash,
        };
        verify_proof(
            header.state_root,
            Nibbles::unpack(keccak256(proof.address)),
            Some(alloy_rlp::encode(account)),
            &proof.account_proof,
        )
        .map_err(WithdrawalError::AccountProof)?;
        let storage_key = Nibbles::unpack(keccak256(slot));
        verify_proof(
            proof.storage_hash,
            storage_key,
            Some(alloy_rlp::encode(storage.value)),
            &storage.proof,
        )
        .map_err(WithdrawalError::StorageProof)?;
        let storage_proof =
            VerifiedWithdrawalProof::expand_inline_nodes(&storage.proof, &storage_key)
                .map_err(|error| WithdrawalError::StorageProof(error.into()))?;
        Ok(VerifiedWithdrawalProof {
            output_root_proof: OutputRootProof {
                version: output_root.version(),
                stateRoot: output_root.state_root,
                messagePasserStorageRoot: output_root.bridge_storage_root,
                latestBlockhash: output_root.block_hash,
            },
            storage_proof,
        })
    }
}

impl VerifiedWithdrawalProof {
    /// Appends nodes embedded in the proof's final node as separate proof elements.
    ///
    /// Trie nodes shorter than 32 bytes are inlined in their parent. Alloy verifies such proofs,
    /// but the portal's `MerkleTrie` expects every node on the path as its own proof element and
    /// reverts otherwise. Inlined nodes can only appear at the tail of a path.
    pub fn expand_inline_nodes(
        proof: &[Bytes],
        key: &Nibbles,
    ) -> Result<Vec<Bytes>, alloy_rlp::Error> {
        let mut expanded = proof.to_vec();
        let Some((last, parents)) = proof.split_last() else {
            return Ok(expanded);
        };
        let mut depth = 0;
        for node in parents {
            match TrieNode::decode(&mut &node[..])? {
                TrieNode::Branch(_) => depth += 1,
                TrieNode::Extension(extension) => depth += extension.key.len(),
                TrieNode::Leaf(_) | TrieNode::EmptyRoot => return Ok(expanded),
            }
        }
        let mut node = last.clone();
        loop {
            let child = match TrieNode::decode(&mut &node[..])? {
                TrieNode::Branch(branch) => {
                    let Some(nibble) = key.get(depth) else { break };
                    depth += 1;
                    branch
                        .as_ref()
                        .children()
                        .find_map(|(index, child)| (index == nibble).then_some(child).flatten())
                        .cloned()
                }
                TrieNode::Extension(extension) => {
                    depth += extension.key.len();
                    Some(extension.child)
                }
                TrieNode::Leaf(_) | TrieNode::EmptyRoot => None,
            };
            match child {
                Some(child) if !child.is_hash() => {
                    node = Bytes::copy_from_slice(&child);
                    expanded.push(node.clone());
                }
                _ => break,
            }
        }
        Ok(expanded)
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use alloy_consensus::TxReceipt;
    use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
    use alloy_sol_types::{SolEvent, SolValue};
    use alloy_trie::{HashBuilder, proof::ProofRetainer};
    use base_common_consensus::Predeploys;
    use serde::Deserialize;

    use super::{
        EIP1186AccountProofResponse, Header, MessagePassed, Nibbles, VerifiedWithdrawalProof,
        Withdrawal, WithdrawalError, WithdrawalTransaction, verify_proof,
    };

    #[derive(Deserialize)]
    struct Fixture {
        receipt: Receipt,
        header: Header,
        proof: EIP1186AccountProofResponse,
        game: Game,
    }

    #[derive(Deserialize)]
    struct Receipt {
        logs: Vec<base_common_rpc_types::BaseLogResponse>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Game {
        root_claim: alloy_primitives::B256,
    }

    fn fixture() -> Fixture {
        serde_json::from_str(include_str!("../fixtures/base-sepolia-withdrawal-proof.json"))
            .unwrap()
    }

    fn deposit_receipt() -> base_common_rpc_types::BaseTransactionReceipt {
        serde_json::from_str(include_str!("../fixtures/synthetic-deposit-withdrawal-receipt.json"))
            .unwrap()
    }

    fn withdrawal() -> Withdrawal {
        let log = &fixture().receipt.logs[0];
        let event = MessagePassed::decode_log(&log.inner.inner).unwrap().data;
        Withdrawal {
            transaction: WithdrawalTransaction {
                nonce: event.nonce,
                sender: event.sender,
                target: event.target,
                value: event.value,
                gasLimit: event.gasLimit,
                data: event.data,
            },
            block_number: 48_112_889,
        }
    }

    #[test]
    fn extracts_withdrawal_from_deposit_receipt_json() {
        let receipt = deposit_receipt();
        let withdrawals = Withdrawal::from_receipt(&receipt).unwrap();
        assert_eq!(withdrawals, vec![withdrawal()]);
    }

    #[test]
    fn ignores_message_passed_from_another_address_before_real_event() {
        let mut receipt = deposit_receipt();
        let mut impostor = receipt.inner.inner.receipt.logs()[0].clone();
        impostor.inner.inner.address = Address::repeat_byte(0x99);
        receipt.inner.inner.receipt.as_receipt_mut().logs.insert(0, impostor);
        assert_eq!(Withdrawal::from_receipt(&receipt).unwrap(), vec![withdrawal()]);
        receipt
            .inner
            .inner
            .receipt
            .as_receipt_mut()
            .logs
            .retain(|log| log.address() != Predeploys::L2_TO_L1_MESSAGE_PASSER);
        assert!(matches!(Withdrawal::from_receipt(&receipt), Err(WithdrawalError::NoWithdrawals)));
    }

    #[test]
    fn extracts_two_withdrawals_in_log_order() {
        let mut receipt = deposit_receipt();
        let mut event =
            MessagePassed::decode_log(&receipt.inner.inner.receipt.logs()[0].inner.inner)
                .unwrap()
                .data;
        event.nonce += U256::from(1);
        let mut second = withdrawal();
        second.transaction.nonce = event.nonce;
        event.withdrawalHash = second.hash();
        let mut log = receipt.inner.inner.receipt.logs()[0].clone();
        log.inner.inner.data = event.encode_log_data();
        receipt.inner.inner.receipt.as_receipt_mut().logs.push(log);
        assert_eq!(Withdrawal::from_receipt(&receipt).unwrap(), vec![withdrawal(), second]);
    }

    #[test]
    fn hash_and_slot_match_onchain_commitments() {
        let fixture = fixture();
        let event = MessagePassed::decode_log(&fixture.receipt.logs[0].inner.inner).unwrap().data;
        let withdrawal = withdrawal();
        assert_eq!(withdrawal.hash(), event.withdrawalHash);
        assert_eq!(withdrawal.slot(), fixture.proof.storage_proof[0].key.as_b256());
    }

    #[test]
    fn verifies_proof_against_onchain_claim() {
        let fixture = fixture();
        let verified = withdrawal()
            .verify_proof(&fixture.header, &fixture.proof, fixture.game.root_claim)
            .unwrap();
        assert_eq!(keccak256(verified.output_root_proof.abi_encode()), fixture.game.root_claim);
        assert_eq!(verified.storage_proof, fixture.proof.storage_proof[0].proof);
    }

    #[test]
    fn expands_inline_nodes_into_separate_proof_elements() {
        let mut keys = [Nibbles::unpack(B256::ZERO), Nibbles::unpack(B256::ZERO)];
        keys[1].set_at(63, 1);
        let value = alloy_rlp::encode(U256::from(1));
        let mut builder =
            HashBuilder::default().with_proof_retainer(ProofRetainer::from_iter([keys[1]]));
        for key in keys {
            builder.add_leaf(key, &value);
        }
        let root = builder.root();
        let portal_proof: Vec<Bytes> = builder
            .take_proof_nodes()
            .into_nodes_sorted()
            .into_iter()
            .map(|(_, node)| node)
            .collect();
        // eth_getProof omits nodes embedded in their parent.
        let rpc_proof: Vec<Bytes> =
            portal_proof.iter().filter(|node| node.len() >= 32).cloned().collect();
        verify_proof(root, keys[1], Some(value), &rpc_proof).unwrap();
        assert_eq!((rpc_proof.len(), portal_proof.len()), (1, 3));
        assert_eq!(
            VerifiedWithdrawalProof::expand_inline_nodes(&rpc_proof, &keys[1]).unwrap(),
            portal_proof
        );
    }

    #[test]
    fn rejects_tampered_rpc_header_hash() {
        let mut fixture = fixture();
        fixture.header.hash = alloy_primitives::B256::ZERO;
        assert!(matches!(
            withdrawal().verify_proof(&fixture.header, &fixture.proof, fixture.game.root_claim),
            Err(WithdrawalError::HeaderHashMismatch { .. })
        ));
    }

    #[test]
    fn rejects_wrong_output_claim() {
        let fixture = fixture();
        assert!(matches!(
            withdrawal().verify_proof(
                &fixture.header,
                &fixture.proof,
                alloy_primitives::B256::ZERO
            ),
            Err(WithdrawalError::OutputRootMismatch { .. })
        ));
    }

    #[test]
    fn rejects_incomplete_account_proof() {
        let mut fixture = fixture();
        fixture.proof.account_proof.pop();
        assert!(matches!(
            withdrawal().verify_proof(&fixture.header, &fixture.proof, fixture.game.root_claim),
            Err(WithdrawalError::AccountProof(_))
        ));
    }

    #[test]
    fn rejects_incomplete_storage_proof() {
        let mut fixture = fixture();
        fixture.proof.storage_proof[0].proof.pop();
        assert!(matches!(
            withdrawal().verify_proof(&fixture.header, &fixture.proof, fixture.game.root_claim),
            Err(WithdrawalError::StorageProof(_))
        ));
    }

    #[test]
    fn rejects_unsent_withdrawal() {
        let mut fixture = fixture();
        fixture.proof.storage_proof[0].value = U256::ZERO;
        assert!(matches!(
            withdrawal().verify_proof(&fixture.header, &fixture.proof, fixture.game.root_claim),
            Err(WithdrawalError::StorageValueMismatch)
        ));
    }

    #[test]
    fn rejects_malformed_message_passer_event_with_source() {
        let mut receipt = deposit_receipt();
        receipt.inner.inner.receipt.as_receipt_mut().logs[0].inner.inner.data.data = Bytes::new();
        let error = Withdrawal::from_receipt(&receipt).unwrap_err();
        assert!(matches!(error, WithdrawalError::DecodeEvent(_)));
        assert!(error.source().is_some());
    }

    #[test]
    fn rejects_event_with_wrong_commitment() {
        let mut receipt = deposit_receipt();
        let log = &mut receipt.inner.inner.receipt.as_receipt_mut().logs[0];
        let mut event = MessagePassed::decode_log(&log.inner.inner).unwrap().data;
        event.withdrawalHash = B256::ZERO;
        log.inner.inner.data = event.encode_log_data();
        assert!(matches!(
            Withdrawal::from_receipt(&receipt),
            Err(WithdrawalError::WithdrawalHashMismatch { .. })
        ));
    }

    #[test]
    fn ignores_other_events_and_empty_receipts() {
        let mut receipt = deposit_receipt();
        receipt.inner.inner.receipt.as_receipt_mut().logs[0].inner.inner.data = Default::default();
        assert!(matches!(Withdrawal::from_receipt(&receipt), Err(WithdrawalError::NoWithdrawals)));
        receipt.inner.inner.receipt.as_receipt_mut().logs.clear();
        assert!(matches!(Withdrawal::from_receipt(&receipt), Err(WithdrawalError::NoWithdrawals)));
    }

    #[test]
    fn requires_inclusion_block_number() {
        let mut receipt = deposit_receipt();
        receipt.inner.block_number = None;
        assert!(matches!(
            Withdrawal::from_receipt(&receipt),
            Err(WithdrawalError::MissingBlockNumber)
        ));
        receipt.inner.block_number = Some(u64::MAX);
        assert_eq!(Withdrawal::from_receipt(&receipt).unwrap()[0].block_number, u64::MAX);
    }

    #[test]
    fn rejects_another_account() {
        let mut fixture = fixture();
        fixture.proof.address = Address::ZERO;
        assert!(matches!(
            withdrawal().verify_proof(&fixture.header, &fixture.proof, fixture.game.root_claim),
            Err(WithdrawalError::ProofAddressMismatch)
        ));
    }

    #[test]
    fn requires_exactly_one_storage_proof() {
        let mut fixture = fixture();
        fixture.proof.storage_proof.push(fixture.proof.storage_proof[0].clone());
        assert!(matches!(
            withdrawal().verify_proof(&fixture.header, &fixture.proof, fixture.game.root_claim),
            Err(WithdrawalError::StorageProofCount(2))
        ));
        fixture.proof.storage_proof.clear();
        assert!(matches!(
            withdrawal().verify_proof(&fixture.header, &fixture.proof, fixture.game.root_claim),
            Err(WithdrawalError::StorageProofCount(0))
        ));
    }

    #[test]
    fn rejects_another_storage_key() {
        let mut fixture = fixture();
        fixture.proof.storage_proof[0].key = B256::ZERO.into();
        assert!(matches!(
            withdrawal().verify_proof(&fixture.header, &fixture.proof, fixture.game.root_claim),
            Err(WithdrawalError::StorageKeyMismatch)
        ));
    }
}
