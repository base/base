//! Helper builder entrypoint to instantiate a [`ProviderFactory`].

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use base_common_chain_config::BaseChainSpec;
use base_execution_state_database::{
    mdbx::DatabaseArguments, mdbx::MaxReadTransactionDuration, open_db_read_only,
};

use crate::{
    ProviderFactory,
    providers::{RocksDBProvider, StaticFileProvider},
};

/// Helper type to create a [`ProviderFactory`].
///
/// See [`ProviderFactoryBuilder::open_read_only`] for usage examples.
#[derive(Debug, Default)]
pub struct ProviderFactoryBuilder;

impl ProviderFactoryBuilder {
    /// Opens the database with the given chainspec and [`ReadOnlyConfig`].
    ///
    /// # Open a monitored instance
    ///
    /// This is recommended when the new read-only instance is used with an active node.
    ///
    /// ```no_run
    /// use base_common_chain_config::BaseChainSpec;
    /// use base_execution_state_provider::providers::{ProviderFactoryBuilder};
    ///
    /// fn demo(
    ///     runtime: base_common_runtime_tasks::Runtime,
    /// ) {
    ///     let provider_factory = ProviderFactoryBuilder
    ///         .open_read_only(BaseChainSpec::mainnet().into(), "datadir", runtime)
    ///         .unwrap();
    /// }
    /// ```
    ///
    /// # Open an unmonitored instance
    ///
    /// This is recommended when no changes to the database are expected (e.g. no active node)
    ///
    /// ```no_run
    /// use base_common_chain_config::BaseChainSpec;
    /// use base_execution_state_provider::providers::{ProviderFactoryBuilder, ReadOnlyConfig};
    ///
    /// fn demo(
    ///     runtime: base_common_runtime_tasks::Runtime,
    /// ) {
    ///     let provider_factory = ProviderFactoryBuilder
    ///         .open_read_only(
    ///             BaseChainSpec::mainnet().into(),
    ///             ReadOnlyConfig::from_datadir("datadir").no_watch(),
    ///             runtime,
    ///         )
    ///         .unwrap();
    /// }
    /// ```
    ///
    /// # Open an instance with disabled read-transaction timeout
    ///
    /// By default, read transactions are automatically terminated after a timeout to prevent
    /// database free list growth. However, if the database is static (no writes occurring), this
    /// safety mechanism can be disabled using
    /// [`ReadOnlyConfig::disable_long_read_transaction_safety`].
    ///
    /// ```no_run
    /// use base_common_chain_config::BaseChainSpec;
    /// use base_execution_state_provider::providers::{ProviderFactoryBuilder, ReadOnlyConfig};
    ///
    /// fn demo(
    ///     runtime: base_common_runtime_tasks::Runtime,
    /// ) {
    ///     let provider_factory = ProviderFactoryBuilder
    ///         .open_read_only(
    ///             BaseChainSpec::mainnet().into(),
    ///             ReadOnlyConfig::from_datadir("datadir").disable_long_read_transaction_safety(),
    ///             runtime,
    ///         )
    ///         .unwrap();
    /// }
    /// ```
    pub fn open_read_only(
        self,
        chainspec: Arc<BaseChainSpec>,
        config: impl Into<ReadOnlyConfig>,
        runtime: base_common_runtime_tasks::Runtime,
    ) -> eyre::Result<ProviderFactory> {
        let ReadOnlyConfig { db_dir, db_args, static_files_dir, rocksdb_dir, watch } =
            config.into();
        let db = open_db_read_only(db_dir, db_args)?;
        let static_file_provider = StaticFileProvider::read_only(static_files_dir)?;
        let rocksdb_provider = RocksDBProvider::builder(&rocksdb_dir)
            .with_default_tables()
            .with_read_only(true)
            .build()?;
        let factory =
            ProviderFactory::new(db, chainspec, static_file_provider, rocksdb_provider, runtime)?
                .with_read_only_sync(watch);
        Ok(factory)
    }
}

