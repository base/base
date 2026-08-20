//! Typestate controller for the [`crate::DerivationActor`].
//!
//! Idle wait states are distinct types with total mailbox handlers. Internal commands
//! (`more_data_needed`, `signal_needed`, `attributes_derived`) exist only on [`Deriving`].
//! Derived-attribute confirmation is a oneshot capability owned by [`AwaitingSafeHead`];
//! out-of-band engine safe-head advances are mailbox events that never return an error.

use base_consensus_derive::{ActivationSignal, ResetSignal, Signal};
use base_protocol::L2BlockInfo;
use tokio::sync::oneshot;

use crate::Metrics;

/// Observability projection of the derivation actor's wait or work phase.
///
/// This enum is a snapshot for logs, metrics, and tests. It does not drive transitions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DerivationState {
    /// Waiting for notification that EL sync has completed before derivation can start.
    AwaitingELSyncCompletion,
    /// Idle awaiting additional L1 data.
    AwaitingL1Data,
    /// Derived attributes were sent to the engine; waiting on the confirmation oneshot.
    AwaitingSafeHeadConfirmation,
    /// A reorg or inconsistency requires a [`base_consensus_derive::Signal`] before continuing.
    AwaitingSignal,
    /// After a signal, waiting for L1 data or an engine safe-head update to derive again.
    AwaitingUpdateAfterSignal,
    /// Actively attempting derivation. Never stored across an inbound `recv`.
    Deriving,
}

impl DerivationState {
    /// Bounded metrics label for this snapshot.
    pub const fn metric_label(self) -> &'static str {
        match self {
            Self::AwaitingELSyncCompletion => "awaiting_el_sync_completion",
            Self::AwaitingL1Data => "awaiting_l1_data",
            Self::AwaitingSafeHeadConfirmation => "awaiting_safe_head_confirmation",
            Self::AwaitingSignal => "awaiting_signal",
            Self::AwaitingUpdateAfterSignal => "awaiting_update_after_signal",
            Self::Deriving => "deriving",
        }
    }
}

/// Applies out-of-band engine safe-head notifications to the derivation cursor.
#[derive(Clone, Copy, Debug, Default)]
pub struct SafeHeadCursor;

impl SafeHeadCursor {
    /// Chooses the derivation cursor after an engine safe-head notification.
    ///
    /// Same-hash updates keep `confirmed`. Heads that do not move the cursor forward by
    /// block number are logged and counted, and also keep `confirmed`. Equal-number hash
    /// changes (same-height replacement) and strictly greater numbers return `incoming`.
    pub fn advance(
        confirmed: L2BlockInfo,
        incoming: L2BlockInfo,
        state: DerivationState,
    ) -> L2BlockInfo {
        if incoming.block_info.hash == confirmed.block_info.hash {
            info!(
                target: "derivation",
                incoming_number = incoming.block_info.number,
                confirmed_number = confirmed.block_info.number,
                "Re-received safe head"
            );
            return confirmed;
        }

        if incoming.block_info.number < confirmed.block_info.number {
            warn!(
                target: "derivation",
                state = ?state,
                incoming_number = incoming.block_info.number,
                confirmed_number = confirmed.block_info.number,
                "Ignoring non-advancing out-of-lockstep safe-head update"
            );
            Metrics::derivation_non_advancing_safe_head_updates(state.metric_label()).increment(1);
            return confirmed;
        }

        incoming
    }

    /// Cursor after a pipeline signal. Reset and activation install `l2_safe_head`; flush
    /// keeps `confirmed`.
    pub const fn after_signal(confirmed: L2BlockInfo, signal: Signal) -> L2BlockInfo {
        match signal {
            Signal::Reset(ResetSignal { l2_safe_head })
            | Signal::Activation(ActivationSignal { l2_safe_head }) => l2_safe_head,
            Signal::FlushChannel => confirmed,
        }
    }
}

/// Waiting for EL sync to complete. Initial idle state; never re-entered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AwaitingELSync {
    /// Last engine-confirmed L2 safe head used as the derivation cursor.
    pub confirmed_safe_head: L2BlockInfo,
}

impl Default for AwaitingELSync {
    fn default() -> Self {
        Self::new()
    }
}

impl AwaitingELSync {
    /// Constructs the initial EL-sync wait state with a default (genesis) cursor.
    pub fn new() -> Self {
        Self { confirmed_safe_head: L2BlockInfo::default() }
    }

