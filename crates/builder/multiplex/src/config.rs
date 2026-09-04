/// Runtime routing configuration.
#[derive(Debug, Clone, Default)]
pub struct RoutingConfig {
    /// Enables the Cobalt payload-builder cutover while running both builders.
    pub cutover_enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::RoutingConfig;

    #[test]
    fn cutover_defaults_to_disabled() {
        assert!(!RoutingConfig::default().cutover_enabled);
    }
}
