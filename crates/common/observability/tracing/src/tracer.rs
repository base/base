//! Application log layers and tracing initialization.

use crate::{FileInfo, Layers, LogFormat, TestTracer, TracingGuards, install_log_handle};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

///  Tracer for application logging.
///
///  Manages the configuration and initialization of logging layers,
/// including standard output, optional journald, and optional file logging.
#[cfg(feature = "std")]
#[derive(Debug, Clone)]
pub struct RethTracer {
    stdout: LayerInfo,
    journald: Option<String>,
    file: Option<(LayerInfo, FileInfo)>,
    samply: Option<LayerInfo>,
    chrome: Option<(LayerInfo, std::path::PathBuf)>,
    #[cfg(feature = "tracy")]
    tracy: Option<LayerInfo>,
    /// When true, the stdout filter is wrapped in a reload layer so log levels
    /// can be changed at runtime.
    enable_reload: bool,
}

#[cfg(feature = "std")]
impl RethTracer {
    /// Constructs a new `Tracer` with default settings.
    ///
    /// Initializes with default stdout layer configuration.
    /// Journald and file layers are not set by default.
    pub fn new() -> Self {
        Self {
            stdout: LayerInfo::default(),
            journald: None,
            file: None,
            samply: None,
            chrome: None,
            #[cfg(feature = "tracy")]
            tracy: None,
            enable_reload: false,
        }
    }

    /// Sets a custom configuration for the stdout layer.
    ///
    /// # Arguments
    /// * `config` - The `LayerInfo` to use for the stdout layer.
    pub fn with_stdout(mut self, config: LayerInfo) -> Self {
        self.stdout = config;
        self
    }

    /// Sets the journald layer filter.
    ///
    /// # Arguments
    /// * `filter` - The `filter` to use for the journald layer.
    pub fn with_journald(mut self, filter: String) -> Self {
        self.journald = Some(filter);
        self
    }

    /// Sets the file layer configuration and associated file info.
    ///
    ///  # Arguments
    /// * `config` - The `LayerInfo` to use for the file layer.
    /// * `file_info` - The `FileInfo` containing details about the log file.
    pub fn with_file(mut self, config: LayerInfo, file_info: FileInfo) -> Self {
        self.file = Some((config, file_info));
        self
    }

    /// Sets the samply layer configuration.
    pub fn with_samply(mut self, config: LayerInfo) -> Self {
        self.samply = Some(config);
        self
    }

    /// Sets the Chrome trace layer configuration.
    pub fn with_chrome(mut self, config: LayerInfo, file: std::path::PathBuf) -> Self {
        self.chrome = Some((config, file));
        self
    }

    /// Sets the tracy layer configuration.
    #[cfg(feature = "tracy")]
    pub fn with_tracy(mut self, config: LayerInfo) -> Self {
        self.tracy = Some(config);
        self
    }

    /// Enables runtime log filter reloading.
    pub const fn with_reload(mut self, enable: bool) -> Self {
        self.enable_reload = enable;
        self
    }
}

#[cfg(feature = "std")]
impl Default for RethTracer {
    fn default() -> Self {
        Self::new()
    }
}

///  Configuration for a logging layer.
///
///  This struct holds configuration parameters for a tracing layer, including
///  the format, filtering directives, optional coloring, and directive.
#[cfg(feature = "std")]
#[derive(Debug, Clone)]
pub struct LayerInfo {
    /// Output format for this layer.
    pub format: LogFormat,
    /// Fallback filter directive.
    pub default_directive: String,
    /// Additional filter directives.
    pub filters: String,
    /// Terminal coloring mode.
    pub color: Option<String>,
}

