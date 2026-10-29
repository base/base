//! Engine-neutral EIP-8130 phased call dispatch and gas-limit estimation search.
//!
//! `calls` is a two-level structure (`Vec<Vec<Call>>`): an ordered list of **phases**, each an
//! ordered list of calls. Phases draw from a single gas pool and commit independently in sequence;
//! the calls within a phase are atomic (all-or-nothing). If any call in a phase reverts (or is
//! blocked by the policy gate), that phase's state changes are discarded and every later phase is
//! skipped, but the gas already consumed is still charged and the transaction is still included.
//!
//! The engine supplies the state primitives through [`Eip8130CallHost`]; the phase semantics live
//! here once so every engine dispatches calls identically.

use alloy_primitives::{Address, Bytes, U256};
use base_common_consensus::{Call, Eip8130Signed};

use crate::{Eip8130Error, Eip8130GasSchedule, Eip8130Outcome, Eip8130Pipeline};

/// Maximum number of bisection steps the gas-limit search runs before returning the tightest
/// verified-feasible pool it has found. The [`POOL_SEARCH_TOLERANCE_PER_MILLE`] early exit
/// normally terminates the search in far fewer steps; this is a hard backstop against
/// pathological ranges.
const POOL_SEARCH_MAX_ITERS: u32 = 16;

/// Early-exit tolerance (in parts-per-thousand) for the gas-limit search: once the
/// `(highest - lowest)` window is within this fraction of `highest`, the search stops and returns
/// `highest` (a verified-feasible pool). Mirrors the standard reth/geth estimator's 1.5%
/// `ESTIMATE_GAS_ERROR_RATIO`.
const POOL_SEARCH_TOLERANCE_PER_MILLE: u64 = 15;

/// The settled outcome of one top-level EIP-8130 call frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Eip8130CallFrame {
    /// Whether the frame succeeded (neither reverted nor halted).
    pub success: bool,
    /// Gas the frame consumed. A frame that halts exceptionally consumes its whole allotment.
    pub gas_spent: u64,
    /// The frame's signed SSTORE refund counter (only counted for successful frames).
    pub refund: i64,
    /// The frame's return or revert data.
    pub output: Bytes,
}

/// The state primitives an engine provides to dispatch EIP-8130 calls.
pub trait Eip8130CallHost {
    /// A journal position phases revert to.
    type Checkpoint;
    /// An engine error that aborts the transaction (database or fatal failure).
    type Error;

    /// Opens a journal checkpoint for a phase.
    fn checkpoint(&mut self) -> Self::Checkpoint;

    /// Reverts every state change (and log) made since `checkpoint`.
    fn revert(&mut self, checkpoint: Self::Checkpoint);

    /// Keeps the state changes made since `checkpoint`. They stay revertible as part of the
    /// transaction until it is committed.
    fn commit(&mut self, checkpoint: Self::Checkpoint);

    /// EIP-2929 account-access gas for dispatching a root call to `to`, as the `CALL` opcode
    /// would charge it: the cold account access cost if `to` is cold or the warm access cost if it
    /// is warm, plus the same again for the delegation target if `to` carries an EIP-7702
    /// delegation. Loading warms both accounts.
    fn root_call_access_gas(&mut self, to: Address) -> Result<u64, Self::Error>;

    /// Whether a call moving `value` from `sender` to `to` creates `to`, so it owes
    /// [`Eip8130GasSchedule::NEW_ACCOUNT_COST`]: `value` is non-zero, `to` is not `sender`, `to`
    /// is empty (EIP-161), and `sender` can cover `value` (an unaffordable call fails its balance
    /// check before the charge applies).
    fn creates_account(
        &mut self,
        sender: Address,
        to: Address,
        value: U256,
    ) -> Result<bool, Self::Error>;

    /// Dispatches `call` from `caller` as a top-level call frame with `gas_limit` and runs it to
    /// completion with `tx.origin == caller` and `msg.value == call.value`. The frame starts with
    /// empty memory; a `value` the caller cannot afford reverts the frame like any other `CALL`.
    fn run_call(
        &mut self,
        caller: Address,
        call: &Call,
        gas_limit: u64,
    ) -> Result<Eip8130CallFrame, Self::Error>;

