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
pub struct Router {
    current: ViewId,
    history: Vec<ViewId>,
}

impl Router {
    /// Creates a new router starting at the given view.
    pub const fn new(initial: ViewId) -> Self {
        Self { current: initial, history: Vec::new() }
    }

    /// Returns the currently active view identifier.
    pub const fn current(&self) -> ViewId {
        self.current
    }

    /// Switches to the given view, pushing the current view onto history.
    pub fn switch_to(&mut self, view: ViewId) {
        if view != self.current {
            self.history.push(self.current);
            self.current = view;
        }
    }

    /// Navigates back to the previous view, returning true if successful.
    pub fn back(&mut self) -> bool {
        if let Some(prev) = self.history.pop() {
            self.current = prev;
            true
        } else {
            false
        }
    }

    /// Clears navigation history and returns to the home view.
    pub fn go_home(&mut self) {
        self.history.clear();
        self.current = ViewId::Home;
    }
}