    /// Marks EL sync complete and starts derivation from the engine's safe head.
    pub const fn into_deriving(self, head: L2BlockInfo) -> Deriving {
        Deriving { confirmed_safe_head: head }
    }
}

/// Idle while the pipeline has no further L1 data to consume.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AwaitingL1Data {
    /// Last engine-confirmed L2 safe head used as the derivation cursor.
    pub confirmed_safe_head: L2BlockInfo,
}

/// Waiting for the oneshot that completes derived-attribute consolidation.
pub struct AwaitingSafeHead {
    /// Last engine-confirmed L2 safe head used as the derivation cursor.
    pub confirmed_safe_head: L2BlockInfo,
    /// Completes when the engine finishes the consolidate task for the in-flight attributes.
    pub confirmed_rx: oneshot::Receiver<L2BlockInfo>,
}

impl std::fmt::Debug for AwaitingSafeHead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwaitingSafeHead")
            .field("confirmed_safe_head", &self.confirmed_safe_head)
            .field("confirmed_rx", &"oneshot::Receiver")
            .finish()
    }
}

impl AwaitingSafeHead {
    /// Completes the derived-attribute handshake and returns to derivation.
    pub fn on_attributes_confirmed(self, head: L2BlockInfo) -> Deriving {
        Deriving {
            confirmed_safe_head: SafeHeadCursor::advance(
                self.confirmed_safe_head,
                head,
                DerivationState::AwaitingSafeHeadConfirmation,
            ),
        }
    }
}

/// Waiting for a pipeline signal (reset or flush) to be processed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AwaitingSignal {
    /// Last engine-confirmed L2 safe head used as the derivation cursor.
    pub confirmed_safe_head: L2BlockInfo,
}

/// After a signal, waiting for L1 data or an engine safe-head update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AwaitingUpdateAfterSignal {
    /// Last engine-confirmed L2 safe head used as the derivation cursor.
    pub confirmed_safe_head: L2BlockInfo,
}

/// Transient work phase that steps the pipeline. Never stored across an inbound `recv`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Deriving {
    /// Last engine-confirmed L2 safe head used as the derivation cursor.
    pub confirmed_safe_head: L2BlockInfo,
}

impl Deriving {
    /// Constructs a derivation work phase from the given cursor.
    pub const fn new(confirmed_safe_head: L2BlockInfo) -> Self {
        Self { confirmed_safe_head }
    }

    /// Yields until more L1 data arrives.
    pub const fn more_data_needed(self) -> AwaitingL1Data {
        AwaitingL1Data { confirmed_safe_head: self.confirmed_safe_head }
    }

    /// Waits for a pipeline signal after requesting an engine reset.
    pub const fn signal_needed(self) -> AwaitingSignal {
        AwaitingSignal { confirmed_safe_head: self.confirmed_safe_head }
    }

    /// Hands derived attributes to the engine and returns the wait state plus the oneshot sender.
    pub fn attributes_derived(self) -> (AwaitingSafeHead, oneshot::Sender<L2BlockInfo>) {
        let (confirmed_tx, confirmed_rx) = oneshot::channel();
        (
            AwaitingSafeHead { confirmed_safe_head: self.confirmed_safe_head, confirmed_rx },
            confirmed_tx,
        )
    }
}

/// Dispatcher for idle wait states at the actor-loop boundary.
#[derive(Debug)]
pub enum Idle {
    /// [`AwaitingELSync`].
    ELSync(AwaitingELSync),
    /// [`AwaitingL1Data`].
    L1Data(AwaitingL1Data),
    /// [`AwaitingSafeHead`].
    SafeHead(AwaitingSafeHead),
    /// [`AwaitingSignal`].
    Signal(AwaitingSignal),
    /// [`AwaitingUpdateAfterSignal`].
    AfterSignal(AwaitingUpdateAfterSignal),
}

impl Idle {
    /// Initial idle state before EL sync completes.
    pub fn initial() -> Self {
        Self::ELSync(AwaitingELSync::new())
    }

    /// Observability snapshot of this idle state.
    pub const fn projection(&self) -> DerivationState {
        match self {
            Self::ELSync(_) => DerivationState::AwaitingELSyncCompletion,
            Self::L1Data(_) => DerivationState::AwaitingL1Data,
            Self::SafeHead(_) => DerivationState::AwaitingSafeHeadConfirmation,
            Self::Signal(_) => DerivationState::AwaitingSignal,
            Self::AfterSignal(_) => DerivationState::AwaitingUpdateAfterSignal,
        }
    }

