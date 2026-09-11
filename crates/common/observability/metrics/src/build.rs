/// Build identity embedded at compile time and attached to exported metrics.
#[derive(Debug, Clone, Copy)]
pub struct MetricsBuild;

impl MetricsBuild {
    /// Manual build name. Set `BASE_BUILD_NAME` when compiling; defaults to `dev`.
    pub const NAME: &'static str = match option_env!("BASE_BUILD_NAME") {
        Some(name) if !name.is_empty() => name,
        _ => "dev",
    };
}
