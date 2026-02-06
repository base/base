/// A keybinding with its key and description for display in help.
#[derive(Debug, Clone, Copy)]
pub struct Keybinding {
    pub key: &'static str,
    pub description: &'static str,
}

impl Keybinding {
    pub const fn new(key: &'static str, description: &'static str) -> Self {
        Self { key, description }
    }
}