    /// Wraps a transaction-level rejection into the engine error.
    fn invalid(error: Eip8130Error) -> Self::Error;
}

/// The result of dispatching an EIP-8130 transaction's `calls`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Eip8130CallsResult {
    /// Total regular gas consumed by dispatched calls (across all phases, including the gas spent
    /// by a reverting phase up to its revert).
    pub call_gas_spent: u64,
    /// Net gas refund accumulated across all committed phases, pre-cap. This is the standard
    /// transaction-level refund counter: every call's refund (which may be negative) is summed
    /// signed across the whole transaction, so offsetting SSTORE refunds cancel exactly as they
    /// would under a single continuous EVM execution. Clamped and EIP-3529-capped once, by
    /// [`Eip8130Outcome::billable_gas`].
    pub refund: i64,
    /// `true` if any phase reverted (or was blocked by the policy gate); later phases are then
    /// skipped.
    pub reverted: bool,
    /// The return data of the call that reverted the transaction (or the `ActorPolicyViolation`
    /// payload for a policy-gate block), or on success the return data of the last call (empty
    /// when `calls` was empty), which is what an EIP-8130 `eth_call` returns.
    pub output: Bytes,
    /// Number of leading phases that committed. Phases run in order and stop at the first revert,
    /// so every phase at or after this index reverted or was skipped. This is the EIP-8130
    /// receipt's `committedPhases`.
    pub committed_phases: u64,
}

impl Eip8130CallsResult {
    /// The net gas the transaction is billed for these calls.
    #[must_use]
    pub fn billable_gas(&self, outcome: &Eip8130Outcome) -> u64 {
        outcome.billable_gas(self.call_gas_spent, self.refund)
    }

    /// The gross metered gas of these calls (refunds excluded), as reported by simulation.
    #[must_use]
    pub const fn gross_gas(&self, outcome: &Eip8130Outcome) -> u64 {
        outcome.metered_gas(outcome.sender_intrinsic.saturating_add(self.call_gas_spent))
    }
}

/// Engine-neutral EIP-8130 call dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Eip8130Calls;

impl Eip8130Calls {
    /// Dispatches the transaction's `calls` phase by phase from a single gas `pool`.
    ///
    /// Each phase runs under a checkpoint: a successful phase commits and its gas refund counts; a
    /// reverting phase (or one blocked by the policy gate) rolls back, is charged for the gas
    /// already consumed without refund, and skips every later phase.
    ///
    /// When the authenticating actor is policy-gated, every `call.to` must equal the resolved
    /// policy target: a mismatched call is not dispatched and fails its phase deterministically
    /// with `ActorPolicyViolation`, charging no call gas for it. Each dispatched call first draws
    /// its root account-access gas and, when it creates its target, the new-account charge from
    /// the pool; a charge that does not fit fails the phase without consuming the unspent balance.
    ///
    /// `pool` is the gas available to the calls (`gas_limit - sender_intrinsic` for execution; the
    /// gas-limit search probes candidate pools).
    pub fn execute<H: Eip8130CallHost>(
        host: &mut H,
        signed: &Eip8130Signed,
        outcome: &Eip8130Outcome,
        pool: u64,
    ) -> Result<Eip8130CallsResult, H::Error> {
        let mut remaining = pool;
        let mut refund: i64 = 0;
        let mut committed_phases: u64 = 0;
        let mut last_output = Bytes::new();

        for phase in &signed.tx().calls {
            let checkpoint = host.checkpoint();
            let mut phase_refund: i64 = 0;
            let mut phase_failure: Option<Bytes> = None;

            for call in phase {
                if outcome.policy_gated && call.to != outcome.policy_target {
                    phase_failure = Some(Eip8130Pipeline::actor_policy_violation_data(
                        outcome.sender_actor_id,
                        call.to,
                    ));
                    break;
                }

                let access_gas = host.root_call_access_gas(call.to)?;
                let Some(left) = remaining.checked_sub(access_gas) else {
                    phase_failure = Some(Bytes::new());
                    break;
                };
                remaining = left;
                if host.creates_account(outcome.sender, call.to, call.value)? {
                    let Some(left) = remaining.checked_sub(Eip8130GasSchedule::NEW_ACCOUNT_COST)
                    else {
                        phase_failure = Some(Bytes::new());
                        break;
                    };
                    remaining = left;
                }

                let frame = host.run_call(outcome.sender, call, remaining)?;
                // The frame is capped at `remaining`, so overspending violates an EVM
                // invariant: a hard error rather than a silent clamp.
                remaining = remaining.checked_sub(frame.gas_spent).ok_or_else(|| {
                    H::invalid(Eip8130Error::invalid(
                        "EIP-8130 call consumed more gas than the phase pool contained",
                    ))
                })?;
                if frame.success {
                    phase_refund = phase_refund.saturating_add(frame.refund);
                    last_output = frame.output;
                } else {
                    phase_failure = Some(frame.output);
                    break;
                }
            }

            if let Some(output) = phase_failure {
                host.revert(checkpoint);
                return Ok(Eip8130CallsResult {
                    call_gas_spent: pool.saturating_sub(remaining),
                    refund,
                    reverted: true,
                    output,
                    committed_phases,
                });
            }

            host.commit(checkpoint);
            committed_phases += 1;
            refund = refund.saturating_add(phase_refund);
        }

        Ok(Eip8130CallsResult {
            call_gas_spent: pool.saturating_sub(remaining),
            refund,
            reverted: false,
            output: last_output,
            committed_phases,
        })
    }

