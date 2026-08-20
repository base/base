use base_protocol::{AttributesWithParent, L2BlockInfo};
use derive_more::PartialEq;
use thiserror::Error;

use crate::Metrics;

/// The possible states of the [`DerivationStateMachine`] implemented by the
/// [`crate::DerivationActor`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DerivationState {
    /// The [`crate::DerivationActor`] is waiting for notification that the EL sync has completed
    /// before it can start derivation.
    AwaitingELSyncCompletion,
    /// The [`crate::DerivationActor`] is idle awaiting data.
    AwaitingL1Data,
    /// [`base_protocol::AttributesWithParent`] were sent to the [`crate::EngineActor`], and the
    /// [`crate::DerivationActor`] is waiting for confirmation that they were processed into a safe
    /// head.
    AwaitingSafeHeadConfirmation,
    /// A reorg or some other inconsistency was detected, necessitating a [`base_consensus_derive::Signal`] to
    /// be processed before continuing derivation.
    AwaitingSignal,
    /// After receiving a [`base_consensus_derive::Signal`], we need an update of L1 data or a new engine
    /// safe head to start deriving again. This represents the state waiting for one of the two.
    AwaitingUpdateAfterSignal,
    /// The [`crate::DerivationActor`] is actively attempting derivation.
    Deriving,
}

/// The possible updates of the [`DerivationStateMachine`] implemented by the
/// [`crate::DerivationActor`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DerivationStateUpdate {
    /// The initial EL sync has completed along with the current safe head, allowing derivation to
    /// start.
    ELSyncCompleted(Box<L2BlockInfo>),
    /// More L1 data has become available to process.
    L1DataReceived,
    /// Further derivation is not possible without additional L1 data becoming available.
    MoreDataNeeded,
    /// Derivation has produced new [`base_protocol::AttributesWithParent`].
    NewAttributesDerived(Box<AttributesWithParent>),
    /// The EL has confirmed the derived [`base_protocol::AttributesWithParent`] as the new safe
    /// head.
    NewAttributesConfirmed(Box<L2BlockInfo>),
    /// A [`base_consensus_derive::Signal`] is necessary to update the derivation pipeline in order to
    /// continue.
    SignalNeeded,
    /// A [`base_consensus_derive::Signal`] has been received and processed.
    SignalProcessed,
}

/// An error processing a [`DerivationStateMachine`] state transition.
#[derive(Debug, Error)]
pub enum DerivationStateTransitionError {
    /// An invalid state transition was attempted.
    #[error("Invalid state transition, starting state: {state:?}, state_update: {update:?}.")]
    InvalidTransition {
        /// The [`DerivationState`] from which an invalid transition was attempted.
        state: DerivationState,
        /// The [`DerivationStateUpdate`] that is invalid from the [`DerivationState`].
        update: DerivationStateUpdate,
    },
}

