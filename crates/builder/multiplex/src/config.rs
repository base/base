use std::time::Duration;

/// Runtime routing configuration.
#[derive(Debug, Clone)]
pub struct RoutingConfig {
    /// Enables the Denim payload-builder cutover while running both builders.
    pub cutover_enabled: bool,
    /// Expected maximum time between a build dispatch and the selected
    /// builder's `getPayload` resolve completing. Exceeding this records a
    /// `mux_selected_deadline_miss_total` sample.
    pub getpayload_deadline: Duration,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self { cutover_enabled: false, getpayload_deadline: Duration::from_secs(5) }
    }
}

#[cfg(test)]
mod tests {
    use super::RoutingConfig;

    #[test]
    fn cutover_defaults_to_disabled() {
        assert!(!RoutingConfig::default().cutover_enabled);
    }
}