    /// Cursor used as the base of the current derivation.
    pub const fn confirmed_safe_head(&self) -> L2BlockInfo {
        match self {
            Self::ELSync(state) => state.confirmed_safe_head,
            Self::L1Data(state) => state.confirmed_safe_head,
            Self::SafeHead(state) => state.confirmed_safe_head,
            Self::Signal(state) => state.confirmed_safe_head,
            Self::AfterSignal(state) => state.confirmed_safe_head,
        }
    }
}

/// Result of applying a mailbox event to an idle state.
#[derive(Debug)]
pub enum AfterMailbox {
    /// Remain idle (possibly in a different wait state).
    Idle(Idle),
    /// Start a derivation attempt.
    Derive(Deriving),
}

impl AfterMailbox {
    /// Cursor after this mailbox event.
    pub const fn confirmed_safe_head(&self) -> L2BlockInfo {
        match self {
            Self::Idle(idle) => idle.confirmed_safe_head(),
            Self::Derive(deriving) => deriving.confirmed_safe_head,
        }
    }
}

/// Total mailbox interface implemented by every idle wait state.
pub trait MailboxIdle: Sized {
    /// Observability snapshot.
    fn projection(&self) -> DerivationState;

    /// Cursor used as the base of the current derivation.
    fn confirmed_safe_head(&self) -> L2BlockInfo;

    /// Wraps this wait state in [`Idle`].
    fn into_idle(self) -> Idle;

    /// More L1 data is available.
    fn on_l1_data(self) -> AfterMailbox;

    /// The engine's safe head moved (out-of-band consolidation or drain).
    fn on_engine_safe_head(self, head: L2BlockInfo) -> AfterMailbox;

    /// A pipeline signal has been processed.
    ///
    /// Reset and activation install that signal's `l2_safe_head` as the derivation cursor
    /// so a reorg cannot leave derivation stepping from a pre-reset head. Flush keeps the
    /// current cursor.
    fn on_signal_processed(self, signal: Signal) -> AfterMailbox;

    /// EL sync completed. Ignored after leaving [`AwaitingELSync`].
    fn on_el_sync_completed(self, head: L2BlockInfo) -> AfterMailbox;
}

impl MailboxIdle for AwaitingELSync {
    fn projection(&self) -> DerivationState {
        DerivationState::AwaitingELSyncCompletion
    }

    fn confirmed_safe_head(&self) -> L2BlockInfo {
        self.confirmed_safe_head
    }

    fn into_idle(self) -> Idle {
        Idle::ELSync(self)
    }

    fn on_l1_data(self) -> AfterMailbox {
        AfterMailbox::Idle(Idle::ELSync(self))
    }

    fn on_engine_safe_head(self, head: L2BlockInfo) -> AfterMailbox {
        AfterMailbox::Idle(Idle::ELSync(Self {
            confirmed_safe_head: SafeHeadCursor::advance(
                self.confirmed_safe_head,
                head,
                DerivationState::AwaitingELSyncCompletion,
            ),
        }))
    }

    fn on_signal_processed(self, signal: Signal) -> AfterMailbox {
        AfterMailbox::Idle(Idle::ELSync(Self {
            confirmed_safe_head: SafeHeadCursor::after_signal(self.confirmed_safe_head, signal),
        }))
    }

    fn on_el_sync_completed(self, head: L2BlockInfo) -> AfterMailbox {
        AfterMailbox::Derive(self.into_deriving(head))
    }
}

impl MailboxIdle for AwaitingL1Data {
    fn projection(&self) -> DerivationState {
        DerivationState::AwaitingL1Data
    }

    fn confirmed_safe_head(&self) -> L2BlockInfo {
        self.confirmed_safe_head
    }

    fn into_idle(self) -> Idle {
        Idle::L1Data(self)
    }

    fn on_l1_data(self) -> AfterMailbox {
        AfterMailbox::Derive(Deriving { confirmed_safe_head: self.confirmed_safe_head })
    }

    fn on_engine_safe_head(self, head: L2BlockInfo) -> AfterMailbox {
        AfterMailbox::Idle(Idle::L1Data(Self {
            confirmed_safe_head: SafeHeadCursor::advance(
                self.confirmed_safe_head,
                head,
                DerivationState::AwaitingL1Data,
            ),
        }))
    }