// Details all valid state transitions.
fn transition(
    state: &DerivationState,
    update: &DerivationStateUpdate,
) -> Result<DerivationState, DerivationStateTransitionError> {
    match state {
        // NB: initial state. Once we transition away from this, we never go back.
        DerivationState::AwaitingELSyncCompletion => match update {
            DerivationStateUpdate::ELSyncCompleted(_) => Ok(DerivationState::Deriving),
            DerivationStateUpdate::NewAttributesConfirmed(_)
            | DerivationStateUpdate::SignalProcessed
            | DerivationStateUpdate::L1DataReceived => {
                Ok(DerivationState::AwaitingELSyncCompletion)
            }
            _ => Err(DerivationStateTransitionError::InvalidTransition {
                state: *state,
                update: update.clone(),
            }),
        },
        DerivationState::AwaitingL1Data => match update {
            DerivationStateUpdate::L1DataReceived => Ok(DerivationState::Deriving),
            DerivationStateUpdate::SignalProcessed => {
                Ok(DerivationState::AwaitingUpdateAfterSignal)
            }
            // The engine pushes a safe-head confirmation on every safe-head advance (e.g. an
            // out-of-lockstep L1 consolidation catch-up), not only in response to attributes we
            // submitted. Absorb it without leaving the wait for L1 data; `update` advances
            // `confirmed_safe_head` when the head moves forward.
            DerivationStateUpdate::NewAttributesConfirmed(_) => Ok(DerivationState::AwaitingL1Data),
            _ => Err(DerivationStateTransitionError::InvalidTransition {
                state: *state,
                update: update.clone(),
            }),
        },
        DerivationState::AwaitingSafeHeadConfirmation => match update {
            DerivationStateUpdate::NewAttributesConfirmed(_) => Ok(DerivationState::Deriving),
            DerivationStateUpdate::SignalProcessed => {
                Ok(DerivationState::AwaitingUpdateAfterSignal)
            }
            DerivationStateUpdate::L1DataReceived => {
                Ok(DerivationState::AwaitingSafeHeadConfirmation)
            }
            _ => Err(DerivationStateTransitionError::InvalidTransition {
                state: *state,
                update: update.clone(),
            }),
        },
        DerivationState::AwaitingSignal => match update {
            DerivationStateUpdate::SignalProcessed => {
                Ok(DerivationState::AwaitingUpdateAfterSignal)
            }
            DerivationStateUpdate::L1DataReceived | DerivationStateUpdate::MoreDataNeeded => {
                Ok(DerivationState::AwaitingSignal)
            }
            // Same rationale as `AwaitingL1Data`: absorb out-of-lockstep engine safe-head
            // confirmations while waiting for the pending reset signal instead of crashing.
            DerivationStateUpdate::NewAttributesConfirmed(_) => Ok(DerivationState::AwaitingSignal),
            _ => Err(DerivationStateTransitionError::InvalidTransition {
                state: *state,
                update: update.clone(),
            }),
        },
        DerivationState::AwaitingUpdateAfterSignal => match update {
            DerivationStateUpdate::L1DataReceived
            | DerivationStateUpdate::NewAttributesConfirmed(_) => Ok(DerivationState::Deriving),
            DerivationStateUpdate::SignalProcessed => {
                Ok(DerivationState::AwaitingUpdateAfterSignal)
            }
            _ => Err(DerivationStateTransitionError::InvalidTransition {
                state: *state,
                update: update.clone(),
            }),
        },
        DerivationState::Deriving => match update {
            DerivationStateUpdate::NewAttributesDerived(_) => {
                Ok(DerivationState::AwaitingSafeHeadConfirmation)
            }
            DerivationStateUpdate::SignalNeeded => Ok(DerivationState::AwaitingSignal),
            DerivationStateUpdate::MoreDataNeeded => Ok(DerivationState::AwaitingL1Data),
            _ => Err(DerivationStateTransitionError::InvalidTransition {
                state: *state,
                update: update.clone(),
            }),
        },
    }
}

/// The state machine that controls the state of the [`crate::DerivationActor`].
/// This machine enforces the following conditions:
///
/// ## General prerequisites:
/// 1. Derivation may not occur until EL sync has completed
/// 2. Derivation may not happen until the Engine L2 safe head is known
///
/// ## Derive -> Message EL -> Receive confirmation
/// When new [`base_protocol::AttributesWithParent`] are derived, they must be sent to the EL,
/// and the EL must confirm them by creating a new L2 safe head from them prior to further
/// derivation. There will be at most one [`base_protocol::AttributesWithParent`] awaiting
/// confirmation at any given time.
///
/// ## Signal handling
/// Certain conditions require a [`base_consensus_derive::Signal`] to be processed by the
/// [`base_consensus_derive::Pipeline`], updating derivation state before continuing derivation. This struct
/// allows a caller to register that it is waiting on a signal as well as mark that it was
/// processed.
#[derive(Debug)]
pub struct DerivationStateMachine {
    /// The last safe head confirmed by the engine, which is the base of the current derivation
    pub confirmed_safe_head: L2BlockInfo,
    /// The derivation state.
    pub state: DerivationState,
}