    /// Searches for the minimum call pool at which the `calls` still succeed, given that they
    /// already succeeded at `ceiling_pool` consuming `ceiling_spent`. Returns a verified-feasible
    /// pool in `[ceiling_spent, ceiling_pool]`.
    ///
    /// `reverts_at(pool)` probes the calls at `pool` (reverting their writes) and reports whether
    /// they reverted. Fast path: the measured spend is usually itself feasible (no gas lost to
    /// EIP-150 forwarding), so a single probe at `ceiling_spent` resolves it. Only when that probe
    /// fails does it bisect upward, seeded with the standard `64/63` optimistic guess and bounded
    /// by [`POOL_SEARCH_MAX_ITERS`] and the [`POOL_SEARCH_TOLERANCE_PER_MILLE`] early exit. Every
    /// bound assigned to `highest` is a probe-verified success, so the returned value always
    /// succeeds.
    pub fn search_estimate_pool<E>(
        ceiling_spent: u64,
        ceiling_pool: u64,
        mut reverts_at: impl FnMut(u64) -> Result<bool, E>,
    ) -> Result<u64, E> {
        let spent = ceiling_spent.min(ceiling_pool);
        if spent >= ceiling_pool || !reverts_at(spent)? {
            return Ok(spent);
        }

        let mut lowest = spent;
        let mut highest = ceiling_pool;

        let seed = spent.saturating_mul(64) / 63;
        if seed > lowest && seed < highest {
            if reverts_at(seed)? {
                lowest = seed;
            } else {
                highest = seed;
            }
        }

        let mut iters = 0;
        while lowest + 1 < highest && iters < POOL_SEARCH_MAX_ITERS {
            if (highest - lowest).saturating_mul(1000)
                <= highest.saturating_mul(POOL_SEARCH_TOLERANCE_PER_MILLE)
            {
                break;
            }
            let mid = lowest + (highest - lowest) / 2;
            if reverts_at(mid)? {
                lowest = mid;
            } else {
                highest = mid;
            }
            iters += 1;
        }

        Ok(highest)
    }
}

#[cfg(test)]
mod tests {
    use core::convert::Infallible;

    use base_common_consensus::TxEip8130;

    use super::*;

    const THRESHOLD: u64 = 100_000;

    #[test]
    fn search_returns_the_measured_spend_when_it_suffices() {
        let pool = Eip8130Calls::search_estimate_pool::<Infallible>(50_000, 1_000_000, |pool| {
            Ok(pool < 50_000)
        });
        assert_eq!(pool, Ok(50_000));
    }

    #[test]
    fn search_finds_a_feasible_pool_within_tolerance() {
        let pool = Eip8130Calls::search_estimate_pool::<Infallible>(60_000, 1_000_000, |pool| {
            Ok(pool < THRESHOLD)
        })
        .unwrap();
        assert!(pool >= THRESHOLD, "returned pool {pool} must succeed");
        assert!(pool <= THRESHOLD + THRESHOLD * POOL_SEARCH_TOLERANCE_PER_MILLE / 500);
    }

