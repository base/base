/// A keybinding with its key and description for display in help.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Keybinding {
    /// Key or key combination label (e.g. "Esc", "↑/↓").
    pub key: &'static str,
    /// Human-readable description of the action.
    pub description: &'static str,
}