impl Default for DerivationStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl DerivationStateMachine {
    /// Constructs a new [`DerivationStateMachine`].
    fn new() -> Self {
        Self {
            confirmed_safe_head: L2BlockInfo::default(),
            state: DerivationState::AwaitingELSyncCompletion,
        }
    }

    /// Gets the current [`DerivationState`] of the state machine.
    pub const fn current_state(&self) -> DerivationState {
        self.state
    }

    /// Gets the last [`L2BlockInfo`] confirmed by the engine.
    pub const fn last_confirmed_safe_head(&self) -> L2BlockInfo {
        self.confirmed_safe_head
    }

    /// Applies the provided  [`DerivationStateUpdate`], returning an
    /// [`DerivationStateTransitionError`] if the state transition was invalid.
    pub fn update(
        &mut self,
        state_update: &DerivationStateUpdate,
    ) -> Result<(), DerivationStateTransitionError> {
        let prev_state = self.state;

        debug!(target: "derivation", state=?self.state, ?state_update, "Executing derivation state update.");
        self.state = transition(&self.state, state_update)?;

        match state_update {
            DerivationStateUpdate::NewAttributesConfirmed(safe_head) => {
                // In `AwaitingL1Data`/`AwaitingSignal` the confirmation is an out-of-lockstep
                // advance pushed by the engine, not a response to attributes we submitted. Only
                // ever move the safe head forward here: a non-advancing head in these states means
                // a reset signal was missing or reordered (legitimate backwards heads arrive via
                // `AwaitingUpdateAfterSignal` after a reset). Surface it, but never regress or
                // crash. In all other states the confirmation is applied unconditionally, which
                // preserves the reset path where the safe head correctly moves backwards.
                let absorbing = matches!(
                    prev_state,
                    DerivationState::AwaitingL1Data | DerivationState::AwaitingSignal
                );
                if absorbing
                    && safe_head.block_info.number <= self.confirmed_safe_head.block_info.number
                {
                    warn!(
                        target: "derivation",
                        state = ?prev_state,
                        safe_head = ?safe_head,
                        confirmed = ?self.confirmed_safe_head,
                        "Ignoring non-advancing out-of-lockstep safe-head update",
                    );
                    Metrics::derivation_non_advancing_safe_head_updates().increment(1);
                } else {
                    self.confirmed_safe_head = **safe_head;
                }
            }
            DerivationStateUpdate::ELSyncCompleted(safe_head) => {
                self.confirmed_safe_head = **safe_head;
            }
            _ => {}
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::BlockNumHash;
    use alloy_primitives::{BlockHash, b256};
    use base_common_rpc_types_engine::BasePayloadAttributes;
    use base_protocol::{AttributesWithParent, BlockInfo};
    use rstest::rstest;

    use super::{
        DerivationState::*, DerivationStateMachine, DerivationStateTransitionError,
        DerivationStateUpdate::*, L2BlockInfo, transition,
    };

    /// Creates a dummy `L2BlockInfo` for testing
    fn dummy_l2_block_info() -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo {
                hash: b256!("0000000000000000000000000000000000000000000000000000000000000001"),
                number: 1,
                parent_hash: BlockHash::default(),
                timestamp: 0,
            },
            l1_origin: BlockNumHash { hash: BlockHash::default(), number: 0 },
            seq_num: 0,
        }
    }

    /// Creates a dummy `AttributesWithParent` for testing
    fn dummy_base_attributes() -> AttributesWithParent {
        AttributesWithParent {
            attributes: BasePayloadAttributes::default(),
            parent: dummy_l2_block_info(),
            derived_from: None,
            is_last_in_span: false,
        }
    }

    // This is just here to shrink the #[case(...)] statements below for readability.
    fn attrs() -> Box<AttributesWithParent> {
        Box::new(dummy_base_attributes())
    }

    // This is just here to shrink the #[case(...)] statements below for readability.
    fn block() -> Box<L2BlockInfo> {
        Box::new(dummy_l2_block_info())
    }