/// Settings for how to open the database, static files, and `RocksDB`.
///
/// The default derivation from a path assumes the path is the datadir:
/// [`ReadOnlyConfig::from_datadir`]
#[derive(Debug, Clone)]
pub struct ReadOnlyConfig {
    /// The path to the database directory.
    pub db_dir: PathBuf,
    /// How to open the database
    pub db_args: DatabaseArguments,
    /// The path to the static file dir
    pub static_files_dir: PathBuf,
    /// The path to the `RocksDB` directory
    pub rocksdb_dir: PathBuf,
    /// Whether to watch the MDBX directory for changes and eagerly sync providers.
    pub watch: bool,
}

impl ReadOnlyConfig {
    /// Derives the [`ReadOnlyConfig`] from the datadir.
    ///
    /// By default this assumes the following datadir layout:
    ///
    /// ```text
    ///  -`datadir`
    ///    |__db
    ///    |__rocksdb
    ///    |__static_files
    /// ```
    ///
    /// By default this watches the static files directory for changes, see also
    /// [`ProviderFactory::with_read_only_sync`]
    pub fn from_datadir(datadir: impl AsRef<Path>) -> Self {
        let datadir = datadir.as_ref();
        Self {
            db_dir: datadir.join("db"),
            db_args: Default::default(),
            static_files_dir: datadir.join("static_files"),
            rocksdb_dir: datadir.join("rocksdb"),
            watch: true,
        }
    }

    /// Disables long-lived read transaction safety guarantees.
    ///
    /// Caution: Keeping database transaction open indefinitely can cause the free list to grow if
    /// changes to the database are made.
    pub const fn disable_long_read_transaction_safety(mut self) -> Self {
        self.db_args.max_read_transaction_duration(Some(MaxReadTransactionDuration::Unbounded));
        self
    }

    /// Derives the [`ReadOnlyConfig`] from the database dir.
    ///
    /// By default this assumes the following datadir layout:
    ///
    /// ```text
    ///    - db
    ///    - rocksdb
    ///    - static_files
    /// ```
    ///
    /// # Panics
    ///
    /// If the path does not exist
    pub fn from_db_dir(db_dir: impl AsRef<Path>) -> Self {
        let db_dir = db_dir.as_ref();
        let datadir = std::fs::canonicalize(db_dir).unwrap().parent().unwrap().to_path_buf();
        let static_files_dir = datadir.join("static_files");
        let rocksdb_dir = datadir.join("rocksdb");
        Self::from_dirs(db_dir, static_files_dir, rocksdb_dir)
    }

    /// Creates the config for the given paths.
    ///
    /// By default this watches the static files directory for changes, see also
    /// [`ProviderFactory::with_read_only_sync`]
    pub fn from_dirs(
        db_dir: impl AsRef<Path>,
        static_files_dir: impl AsRef<Path>,
        rocksdb_dir: impl AsRef<Path>,
    ) -> Self {
        Self {
            db_dir: db_dir.as_ref().into(),
            db_args: Default::default(),
            static_files_dir: static_files_dir.as_ref().into(),
            rocksdb_dir: rocksdb_dir.as_ref().into(),
            watch: true,
        }
    }

    /// Configures the db arguments used when opening the database.
    pub fn with_db_args(mut self, db_args: impl Into<DatabaseArguments>) -> Self {
        self.db_args = db_args.into();
        self
    }

    /// Configures the db directory.
    pub fn with_db_dir(mut self, db_dir: impl Into<PathBuf>) -> Self {
        self.db_dir = db_dir.into();
        self
    }

    /// Configures the static file directory.
    pub fn with_static_file_dir(mut self, static_file_dir: impl Into<PathBuf>) -> Self {
        self.static_files_dir = static_file_dir.into();
        self
    }

    /// Don't watch the static files directory for changes.
    ///
    /// This is only recommended if this is used without a running node instance that modifies
    /// the database.
    pub const fn no_watch(mut self) -> Self {
        self.watch = false;
        self
    }
}

impl<T> From<T> for ReadOnlyConfig
where
    T: AsRef<Path>,
{
    fn from(value: T) -> Self {
        Self::from_datadir(value.as_ref())
    }
}
