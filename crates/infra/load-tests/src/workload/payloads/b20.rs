//! B-20 precompile token transfer payload for load testing.

use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_rpc_types::TransactionRequest;
use alloy_sol_types::SolCall;
use base_common_precompiles::{B20Variant, IB20};

use super::Payload;
use crate::workload::SeededRng;

/// Derives the deterministic B-20 token salt for a sender within a single run.
///
/// The preimage is `sender (20 bytes) ‖ run_salt (32 bytes)`, so the salt is unique per
/// (sender, run). The fresh per-run `run_salt` ensures a re-run with the same sender set
/// creates brand-new tokens instead of colliding with the previous run's tokens (which would
/// revert `createB20` with `TokenAlreadyExists`).
///
/// Setup, the transfer payload, and teardown all derive the token address through this helper so
/// they cannot drift.
pub(crate) fn b20_salt_for(sender: Address, run_salt: B256) -> B256 {
    let mut preimage = [0u8; 52];
    preimage[..20].copy_from_slice(sender.as_slice());
    preimage[20..].copy_from_slice(run_salt.as_slice());
    keccak256(preimage)
}

/// Derives the deterministic B-20 ASSET token address a sender owns for this run.
///
/// Equivalent to the factory's address derivation: each sender is the creator of its own token,
/// so the address is `B20Variant::Asset.compute_address(sender, b20_salt_for(sender, run_salt))`.
pub(crate) fn b20_token_for(sender: Address, run_salt: B256) -> Address {
    B20Variant::Asset.compute_address(sender, b20_salt_for(sender, run_salt)).0
}

/// Generates B-20 precompile token transfer transactions.
///
/// During the load phase, each generated transaction calls `transfer(address,uint256)` on the
/// sender's own B-20 token precompile. The B-20 `transfer` selector is ERC-20 compatible, so this
/// exercises the precompile's state-mutation code path (balance updates, event emission) under
/// sustained load.
///
/// The token is derived per sender from the run's salt rather than stored, so a single payload
/// instance serves every sender's own token without a sender→token map.
#[derive(Debug, Clone)]
pub struct B20TransferPayload {
    /// Per-run salt seed used to derive each sender's own token address.
    pub run_salt: B256,
    /// Minimum transfer amount.
    pub min_amount: U256,
    /// Maximum transfer amount.
    pub max_amount: U256,
}

impl B20TransferPayload {
    /// Creates a new B-20 transfer payload bound to a run's token salt.
    pub const fn new(run_salt: B256, min_amount: U256, max_amount: U256) -> Self {
        Self { run_salt, min_amount, max_amount }
    }
}

impl Payload for B20TransferPayload {
    fn name(&self) -> &'static str {
        "b20"
    }

    fn generate(&self, rng: &mut SeededRng, from: Address, to: Address) -> TransactionRequest {
        let amount = if self.min_amount == self.max_amount {
            self.min_amount
        } else {
            let min: u128 = self.min_amount.try_into().expect("b20 min_amount must fit in u128");
            let max: u128 = self.max_amount.try_into().expect("b20 max_amount must fit in u128");
            U256::from(rng.gen_range(min..=max))
        };

        // Each sender transfers its OWN token; derive the token from the sender, not a fixed
        // address, so any sender produces a valid transfer against the token it minted at setup.
        let token = b20_token_for(from, self.run_salt);
        let call = IB20::transferCall { to, amount };

        TransactionRequest::default()
            .with_to(token)
            .with_input(Bytes::from(call.abi_encode()))
            .with_gas_limit(100_000)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    #[test]
    fn token_is_deterministic_per_sender_and_run() {
        let sender = address!("00000000000000000000000000000000000000a1");
        let run_salt = B256::repeat_byte(0x11);

        // Same (sender, run_salt) always yields the same salt and token.
        assert_eq!(b20_salt_for(sender, run_salt), b20_salt_for(sender, run_salt));
        assert_eq!(b20_token_for(sender, run_salt), b20_token_for(sender, run_salt));
    }

    #[test]
    fn different_senders_get_different_tokens() {
        let run_salt = B256::repeat_byte(0x22);
        let sender_a = address!("00000000000000000000000000000000000000a1");
        let sender_b = address!("00000000000000000000000000000000000000b2");

        assert_ne!(
            b20_token_for(sender_a, run_salt),
            b20_token_for(sender_b, run_salt),
            "distinct senders must own distinct tokens"
        );
    }

    #[test]
    fn different_run_salts_get_different_tokens() {
        let sender = address!("00000000000000000000000000000000000000a1");
        let run_salt_1 = B256::repeat_byte(0x33);
        let run_salt_2 = B256::repeat_byte(0x44);

        assert_ne!(
            b20_token_for(sender, run_salt_1),
            b20_token_for(sender, run_salt_2),
            "a fresh run salt must produce a fresh token for the same sender"
        );
    }

    #[test]
    fn generate_targets_senders_own_token() {
        let run_salt = B256::repeat_byte(0x55);
        let sender = address!("00000000000000000000000000000000000000a1");
        let recipient = address!("00000000000000000000000000000000000000b2");
        let payload = B20TransferPayload::new(run_salt, U256::from(1000), U256::from(1000));
        let mut rng = SeededRng::new(7);

        let tx = payload.generate(&mut rng, sender, recipient);

        assert_eq!(
            tx.to,
            Some(alloy_primitives::TxKind::Call(b20_token_for(sender, run_salt))),
            "transfer must target the sender's own token"
        );

        let expected = IB20::transferCall { to: recipient, amount: U256::from(1000) };
        assert_eq!(
            tx.input.input().expect("input set").as_ref(),
            expected.abi_encode().as_slice(),
            "calldata must be a transfer of the chosen amount to the recipient"
        );
    }
}