    /// Creates an `L2BlockInfo` at a specific block number with a distinct hash.
    fn block_at(number: u64) -> Box<L2BlockInfo> {
        Box::new(L2BlockInfo {
            block_info: BlockInfo {
                hash: BlockHash::with_last_byte(number as u8),
                number,
                parent_hash: BlockHash::default(),
                timestamp: number,
            },
            l1_origin: BlockNumHash { hash: BlockHash::default(), number: 0 },
            seq_num: 0,
        })
    }

    #[rstest]
    // AwaitingELSyncCompletion valid transitions
    #[case(AwaitingELSyncCompletion, ELSyncCompleted(block()), Deriving)]
    #[case(AwaitingELSyncCompletion, NewAttributesConfirmed(block()), AwaitingELSyncCompletion)]
    #[case(AwaitingELSyncCompletion, SignalProcessed, AwaitingELSyncCompletion)]
    #[case(AwaitingELSyncCompletion, L1DataReceived, AwaitingELSyncCompletion)]
    // AwaitingL1Data valid transitions
    #[case(AwaitingL1Data, L1DataReceived, Deriving)]
    #[case(AwaitingL1Data, SignalProcessed, AwaitingUpdateAfterSignal)]
    #[case(AwaitingL1Data, NewAttributesConfirmed(block()), AwaitingL1Data)]
    // AwaitingSafeHeadConfirmation valid transitions
    #[case(AwaitingSafeHeadConfirmation, NewAttributesConfirmed(block()), Deriving)]
    #[case(AwaitingSafeHeadConfirmation, SignalProcessed, AwaitingUpdateAfterSignal)]
    #[case(AwaitingSafeHeadConfirmation, L1DataReceived, AwaitingSafeHeadConfirmation)]
    // AwaitingSignal valid transitions
    #[case(AwaitingSignal, SignalProcessed, AwaitingUpdateAfterSignal)]
    #[case(AwaitingSignal, L1DataReceived, AwaitingSignal)]
    #[case(AwaitingSignal, MoreDataNeeded, AwaitingSignal)]
    #[case(AwaitingSignal, NewAttributesConfirmed(block()), AwaitingSignal)]
    // AwaitingUpdateAfterSignal valid transitions
    #[case(AwaitingUpdateAfterSignal, L1DataReceived, Deriving)]
    #[case(AwaitingUpdateAfterSignal, NewAttributesConfirmed(block()), Deriving)]
    #[case(AwaitingUpdateAfterSignal, SignalProcessed, AwaitingUpdateAfterSignal)]
    // Deriving valid transitions
    #[case(Deriving, NewAttributesDerived(attrs()), AwaitingSafeHeadConfirmation)]
    #[case(Deriving, SignalNeeded, AwaitingSignal)]
    #[case(Deriving, MoreDataNeeded, AwaitingL1Data)]
    fn test_valid_transitions(
        #[case] state: super::DerivationState,
        #[case] update: super::DerivationStateUpdate,
        #[case] expected_state: super::DerivationState,
    ) {
        let result = transition(&state, &update);
        assert!(result.is_ok(), "Expected valid transition from {state:?} with {update:?}");
        assert_eq!(
            result.unwrap(),
            expected_state,
            "Transition from {state:?} with {update:?} should result in {expected_state:?}"
        );
    }