    fn on_signal_processed(self, signal: Signal) -> AfterMailbox {
        AfterMailbox::Idle(Idle::AfterSignal(AwaitingUpdateAfterSignal {
            confirmed_safe_head: SafeHeadCursor::after_signal(self.confirmed_safe_head, signal),
        }))
    }

    fn on_el_sync_completed(self, _head: L2BlockInfo) -> AfterMailbox {
        AfterMailbox::Idle(Idle::L1Data(self))
    }
}

impl MailboxIdle for AwaitingSafeHead {
    fn projection(&self) -> DerivationState {
        DerivationState::AwaitingSafeHeadConfirmation
    }

    fn confirmed_safe_head(&self) -> L2BlockInfo {
        self.confirmed_safe_head
    }

    fn into_idle(self) -> Idle {
        Idle::SafeHead(self)
    }

    fn on_l1_data(self) -> AfterMailbox {
        AfterMailbox::Idle(Idle::SafeHead(self))
    }

    fn on_engine_safe_head(self, head: L2BlockInfo) -> AfterMailbox {
        AfterMailbox::Idle(Idle::SafeHead(Self {
            confirmed_safe_head: SafeHeadCursor::advance(
                self.confirmed_safe_head,
                head,
                DerivationState::AwaitingSafeHeadConfirmation,
            ),
            confirmed_rx: self.confirmed_rx,
        }))
    }

    fn on_signal_processed(self, signal: Signal) -> AfterMailbox {
        AfterMailbox::Idle(Idle::AfterSignal(AwaitingUpdateAfterSignal {
            confirmed_safe_head: SafeHeadCursor::after_signal(self.confirmed_safe_head, signal),
        }))
    }

    fn on_el_sync_completed(self, _head: L2BlockInfo) -> AfterMailbox {
        AfterMailbox::Idle(Idle::SafeHead(self))
    }
}

impl MailboxIdle for AwaitingSignal {
    fn projection(&self) -> DerivationState {
        DerivationState::AwaitingSignal
    }

    fn confirmed_safe_head(&self) -> L2BlockInfo {
        self.confirmed_safe_head
    }

    fn into_idle(self) -> Idle {
        Idle::Signal(self)
    }

    fn on_l1_data(self) -> AfterMailbox {
        AfterMailbox::Idle(Idle::Signal(self))
    }

    fn on_engine_safe_head(self, head: L2BlockInfo) -> AfterMailbox {
        AfterMailbox::Idle(Idle::Signal(Self {
            confirmed_safe_head: SafeHeadCursor::advance(
                self.confirmed_safe_head,
                head,
                DerivationState::AwaitingSignal,
            ),
        }))
    }

    fn on_signal_processed(self, signal: Signal) -> AfterMailbox {
        AfterMailbox::Idle(Idle::AfterSignal(AwaitingUpdateAfterSignal {
            confirmed_safe_head: SafeHeadCursor::after_signal(self.confirmed_safe_head, signal),
        }))
    }

    fn on_el_sync_completed(self, _head: L2BlockInfo) -> AfterMailbox {
        AfterMailbox::Idle(Idle::Signal(self))
    }
}

impl MailboxIdle for AwaitingUpdateAfterSignal {
    fn projection(&self) -> DerivationState {
        DerivationState::AwaitingUpdateAfterSignal
    }

    fn confirmed_safe_head(&self) -> L2BlockInfo {
        self.confirmed_safe_head
    }

    fn into_idle(self) -> Idle {
        Idle::AfterSignal(self)
    }

    fn on_l1_data(self) -> AfterMailbox {
        AfterMailbox::Derive(Deriving { confirmed_safe_head: self.confirmed_safe_head })
    }

    fn on_engine_safe_head(self, head: L2BlockInfo) -> AfterMailbox {
        AfterMailbox::Derive(Deriving {
            confirmed_safe_head: SafeHeadCursor::advance(
                self.confirmed_safe_head,
                head,
                DerivationState::AwaitingUpdateAfterSignal,
            ),
        })
    }

    fn on_signal_processed(self, signal: Signal) -> AfterMailbox {
        AfterMailbox::Idle(Idle::AfterSignal(Self {
            confirmed_safe_head: SafeHeadCursor::after_signal(self.confirmed_safe_head, signal),
        }))
    }

