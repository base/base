use super::ViewId;

/// An action returned by a view in response to user input or a tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    /// No action to take.
    None,
    /// Quit the application.
    Quit,
    /// Switch to the specified view.
    SwitchView(ViewId),
}