    #[rstest]
    // AwaitingELSyncCompletion invalid transitions
    #[case(AwaitingELSyncCompletion, MoreDataNeeded)]
    #[case(AwaitingELSyncCompletion, NewAttributesDerived(attrs()))]
    #[case(AwaitingELSyncCompletion, SignalNeeded)]
    // AwaitingL1Data invalid transitions
    #[case(AwaitingL1Data, ELSyncCompleted(block()))]
    #[case(AwaitingL1Data, MoreDataNeeded)]
    #[case(AwaitingL1Data, NewAttributesDerived(attrs()))]
    #[case(AwaitingL1Data, SignalNeeded)]
    // AwaitingSafeHeadConfirmation invalid transitions
    #[case(AwaitingSafeHeadConfirmation, ELSyncCompleted(block()))]
    #[case(AwaitingSafeHeadConfirmation, MoreDataNeeded)]
    #[case(AwaitingSafeHeadConfirmation, NewAttributesDerived(attrs()))]
    #[case(AwaitingSafeHeadConfirmation, SignalNeeded)]
    // AwaitingSignal invalid transitions
    #[case(AwaitingSignal, ELSyncCompleted(block()))]
    #[case(AwaitingSignal, NewAttributesDerived(attrs()))]
    #[case(AwaitingSignal, SignalNeeded)]
    // AwaitingUpdateAfterSignal invalid transitions
    #[case(AwaitingUpdateAfterSignal, ELSyncCompleted(block()))]
    #[case(AwaitingUpdateAfterSignal, MoreDataNeeded)]
    #[case(AwaitingUpdateAfterSignal, NewAttributesDerived(attrs()))]
    #[case(AwaitingUpdateAfterSignal, SignalNeeded)]
    // Deriving invalid transitions
    #[case(Deriving, ELSyncCompleted(block()))]
    #[case(Deriving, L1DataReceived)]
    #[case(Deriving, NewAttributesConfirmed(block()))]
    #[case(Deriving, SignalProcessed)]
    fn test_invalid_transitions(
        #[case] state: super::DerivationState,
        #[case] update: super::DerivationStateUpdate,
    ) {
        let result = transition(&state, &update);
        assert!(result.is_err(), "Expected invalid transition from {state:?} with {update:?}");
        match result.unwrap_err() {
            DerivationStateTransitionError::InvalidTransition {
                state: err_state,
                update: err_update,
            } => {
                assert_eq!(err_state, state);
                assert_eq!(err_update, update);
            }
        }
    }

    #[test]
    fn test_state_machine_initial_state() {
        let machine = DerivationStateMachine::new();
        assert_eq!(machine.current_state(), AwaitingELSyncCompletion);
        assert_eq!(machine.last_confirmed_safe_head(), L2BlockInfo::default());
    }

    #[test]
    fn test_state_machine_sync_completed_safe_head_update() {
        let mut machine = DerivationStateMachine::new();
        let safe_head = dummy_l2_block_info();

        machine.update(&ELSyncCompleted(Box::new(safe_head))).unwrap();

        assert_eq!(machine.current_state(), Deriving);
        assert_eq!(machine.last_confirmed_safe_head(), safe_head);
    }

    #[test]
    fn test_state_machine_update_preserves_confirmed_safe_head() {
        let mut machine = DerivationStateMachine::new();
        let first_safe_head = dummy_l2_block_info();

        machine.update(&ELSyncCompleted(Box::new(first_safe_head))).unwrap();

        // Transition to AwaitingL1Data
        machine.update(&MoreDataNeeded).unwrap();

        // Receive L1 data and go back to Deriving
        machine.update(&L1DataReceived).unwrap();

        // Safe head should still be the first one
        assert_eq!(machine.last_confirmed_safe_head(), first_safe_head);
    }

    #[test]
    fn test_state_machine_updates_safe_head_on_confirmation() {
        let mut machine = DerivationStateMachine::new();
        let initial_safe_head = dummy_l2_block_info();

        machine.update(&ELSyncCompleted(Box::new(initial_safe_head))).unwrap();

        // Derive new attributes
        machine.update(&NewAttributesDerived(Box::new(dummy_base_attributes()))).unwrap();

        let new_safe_head = L2BlockInfo {
            block_info: BlockInfo {
                hash: b256!("0000000000000000000000000000000000000000000000000000000000000002"),
                number: 2,
                parent_hash: initial_safe_head.block_info.hash,
                timestamp: 1,
            },
            l1_origin: BlockNumHash { hash: BlockHash::default(), number: 0 },
            seq_num: 0,
        };

        // Confirm new attributes
        machine.update(&NewAttributesConfirmed(Box::new(new_safe_head))).unwrap();

        assert_eq!(machine.current_state(), Deriving);
        assert_eq!(machine.last_confirmed_safe_head(), new_safe_head);
    }

