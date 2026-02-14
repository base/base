/// Identifies a TUI view screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViewId {
    /// Main menu home screen.
    Home,
    /// Combined monitoring command center.
    CommandCenter,
    /// Data availability backlog monitor.
    DaMonitor,
    /// Flashblocks stream viewer.
    Flashblocks,
    /// Chain and system configuration viewer.
    Config,
}

/// Manages view navigation history and the current active view.
#[derive(Debug)]
pub(crate) struct Router {
    current: ViewId,
    history: Vec<ViewId>,
}

impl Router {
    /// Creates a new router starting at the given view.
    pub(crate) const fn new(initial: ViewId) -> Self {
        Self { current: initial, history: Vec::new() }
    }

    /// Returns the currently active view identifier.
    pub(crate) const fn current(&self) -> ViewId {
        self.current
    }

    /// Switches to the given view, pushing the current view onto history.
    pub(crate) fn switch_to(&mut self, view: ViewId) {
        if view != self.current {
            self.history.push(self.current);
            self.current = view;
        }
    }
}