#[cfg(feature = "std")]
impl LayerInfo {
    ///  Constructs a new `LayerInfo`.
    ///
    ///  # Arguments
    ///  * `format` - Specifies the format for log messages. Possible values are:
    ///      - `LogFormat::Json` for JSON formatting.
    ///      - `LogFormat::LogFmt` for logfmt (key=value) formatting.
    ///      - `LogFormat::Terminal` for human-readable, terminal-friendly formatting.
    ///  * `default_directive` - Directive for filtering log messages.
    ///  * `filters` - Additional filtering parameters as a string.
    ///  * `color` - Optional color configuration for the log messages.
    pub const fn new(
        format: LogFormat,
        default_directive: String,
        filters: String,
        color: Option<String>,
    ) -> Self {
        Self { format, default_directive, filters, color }
    }
}

#[cfg(feature = "std")]
impl Default for LayerInfo {
    ///  Provides default values for `LayerInfo`.
    ///
    ///  By default, it uses terminal format, INFO level filter,
    ///  no additional filters, and no color configuration.
    fn default() -> Self {
        Self {
            format: LogFormat::Terminal,
            default_directive: LevelFilter::INFO.to_string(),
            filters: String::new(),
            color: Some("always".to_string()),
        }
    }
}

/// Trait defining a general interface for logging configuration.
///
/// The `Tracer` trait provides a standardized way to initialize logging configurations
/// in an application. Implementations of this trait can specify different logging setups,
/// such as standard output logging, file logging, journald logging, or custom logging
/// configurations tailored for specific environments (like testing).
#[cfg(feature = "std")]
pub trait Tracer: Sized {
    /// Initialize the logging configuration.
    ///
    /// By default, this method creates a new `Layers` instance and delegates to `init_with_layers`.
    ///
    /// # Returns
    /// An `eyre::Result` with guards for layers that need to stay alive, or an `Err` in case of an
    /// error during initialization.
    fn init(self) -> eyre::Result<TracingGuards> {
        self.init_with_layers(Layers::new())
    }

    /// Initialize the logging configuration with additional custom layers.
    ///
    /// This is the primary method that implementors must provide.
    ///
    /// # Arguments
    /// * `layers` - Pre-configured `Layers` instance to use for initialization
    ///
    /// # Returns
    /// An `eyre::Result` with guards for layers that need to stay alive, or an `Err` in case of an
    /// error during initialization.
    fn init_with_layers(self, layers: Layers) -> eyre::Result<TracingGuards>;
}

#[cfg(feature = "std")]
impl Tracer for RethTracer {
    fn init_with_layers(self, mut layers: Layers) -> eyre::Result<TracingGuards> {
        // Configure stdout layer - reloadable if requested for runtime log level changes
        if let Some(handle) = layers.stdout(
            self.stdout.format,
            self.stdout.default_directive.parse()?,
            &self.stdout.filters,
            self.stdout.color,
            self.enable_reload,
        )? {
            install_log_handle(handle);
        }

        if let Some(config) = self.journald {
            layers.journald(&config)?;
        }

        let file_guard = if let Some((config, file_info)) = self.file {
            let (guard, handle) =
                layers.file(config.format, &config.filters, file_info, self.enable_reload)?;
            if let Some(handle) = handle {
                install_log_handle(handle);
            }
            Some(guard)
        } else {
            None
        };

        if let Some(config) = self.samply {
            layers.samply(config)?;
        }

        let chrome_guard = if let Some((config, file)) = self.chrome {
            Some(layers.chrome(config, &file)?)
        } else {
            None
        };

        #[cfg(feature = "tracy")]
        if let Some(config) = self.tracy {
            layers.tracy(config)?;
        }

        // The error is returned if the global default subscriber is already set,
        // so it's safe to ignore it
        let _ = tracing_subscriber::registry().with(layers.into_inner()).try_init();

        Ok(TracingGuards::new(file_guard, chrome_guard))
    }
}

///  Initializes a tracing subscriber for tests.
///
///  The filter is configurable via `RUST_LOG`.
///
///  # Note
///
///  The subscriber will silently fail if it could not be installed.
#[cfg(feature = "std")]
pub fn init_test_tracing() {
    let _ = TestTracer::default().init();
}