    #[test]
    fn test_state_machine_invalid_transition_error() {
        let mut machine = DerivationStateMachine::new();

        let result = machine.update(&MoreDataNeeded);
        assert!(result.is_err());

        match result.unwrap_err() {
            DerivationStateTransitionError::InvalidTransition { state, update } => {
                assert_eq!(state, AwaitingELSyncCompletion);
                assert!(matches!(update, MoreDataNeeded));
            }
        }
    }

    /// An out-of-lockstep engine safe-head confirmation that advances the safe head while
    /// `AwaitingL1Data` must be absorbed: stay in `AwaitingL1Data` and move the safe head forward.
    #[test]
    fn test_awaiting_l1_data_absorbs_forward_confirmation() {
        let mut machine = DerivationStateMachine::new();
        machine.update(&ELSyncCompleted(block_at(5))).unwrap();
        machine.update(&MoreDataNeeded).unwrap();
        assert_eq!(machine.current_state(), AwaitingL1Data);

        machine.update(&NewAttributesConfirmed(block_at(7))).unwrap();

        assert_eq!(machine.current_state(), AwaitingL1Data);
        assert_eq!(machine.last_confirmed_safe_head().block_info.number, 7);
    }

    /// A non-advancing confirmation received in `AwaitingL1Data` (missing/reordered reset signal)
    /// must never regress the confirmed safe head, and must not crash.
    #[rstest]
    #[case(block_at(5))] // equal
    #[case(block_at(3))] // backwards
    fn test_awaiting_l1_data_ignores_non_advancing_confirmation(
        #[case] confirmation: Box<L2BlockInfo>,
    ) {
        let mut machine = DerivationStateMachine::new();
        machine.update(&ELSyncCompleted(block_at(5))).unwrap();
        machine.update(&MoreDataNeeded).unwrap();
        assert_eq!(machine.current_state(), AwaitingL1Data);

        machine.update(&NewAttributesConfirmed(confirmation)).unwrap();

        assert_eq!(machine.current_state(), AwaitingL1Data);
        assert_eq!(machine.last_confirmed_safe_head().block_info.number, 5);
    }

    /// The same absorption behaviour applies while `AwaitingSignal` (waiting on a pending reset).
    #[test]
    fn test_awaiting_signal_absorbs_forward_confirmation() {
        let mut machine = DerivationStateMachine::new();
        machine.update(&ELSyncCompleted(block_at(5))).unwrap();
        machine.update(&SignalNeeded).unwrap();
        assert_eq!(machine.current_state(), AwaitingSignal);

        machine.update(&NewAttributesConfirmed(block_at(7))).unwrap();

        assert_eq!(machine.current_state(), AwaitingSignal);
        assert_eq!(machine.last_confirmed_safe_head().block_info.number, 7);
    }

    /// Regression guard: a reset legitimately moves the safe head backwards. The confirmation
    /// arrives in `AwaitingUpdateAfterSignal` (after the reset signal), where it must be applied
    /// unconditionally so the safe head regresses to the reset target.
    #[test]
    fn test_reset_path_applies_backwards_confirmation() {
        let mut machine = DerivationStateMachine::new();
        machine.update(&ELSyncCompleted(block_at(5))).unwrap();
        machine.update(&SignalNeeded).unwrap();
        machine.update(&SignalProcessed).unwrap();
        assert_eq!(machine.current_state(), AwaitingUpdateAfterSignal);

        // Reset target is behind the previously confirmed head.
        machine.update(&NewAttributesConfirmed(block_at(3))).unwrap();

        assert_eq!(machine.current_state(), Deriving);
        assert_eq!(machine.last_confirmed_safe_head().block_info.number, 3);
    }
}