    /// Records the host interactions and scripts frame outcomes by call target.
    #[derive(Default)]
    struct ScriptedHost {
        reverting_target: Option<Address>,
        reverted_phases: u32,
        committed_phases: u32,
    }

    impl Eip8130CallHost for ScriptedHost {
        type Checkpoint = ();
        type Error = Eip8130Error;

        fn checkpoint(&mut self) {}

        fn revert(&mut self, (): ()) {
            self.reverted_phases += 1;
        }

        fn commit(&mut self, (): ()) {
            self.committed_phases += 1;
        }

        fn root_call_access_gas(&mut self, _to: Address) -> Result<u64, Self::Error> {
            Ok(100)
        }

        fn creates_account(
            &mut self,
            _: Address,
            _: Address,
            _: U256,
        ) -> Result<bool, Self::Error> {
            Ok(false)
        }

        fn run_call(
            &mut self,
            _caller: Address,
            call: &Call,
            _gas_limit: u64,
        ) -> Result<Eip8130CallFrame, Self::Error> {
            let success = Some(call.to) != self.reverting_target;
            Ok(Eip8130CallFrame {
                success,
                gas_spent: 1_000,
                refund: 10,
                output: call.data.clone(),
            })
        }

        fn invalid(error: Eip8130Error) -> Self::Error {
            error
        }
    }

    fn signed(calls: Vec<Vec<Call>>) -> Eip8130Signed {
        Eip8130Signed::new(TxEip8130 { calls, ..Default::default() }, Bytes::new(), Bytes::new())
    }

    fn call(to: u8) -> Call {
        Call { to: Address::repeat_byte(to), value: U256::ZERO, data: Bytes::from(vec![to]) }
    }

    fn outcome() -> Eip8130Outcome {
        Eip8130Outcome {
            sender: Address::ZERO,
            payer: Address::ZERO,
            sender_actor_id: Default::default(),
            policy_gated: false,
            policy_target: Address::ZERO,
            gas_limit: 0,
            sender_intrinsic: 0,
            sender_floor: 0,
            payer_auth: 0,
            payer_auth_standard: 0,
            execution_gas_available: 0,
            effective: 0,
            base_fee: 0,
            bump_protocol_nonce: false,
        }
    }

    #[test]
    fn a_reverting_phase_skips_later_phases_and_keeps_its_gas() {
        let mut host =
            ScriptedHost { reverting_target: Some(Address::repeat_byte(2)), ..Default::default() };
        let tx = signed(vec![vec![call(1)], vec![call(2), call(3)], vec![call(4)]]);
        let result = Eip8130Calls::execute(&mut host, &tx, &outcome(), 1_000_000).unwrap();
        assert!(result.reverted);
        assert_eq!(result.committed_phases, 1);
        // Phase 1 (1 call) and the reverting call of phase 2 were charged; phase 3 never ran.
        assert_eq!(result.call_gas_spent, 2 * 1_100);
        assert_eq!(result.refund, 10, "only the committed phase's refund counts");
        assert_eq!(result.output, Bytes::from(vec![2]));
        assert_eq!((host.committed_phases, host.reverted_phases), (1, 1));
    }

    #[test]
    fn a_policy_gated_call_to_another_target_fails_its_phase_without_gas() {
        let mut host = ScriptedHost::default();
        let mut outcome = outcome();
        outcome.policy_gated = true;
        outcome.policy_target = Address::repeat_byte(1);
        let tx = signed(vec![vec![call(1)], vec![call(9)]]);
        let result = Eip8130Calls::execute(&mut host, &tx, &outcome, 1_000_000).unwrap();
        assert!(result.reverted);
        assert_eq!(result.committed_phases, 1);
        assert_eq!(result.call_gas_spent, 1_100);
        assert_eq!(
            result.output,
            Eip8130Pipeline::actor_policy_violation_data(outcome.sender_actor_id, call(9).to)
        );
    }

    #[test]
    fn an_unaffordable_access_charge_fails_the_phase_without_burning_the_pool() {
        let mut host = ScriptedHost::default();
        let tx = signed(vec![vec![call(1)]]);
        let result = Eip8130Calls::execute(&mut host, &tx, &outcome(), 99).unwrap();
        assert!(result.reverted);
        assert_eq!(result.call_gas_spent, 0);
    }
}