    fn on_el_sync_completed(self, _head: L2BlockInfo) -> AfterMailbox {
        AfterMailbox::Idle(Idle::AfterSignal(self))
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::BlockNumHash;
    use alloy_primitives::{B256, BlockHash};
    use base_consensus_derive::{ResetSignal, Signal};
    use base_protocol::BlockInfo;

    use super::{
        AfterMailbox, AwaitingELSync, AwaitingL1Data, AwaitingSafeHead, AwaitingSignal,
        AwaitingUpdateAfterSignal, DerivationState, Deriving, Idle, L2BlockInfo, MailboxIdle,
        SafeHeadCursor,
    };

    fn block(number: u64, hash_byte: u8) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo {
                hash: B256::with_last_byte(hash_byte),
                number,
                parent_hash: BlockHash::default(),
                timestamp: number,
            },
            l1_origin: BlockNumHash { hash: BlockHash::default(), number: 0 },
            seq_num: 0,
        }
    }

    fn assert_idle_l1(after: AfterMailbox) -> AwaitingL1Data {
        match after {
            AfterMailbox::Idle(Idle::L1Data(state)) => state,
            other => panic!("expected Idle::L1Data, got {other:?}"),
        }
    }

    fn assert_derive(after: AfterMailbox) -> Deriving {
        match after {
            AfterMailbox::Derive(deriving) => deriving,
            other => panic!("expected Derive, got {other:?}"),
        }
    }

    fn assert_after_signal(after: AfterMailbox) -> AwaitingUpdateAfterSignal {
        match after {
            AfterMailbox::Idle(Idle::AfterSignal(state)) => state,
            other => panic!("expected Idle::AfterSignal, got {other:?}"),
        }
    }

    #[test]
    fn initial_idle_is_el_sync() {
        let idle = Idle::initial();
        assert_eq!(idle.projection(), DerivationState::AwaitingELSyncCompletion);
        assert_eq!(idle.confirmed_safe_head(), L2BlockInfo::default());
    }

    #[test]
    fn el_sync_completed_starts_deriving() {
        let head = block(1, 1);
        let deriving = AwaitingELSync::new().into_deriving(head);
        assert_eq!(deriving.confirmed_safe_head, head);
    }

    #[test]
    fn el_sync_mailbox_events_stay() {
        let state = AwaitingELSync::new();
        assert!(matches!(state.on_l1_data(), AfterMailbox::Idle(Idle::ELSync(_))));
        assert!(matches!(
            state.on_signal_processed(Signal::FlushChannel),
            AfterMailbox::Idle(Idle::ELSync(_))
        ));
        assert!(matches!(
            state.on_engine_safe_head(block(1, 1)),
            AfterMailbox::Idle(Idle::ELSync(_))
        ));
        assert!(matches!(state.on_el_sync_completed(block(1, 1)), AfterMailbox::Derive(_)));
    }

    #[test]
    fn l1_data_l1_received_derives() {
        let state = AwaitingL1Data { confirmed_safe_head: block(1, 1) };
        let deriving = assert_derive(state.on_l1_data());
        assert_eq!(deriving.confirmed_safe_head, block(1, 1));
    }

    #[test]
    fn l1_data_engine_safe_head_stays_and_advances_cursor() {
        let state = AwaitingL1Data { confirmed_safe_head: block(1, 1) };
        let next = assert_idle_l1(state.on_engine_safe_head(block(2, 2)));
        assert_eq!(next.confirmed_safe_head, block(2, 2));
        assert_eq!(next.projection(), DerivationState::AwaitingL1Data);
    }

    #[test]
    fn l1_data_non_advancing_head_does_not_rewind() {
        let original = block(5, 5);
        let state = AwaitingL1Data { confirmed_safe_head: original };
        let next = assert_idle_l1(state.on_engine_safe_head(block(3, 3)));
        assert_eq!(next.confirmed_safe_head, original);
    }

    #[test]
    fn l1_data_equal_hash_is_noop() {
        let original = block(5, 5);
        let state = AwaitingL1Data { confirmed_safe_head: original };
        let next = assert_idle_l1(state.on_engine_safe_head(original));
        assert_eq!(next.confirmed_safe_head, original);
    }

    #[test]
    fn l1_data_signal_processed_goes_to_after_signal() {
        let state = AwaitingL1Data { confirmed_safe_head: block(1, 1) };
        assert_after_signal(state.on_signal_processed(Signal::FlushChannel));
    }

    #[test]
    fn l1_data_reset_signal_rewinds_cursor() {
        let state = AwaitingL1Data { confirmed_safe_head: block(5, 5) };
        let after = assert_after_signal(
            state.on_signal_processed(ResetSignal { l2_safe_head: block(3, 3) }.signal()),
        );
        assert_eq!(after.confirmed_safe_head, block(3, 3));
        let deriving = assert_derive(after.on_l1_data());
        assert_eq!(deriving.confirmed_safe_head, block(3, 3));
    }

    #[test]
    fn l1_data_flush_signal_keeps_cursor() {
        let original = block(5, 5);
        let state = AwaitingL1Data { confirmed_safe_head: original };
        let after = assert_after_signal(state.on_signal_processed(Signal::FlushChannel));
        assert_eq!(after.confirmed_safe_head, original);
    }

    #[test]
    fn awaiting_signal_engine_safe_head_stays() {
        let state = AwaitingSignal { confirmed_safe_head: block(1, 1) };
        match state.on_engine_safe_head(block(2, 2)) {
            AfterMailbox::Idle(Idle::Signal(next)) => {
                assert_eq!(next.confirmed_safe_head, block(2, 2));
            }
            other => panic!("expected Idle::Signal, got {other:?}"),
        }
    }

    #[test]
    fn awaiting_signal_l1_stays() {
        let state = AwaitingSignal { confirmed_safe_head: block(1, 1) };
        assert!(matches!(state.on_l1_data(), AfterMailbox::Idle(Idle::Signal(_))));
    }

    #[test]
    fn after_signal_l1_or_safe_head_derives() {
        let state = AwaitingUpdateAfterSignal { confirmed_safe_head: block(1, 1) };
        assert_derive(state.on_l1_data());
        let state = AwaitingUpdateAfterSignal { confirmed_safe_head: block(1, 1) };
        let deriving = assert_derive(state.on_engine_safe_head(block(2, 2)));
        assert_eq!(deriving.confirmed_safe_head, block(2, 2));
    }

    #[test]
    fn after_signal_reset_rewinds_before_derive() {
        let state = AwaitingUpdateAfterSignal { confirmed_safe_head: block(5, 5) };
        let after = assert_after_signal(
            state.on_signal_processed(ResetSignal { l2_safe_head: block(2, 2) }.signal()),
        );
        assert_eq!(after.confirmed_safe_head, block(2, 2));
        let deriving = assert_derive(after.on_engine_safe_head(block(2, 2)));
        assert_eq!(deriving.confirmed_safe_head, block(2, 2));
    }

    #[test]
    fn awaiting_safe_head_mailbox_advance_stays() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let state = AwaitingSafeHead { confirmed_safe_head: block(1, 1), confirmed_rx: rx };
        match state.on_engine_safe_head(block(2, 2)) {
            AfterMailbox::Idle(Idle::SafeHead(next)) => {
                assert_eq!(next.confirmed_safe_head, block(2, 2));
            }
            other => panic!("expected Idle::SafeHead, got {other:?}"),
        }
        drop(tx);
    }

    #[test]
    fn awaiting_safe_head_oneshot_leaves_to_deriving() {
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let state = AwaitingSafeHead { confirmed_safe_head: block(1, 1), confirmed_rx: rx };
        let deriving = state.on_attributes_confirmed(block(2, 2));
        assert_eq!(deriving.confirmed_safe_head, block(2, 2));
    }

    #[test]
    fn deriving_internal_commands() {
        let deriving = Deriving::new(block(1, 1));
        assert_eq!(deriving.more_data_needed().confirmed_safe_head, block(1, 1));

        let deriving = Deriving::new(block(1, 1));
        assert_eq!(deriving.signal_needed().confirmed_safe_head, block(1, 1));

        let deriving = Deriving::new(block(1, 1));
        let (waiting, tx) = deriving.attributes_derived();
        assert_eq!(waiting.confirmed_safe_head, block(1, 1));
        assert!(tx.send(block(2, 2)).is_ok());
    }

    #[test]
    fn safe_head_cursor_same_height_hash_change_applies() {
        assert_eq!(
            SafeHeadCursor::advance(block(4, 1), block(4, 2), DerivationState::AwaitingL1Data),
            block(4, 2)
        );
    }

    #[test]
    fn safe_head_cursor_same_hash_does_not_apply() {
        let original = block(4, 1);
        assert_eq!(
            SafeHeadCursor::advance(original, original, DerivationState::AwaitingL1Data),
            original
        );
    }

    #[test]
    fn safe_head_cursor_rewind_keeps_confirmed() {
        let confirmed = block(5, 5);
        assert_eq!(
            SafeHeadCursor::advance(confirmed, block(3, 3), DerivationState::AwaitingL1Data),
            confirmed
        );
    }
}
