//! Payload build resolution policy.

/// Determines how we should choose the payload to return.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PayloadKind {
    /// Returns the next best available payload (the earliest available payload).
    /// This does not wait for a real for pending job to finish if there's no best payload yet and
    /// is allowed to race various payload jobs (empty, pending best) against each other and
    /// returns whichever job finishes faster.
    ///
    /// This should be used when it's more important to return a valid payload as fast as possible.
    /// This allows a sequencer to meet its slot deadline when the pending build is slow.
    #[default]
    Earliest,
    /// Only returns once we have at least one built payload.
    ///
    /// Compared to [`PayloadKind::Earliest`] this does not race an empty payload job against the
    /// already in progress one, and returns the best available built payload or awaits the job in
    /// progress.
    WaitForPending,
}
