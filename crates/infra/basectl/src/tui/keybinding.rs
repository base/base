/// A keybinding with its key and description for display in help.
#[derive(Debug, Clone, Copy)]
pub struct Keybinding {
    /// Key or key combination label (e.g. "Esc", "↑/↓").
    pub key: &'static str,
    /// Human-readable description of the action.
    pub description: &'static str,
}

impl Keybinding {
    /// Creates a new keybinding entry.
    pub const fn new(key: &'static str, description: &'static str) -> Self {
        Self { key, description }
    }
}
